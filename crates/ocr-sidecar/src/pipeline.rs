//! Page analysis behind a seam: [`Analyzer`] is what the protocol loop drives.
//! [`StubAnalyzer`] is the test double. [`OarAnalyzer`] is the real fast-mode
//! pipeline (hayro render + text layer, oar-ocr layout / OCR / SLANet+ tables).
//!
//! Fast-mode limits that hold the 4 GB promise: input long side
//! capped at `max_side`, text detection `Max det_limit`, ORT memory pattern off,
//! one process per job. There is no wall-clock watchdog here: the parent and
//! its Job Object enforce every limit.

use crate::table::grid_from_tokens;
use crate::text_layer::{extract_page, use_text_layer};
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::{PixmapSettings, RenderCache, RenderSettings};
use image::{ImageFormat, RgbImage};
use infield_ocr_protocol::{
    encode_rgb_crop, page_crop_name, region_crop_name, wants_rgb_crop, Analyze, Block, BlockKind, Cell, Done, ErrorCode, Line,
    FastModels, PageResult, PageSource, TableCells, MAX_RGB_CROP_BYTES, MIN_VLM_REGIONS, RGB_CROP_HEADER_LEN,
};
use oar_ocr::core::config::OrtSessionConfig;
use oar_ocr::domain::tasks::{LayoutDetectionConfig, TextDetectionConfig, TextRecognitionConfig};
use oar_ocr::domain::structure::{LayoutElement, LayoutElementType, TableResult};
use oar_ocr::oarocr::{OARStructure, OARStructureBuilder};
use oar_ocr::processors::LimitType;
use std::collections::HashSet;
use std::path::Path;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Hard cap on a rendered page's long side, whatever the request says.
const MAX_RENDER_SIDE: f32 = 8000.0;
/// Largest input file read into memory.
const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;
/// Largest image allocation the decoder may make.
const MAX_IMAGE_ALLOC: u64 = 400 * 1024 * 1024;
/// Largest accepted image, checked from the header before any decode, whatever `max_side` is.
const MAX_IMAGE_SIDE: u32 = 20_000;
const MAX_IMAGE_PIXELS: u64 = 120_000_000;

/// A failure that ends the whole job. `message` never contains document text.
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeError {
    pub code: ErrorCode,
    pub message: String,
}

impl AnalyzeError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn cancelled() -> Self {
        Self::new(ErrorCode::Cancelled, "cancelled")
    }
}

pub type Cancelled<'a> = &'a mut dyn FnMut() -> bool;
pub type Sink<'a> = &'a mut dyn FnMut(PageResult) -> Result<(), AnalyzeError>;

pub trait Analyzer {
    /// Run one `Analyze` request. Call `sink` once per page, in order. Poll
    /// `cancelled` between pages and return [`AnalyzeError::cancelled`] when it
    /// answers `true`.
    fn analyze(&mut self, req: &Analyze, cancelled: Cancelled, sink: Sink) -> Result<Done, AnalyzeError>;
}

// ---- stub -------------------------------------------------------------------------------------

/// Test double: a document of `pages` pages that yields one text block per page.
pub struct StubAnalyzer {
    pub pages: u32,
    /// Pages that come back as `Failed { render }`.
    pub fail_pages: Vec<u32>,
    /// After emitting this many pages, wait (up to 10 s) for a cancel.
    pub hold_after: Option<u32>,
    /// `(blocks, chars per block)` for every page, to exercise big frames.
    pub dense: Option<(usize, usize)>,
}

impl StubAnalyzer {
    pub fn new(pages: u32) -> Self {
        Self { pages, fail_pages: Vec::new(), hold_after: None, dense: None }
    }
}

impl Analyzer for StubAnalyzer {
    fn analyze(&mut self, req: &Analyze, cancelled: Cancelled, sink: Sink) -> Result<Done, AnalyzeError> {
        let first = req.first_page.max(1);
        let last = req.last_page.unwrap_or(self.pages).min(self.pages);
        if first > self.pages {
            return Err(AnalyzeError::new(ErrorCode::BadInput, "first page is past the end"));
        }
        let (mut emitted, mut failed) = (0u32, Vec::new());
        for page in first..=last {
            if self.hold_after == Some(emitted) {
                let start = Instant::now();
                while !cancelled() {
                    if start.elapsed() > Duration::from_secs(10) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if cancelled() {
                return Err(AnalyzeError::cancelled());
            }
            let ok = !self.fail_pages.contains(&page);
            let blocks = if ok {
                let (count, len) = self.dense.unwrap_or((1, 0));
                (0..count as u32)
                    .map(|id| Block {
                        id,
                        kind: BlockKind::Text,
                        bbox: [0.0, 0.0, 100.0, 20.0],
                        order: Some(id + 1),
                        text: Some(if len == 0 { format!("stub page {page}") } else { "x".repeat(len) }),
                        cells: None,
                        confidence: 1.0,
                        crop_file: None,
                        model: None,
                        vlm_fallback: false,
                        lines: Vec::new(),
                    })
                    .collect()
            } else {
                failed.push(page);
                Vec::new()
            };
            let source = if ok { PageSource::Ocr } else { PageSource::Failed { code: ErrorCode::Render } };
            sink(PageResult {
                page,
                width: 100,
                height: 100,
                source,
                blocks,
                render_ms: 0,
                analyze_ms: 0,
                peak_working_set_mb: 0,
                doc_page_count: self.pages,
            })?;
            emitted += 1;
        }
        Ok(Done { pages: emitted, failed_pages: failed })
    }
}

// ---- real pipeline ----------------------------------------------------------------------------

/// The fast-mode analyzer. Models load lazily, on the first page that needs OCR,
/// so a born-digital PDF never touches them.
#[derive(Default)]
pub struct OarAnalyzer;

fn ms(t: Instant) -> u32 {
    t.elapsed().as_millis().min(u32::MAX as u128) as u32
}

fn model_error(msg: &str) -> AnalyzeError {
    AnalyzeError::new(ErrorCode::ModelLoad, msg)
}

fn check_models(m: &FastModels) -> Result<(), AnalyzeError> {
    let required = [&m.layout, &m.det, &m.rec, &m.rec_dict];
    let optional = [&m.table, &m.table_dict, &m.orientation];
    for p in required {
        if !p.is_file() {
            return Err(model_error("a required model file is missing"));
        }
    }
    for p in optional.into_iter().flatten() {
        if !p.is_file() {
            return Err(model_error("an optional model file is missing"));
        }
    }
    Ok(())
}

fn build_structure(req: &Analyze) -> Result<OARStructure, AnalyzeError> {
    let m = &req.models;
    check_models(m)?;
    let mut ort = OrtSessionConfig::default().with_memory_pattern(false);
    if req.threads > 0 {
        ort = ort.with_intra_threads(req.threads as usize);
    }
    let mut layout_cfg = LayoutDetectionConfig::with_pp_structurev3_defaults();
    if m.table.is_none() || m.table_dict.is_none() {
        // Without table models oar-ocr fails the whole page when layout finds a table. A
        // threshold above any score drops the class: the table's text is then recognized as
        // ordinary text (unmatched OCR regions become text elements).
        layout_cfg.class_thresholds.get_or_insert_with(Default::default).insert("table".to_string(), 1.01);
    }
    let mut b = OARStructureBuilder::new(m.layout.clone())
        .layout_model_name("PP-DocLayout_plus-L")
        .layout_detection_config(layout_cfg)
        .ort_session(ort)
        .with_ocr(m.det.clone(), m.rec.clone(), m.rec_dict.clone())
        .text_detection_model_name("PP-OCRv5_mobile_det")
        .text_recognition_model_name("PP-OCRv5_mobile_rec")
        .text_detection_config(TextDetectionConfig {
            score_threshold: 0.3,
            box_threshold: 0.6,
            unclip_ratio: 1.5,
            max_candidates: 1000,
            limit_side_len: Some(req.det_limit),
            limit_type: Some(LimitType::Max),
            max_side_len: None,
        })
        .text_recognition_config(TextRecognitionConfig { score_threshold: 0.0 });
    if let (Some(table), Some(dict)) = (&m.table, &m.table_dict) {
        b = b
            .with_wireless_table_structure(table.clone())
            .wireless_table_structure_model_name("SLANet_plus")
            .table_structure_dict_path(dict.clone())
            .use_e2e_wireless_table_rec(true);
    }
    if let Some(o) = &m.orientation {
        b = b.with_document_orientation(o.clone());
    }
    b.build().map_err(|_| model_error("the models could not be loaded"))
}

fn map_kind(t: &LayoutElementType) -> BlockKind {
    use LayoutElementType as T;
    match t {
        T::DocTitle => BlockKind::DocTitle,
        T::ParagraphTitle => BlockKind::ParagraphTitle,
        T::Text | T::Content | T::Abstract | T::Footnote | T::Reference | T::ReferenceContent | T::Algorithm
        | T::AsideText => BlockKind::Text,
        T::List => BlockKind::List,
        T::Image | T::HeaderImage | T::FooterImage => BlockKind::Figure,
        T::Table => BlockKind::Table,
        T::Chart => BlockKind::Chart,
        T::Formula => BlockKind::Formula,
        T::FigureTitle | T::TableTitle | T::ChartTitle | T::FigureTableChartTitle => BlockKind::Caption,
        T::Header => BlockKind::Header,
        T::Footer => BlockKind::Footer,
        T::Number => BlockKind::PageNumber,
        T::Seal => BlockKind::Seal,
        T::FormulaNumber | T::Region | T::Other => BlockKind::Other,
    }
}

fn iou(a: [f32; 4], b: [f32; 4]) -> f32 {
    let w = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let h = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    let inter = w * h;
    let union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter;
    if union > 0.0 { inter / union } else { 0.0 }
}

/// Non-finite values cannot be serialized and are never valid coordinates: drop them to 0
/// and keep the box inside the page.
pub fn clamp_box(b: [f32; 4], width: f32, height: f32) -> [f32; 4] {
    let f = |v: f32, max: f32| if v.is_finite() { v.clamp(0.0, max) } else { 0.0 };
    [f(b[0], width), f(b[1], height), f(b[2], width), f(b[3], height)]
}

fn bbox_of(e: &LayoutElement, img: &RgbImage) -> [f32; 4] {
    clamp_box(
        [e.bbox.x_min(), e.bbox.y_min(), e.bbox.x_max(), e.bbox.y_max()],
        img.width() as f32,
        img.height() as f32,
    )
}

/// Cells for a table block: from the structure tokens when present, else from
/// cells that already carry grid positions.
fn table_cells(t: &TableResult) -> Option<TableCells> {
    let texts = t.cell_texts.as_deref().unwrap_or(&[]);
    if let Some(grid) = t.structure_tokens.as_deref().and_then(|tok| grid_from_tokens(tok, texts)) {
        return Some(grid);
    }
    let cells: Vec<Cell> = t
        .cells
        .iter()
        .filter_map(|c| {
            Some(Cell {
                row: u16::try_from(c.row?).ok()?,
                col: u16::try_from(c.col?).ok()?,
                row_span: u16::try_from(c.row_span.unwrap_or(1)).ok()?.max(1),
                col_span: u16::try_from(c.col_span.unwrap_or(1)).ok()?.max(1),
                text: c.text.clone().unwrap_or_default(),
            })
        })
        .collect();
    if cells.is_empty() {
        return None;
    }
    let rows = cells.iter().map(|c| c.row + c.row_span).max()?;
    let cols = cells.iter().map(|c| c.col + c.col_span).max()?;
    Some(TableCells { rows, cols, cells })
}

/// The smallest crop side, in pixels, worth writing.
const MIN_CROP_SIDE: u32 = 4;

/// A box as a crop rectangle `(x, y, width, height)` inside `img`, or `None` when it has no usable
/// area. Checked before anything is allocated: a NaN or infinite coordinate, a box that starts
/// outside the page or ends before it starts, and a side under [`MIN_CROP_SIDE`] are all refused
/// (the arithmetic saturates, so no coordinate can overflow or underflow). A box that only
/// sticks out of the page is clamped to it.
fn crop_rect(img: &RgbImage, bbox: [f32; 4]) -> Option<(u32, u32, u32, u32)> {
    // A NaN or infinite coordinate is not a position: refuse the box rather than guess one.
    if !bbox.iter().all(|v| v.is_finite()) {
        return None;
    }
    let (width, height) = (img.width(), img.height());
    let edge = |v: f32, max: u32| (v.max(0.0) as u32).min(max);
    let (x0, y0) = (edge(bbox[0], width), edge(bbox[1], height));
    let (x1, y1) = (edge(bbox[2], width), edge(bbox[3], height));
    if x1 < x0.saturating_add(MIN_CROP_SIDE) || y1 < y0.saturating_add(MIN_CROP_SIDE) {
        return None;
    }
    Some((x0, y0, x1 - x0, y1 - y0))
}

/// Write a figure crop to a temp name, then rename it into `crop_dir`.
fn write_crop(img: &RgbImage, bbox: [f32; 4], dir: &Path, name: &str) -> Option<String> {
    let (x, y, w, h) = crop_rect(img, bbox)?;
    std::fs::create_dir_all(dir).ok()?;
    let crop = image::imageops::crop_imm(img, x, y, w, h).to_image();
    let tmp = dir.join(format!("{name}.tmp"));
    if crop.save_with_format(&tmp, ImageFormat::Png).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    if std::fs::rename(&tmp, dir.join(name)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    Some(name.to_string())
}

/// Most lines kept for one page, in detection order. A page with more is a hostile or broken
/// input; the cap also keeps the page frame far below the protocol's payload cap.
pub const MAX_LINES_PER_PAGE: usize = 2000;
/// Most characters kept for one line.
pub const MAX_LINE_CHARS: usize = 1000;

/// One recognized text line before it is assigned to a block: an axis-aligned box in page pixels,
/// the text and the recognizer's confidence.
#[derive(Clone, Debug, PartialEq)]
pub struct LineCandidate {
    pub bbox: [f32; 4],
    pub text: String,
    pub confidence: f32,
}

/// The recognized lines of a page as candidates. A line's box is the axis-aligned box of its
/// detection polygon (the box as detected, before any layout-guided refinement); a region with no
/// text or no points is skipped.
fn line_candidates(result: &oar_ocr::domain::structure::StructureResult) -> Vec<LineCandidate> {
    result
        .text_regions
        .iter()
        .flatten()
        .filter_map(|r| {
            let text = r.text.as_deref()?;
            let poly = r.dt_poly.as_ref().unwrap_or(&r.bounding_box);
            if poly.points.is_empty() {
                return None;
            }
            Some(LineCandidate {
                bbox: [poly.x_min(), poly.y_min(), poly.x_max(), poly.y_max()],
                text: text.to_string(),
                confidence: r.confidence.unwrap_or(0.0),
            })
        })
        .collect()
}

fn intersection_area(a: [f32; 4], b: [f32; 4]) -> f32 {
    let w = (a[2].min(b[2]) - a[0].max(b[0])).max(0.0);
    let h = (a[3].min(b[3]) - a[1].max(b[1])).max(0.0);
    w * h
}

/// Attach each candidate to the block it overlaps most (the first one on a tie) and drop the
/// rest. Dropped: a line whose box has a non-finite coordinate or no area once clamped to the
/// page, whose text is empty or only whitespace, and one that overlaps no block. A kept line has
/// its box clamped to `width` x `height`, its text cut to [`MAX_LINE_CHARS`] characters and its
/// confidence held in 0..=1. At most [`MAX_LINES_PER_PAGE`] lines are kept, in input order.
pub fn attach_lines(blocks: &mut [Block], candidates: Vec<LineCandidate>, width: f32, height: f32) {
    let mut kept = 0usize;
    for c in candidates {
        if kept >= MAX_LINES_PER_PAGE {
            break;
        }
        if !c.bbox.iter().all(|v| v.is_finite()) {
            continue;
        }
        let text: String = c.text.chars().take(MAX_LINE_CHARS).collect();
        if text.trim().is_empty() {
            continue;
        }
        let bbox = clamp_box(c.bbox, width, height);
        if bbox[2] <= bbox[0] || bbox[3] <= bbox[1] {
            continue;
        }
        let mut best: Option<(usize, f32)> = None;
        for (i, b) in blocks.iter().enumerate() {
            let area = intersection_area(bbox, b.bbox);
            if area > 0.0 && best.is_none_or(|(_, a)| area > a) {
                best = Some((i, area));
            }
        }
        let Some((i, _)) = best else { continue };
        let confidence = if c.confidence.is_finite() { c.confidence.clamp(0.0, 1.0) } else { 0.0 };
        blocks[i].lines.push(Line { bbox, text, confidence });
        kept += 1;
    }
}

fn blocks_from(
    result: &oar_ocr::domain::structure::StructureResult,
    img: &RgbImage,
    req: &Analyze,
    page: u32,
) -> Vec<Block> {
    let mut used: HashSet<usize> = HashSet::new();
    let mut blocks = Vec::with_capacity(result.layout_elements.len());
    for (i, e) in result.layout_elements.iter().enumerate() {
        let kind = map_kind(&e.element_type);
        let bbox = bbox_of(e, img);
        let id = i as u32;
        let (mut text, mut cells, mut crop_file) = (None, None, None);
        match kind {
            BlockKind::Figure | BlockKind::Chart => {
                crop_file = write_crop(img, bbox, &req.crop_dir, &format!("p{page:04}-b{id:03}.png"));
            }
            BlockKind::Table => {
                let best = result
                    .tables
                    .iter()
                    .enumerate()
                    .filter(|(ti, _)| !used.contains(ti))
                    .map(|(ti, t)| (ti, iou(bbox, [t.bbox.x_min(), t.bbox.y_min(), t.bbox.x_max(), t.bbox.y_max()])))
                    .filter(|(_, v)| *v >= 0.5)
                    .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                if let Some((ti, _)) = best {
                    used.insert(ti);
                    let t = &result.tables[ti];
                    cells = table_cells(t);
                    if cells.is_none() {
                        let joined: Vec<&str> =
                            t.cell_texts.iter().flatten().flatten().map(|s| s.as_str()).collect();
                        if !joined.is_empty() {
                            text = Some(joined.join(" "));
                        }
                    }
                }
            }
            _ => text = e.text.clone().filter(|s| !s.trim().is_empty()),
        }
        let confidence = if e.confidence.is_finite() { e.confidence.clamp(0.0, 1.0) } else { 0.0 };
        blocks.push(Block {
            id,
            kind,
            bbox,
            order: e.order_index,
            text,
            cells,
            confidence,
            crop_file,
            model: None,
            vlm_fallback: false,
            lines: Vec::new(),
        });
    }
    // Before the crop pass: its full-page block must not capture the lines of a flyer.
    if req.emit_lines {
        attach_lines(&mut blocks, line_candidates(result), img.width() as f32, img.height() as f32);
    }
    if req.emit_crops {
        add_rgb_crops(&mut blocks, img, req, page);
    }
    blocks
}

/// Write a raw RGB crop (`infield_ocr_protocol::encode_rgb_crop` layout) to a temp name, then
/// rename it into `dir`, so the final name never holds a partial file.
fn write_rgb_crop(img: &RgbImage, bbox: [f32; 4], dir: &Path, name: &str) -> Option<String> {
    let (x, y, width, height) = crop_rect(img, bbox)?;
    // Too large for the protocol's cap: refused here, before the pixels are copied.
    let raw_len = u64::from(width) * u64::from(height) * 3;
    if raw_len + RGB_CROP_HEADER_LEN as u64 > MAX_RGB_CROP_BYTES as u64 {
        return None;
    }
    let crop = image::imageops::crop_imm(img, x, y, width, height).to_image();
    let bytes = encode_rgb_crop(width, height, &crop.into_raw()).ok()?;
    std::fs::create_dir_all(dir).ok()?;
    let tmp = dir.join(format!("{name}.tmp"));
    if std::fs::write(&tmp, &bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    let dest = dir.join(name);
    // A retried page (after a crash, a timeout or a requeue) finds the first attempt's final file
    // already there, and a rename onto an existing file can fail on Windows. Replace it: the new
    // file is the one for this attempt.
    if dest.exists() {
        let _ = std::fs::remove_file(&dest);
    }
    if std::fs::rename(&tmp, &dest).is_err() {
        let _ = std::fs::remove_file(&tmp);
        // The old file could not be removed (still open elsewhere): reuse it when it is exactly
        // this crop, rather than dropping the block back to the fast text.
        return (std::fs::read(&dest).ok().as_deref() == Some(bytes.as_slice())).then(|| name.to_string());
    }
    Some(name.to_string())
}

/// The accurate-reading additions to an OCR page, after the normal fast pass: a raw RGB crop per
/// text-like block (`crop_file`), and, when the page has fewer than [`MIN_VLM_REGIONS`] blocks of
/// the crop list (a flyer, a poster; furniture and figures do not count), one full-page crop block
/// named [`page_crop_name`]. Nothing is dropped: every block keeps its fast text and cells, which
/// are the fallback when the vision model cannot read a region.
pub fn add_rgb_crops(blocks: &mut Vec<Block>, img: &RgbImage, req: &Analyze, page: u32) {
    // Counted before any crop is written, on every block the layout found (running text inside a
    // table included): the app decides page mode on the same count.
    let few_regions = blocks.iter().filter(|b| wants_rgb_crop(b.kind)).count() < MIN_VLM_REGIONS;
    for block in blocks.iter_mut() {
        if wants_rgb_crop(block.kind) && block.crop_file.is_none() {
            block.crop_file = write_rgb_crop(img, block.bbox, &req.crop_dir, &region_crop_name(page, block.id));
        }
    }
    if few_regions {
        let id = blocks.iter().map(|b| b.id + 1).max().unwrap_or(0);
        let bbox = [0.0, 0.0, img.width() as f32, img.height() as f32];
        if let Some(name) = write_rgb_crop(img, bbox, &req.crop_dir, &page_crop_name(page)) {
            blocks.push(Block {
                id,
                kind: BlockKind::Other,
                bbox,
                order: Some(blocks.iter().filter_map(|b| b.order).max().unwrap_or(0) + 1),
                text: None,
                cells: None,
                confidence: 1.0,
                crop_file: Some(name),
                model: None,
                vlm_fallback: false,
                lines: Vec::new(),
            });
        }
    }
}

fn ocr_page(
    structure: &OARStructure,
    img: RgbImage,
    req: &Analyze,
    page: u32,
    render_ms: u32,
) -> PageResult {
    let (width, height) = (img.width(), img.height());
    let t = Instant::now();
    let res = structure.predict_image(img.clone());
    let (source, blocks) = match res {
        Ok(r) => (PageSource::Ocr, blocks_from(&r, &img, req, page)),
        Err(_) => (PageSource::Failed { code: ErrorCode::Analyze }, Vec::new()),
    };
    PageResult {
        page,
        width,
        height,
        source,
        blocks,
        render_ms,
        analyze_ms: ms(t),
        peak_working_set_mb: crate::platform::peak_working_set_mb(),
        doc_page_count: 0, // filled in by `emit`, which knows the document
    }
}

fn failed_page(page: u32, code: ErrorCode, render_ms: u32) -> PageResult {
    PageResult {
        page,
        width: 0,
        height: 0,
        source: PageSource::Failed { code },
        blocks: Vec::new(),
        render_ms,
        analyze_ms: 0,
        peak_working_set_mb: crate::platform::peak_working_set_mb(),
        doc_page_count: 0, // filled in by `emit`, which knows the document
    }
}

fn downscale(img: RgbImage, max_side: u32) -> RgbImage {
    let long = img.width().max(img.height());
    if max_side == 0 || long <= max_side {
        return img;
    }
    let k = max_side as f32 / long as f32;
    image::imageops::resize(
        &img,
        ((img.width() as f32 * k) as u32).max(1),
        ((img.height() as f32 * k) as u32).max(1),
        image::imageops::FilterType::Triangle,
    )
}

fn render_pdf_page<'a>(
    page: &'a hayro::hayro_syntax::page::Page<'a>,
    cache: &RenderCache<'a>,
    req: &Analyze,
) -> Option<RgbImage> {
    let (w, h) = page.render_dimensions();
    let long = w.max(h);
    if long.is_nan() || long <= 0.0 {
        return None;
    }
    let cap = if req.max_side > 0 { (req.max_side as f32).min(MAX_RENDER_SIDE) } else { MAX_RENDER_SIDE };
    let mut scale = req.render_dpi.max(1) as f32 / 72.0;
    if long * scale > cap {
        scale = cap / long;
    }
    let pix = hayro::render(
        page,
        cache,
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            x_scale: scale,
            y_scale: scale,
            bg_color: hayro::vello_cpu::color::palette::css::WHITE,
        },
    );
    let (pw, ph) = (pix.width() as u32, pix.height() as u32);
    let mut rgb = Vec::with_capacity(pw as usize * ph as usize * 3);
    for c in pix.data_as_u8_slice().chunks_exact(4) {
        rgb.extend_from_slice(&c[..3]);
    }
    RgbImage::from_raw(pw, ph, rgb)
}

/// Decode an image file to RGB, upright: the EXIF orientation (a phone photo is stored sideways
/// with a tag saying how to turn it) is applied here, so the page is OCR'd the way it is shown and
/// every box is in the upright image's pixels. The size is checked from the header before any
/// pixel is decoded, whatever `max_side` is.
pub fn decode_image(data: &[u8]) -> Result<RgbImage, AnalyzeError> {
    use image::ImageDecoder;
    let unsupported = |m: &str| AnalyzeError::new(ErrorCode::UnsupportedInput, m.to_string());
    let open = || {
        image::ImageReader::new(std::io::Cursor::new(data))
            .with_guessed_format()
            .map_err(|_| unsupported("the image format is not supported"))
    };
    let (w, h) = open()?
        .into_dimensions()
        .map_err(|_| unsupported("the image could not be decoded"))?;
    if w.max(h) > MAX_IMAGE_SIDE || w as u64 * h as u64 > MAX_IMAGE_PIXELS {
        return Err(unsupported("the image is too large"));
    }
    let mut reader = open()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_IMAGE_ALLOC);
    reader.limits(limits);
    let mut decoder = reader
        .into_decoder()
        .map_err(|_| unsupported("the image could not be decoded"))?;
    // A missing or unreadable tag means upright.
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img =
        image::DynamicImage::from_decoder(decoder).map_err(|_| unsupported("the image could not be decoded"))?;
    img.apply_orientation(orientation);
    Ok(img.to_rgb8())
}

fn read_input(path: &Path) -> Result<Vec<u8>, AnalyzeError> {
    let meta = std::fs::metadata(path).map_err(|_| AnalyzeError::new(ErrorCode::BadInput, "input file not found"))?;
    if !meta.is_file() || meta.len() > MAX_INPUT_BYTES {
        return Err(AnalyzeError::new(ErrorCode::BadInput, "input is not a file or is too large"));
    }
    std::fs::read(path).map_err(|_| AnalyzeError::new(ErrorCode::BadInput, "input file could not be read"))
}

fn page_range(req: &Analyze, total: u32) -> Result<(u32, u32), AnalyzeError> {
    let first = req.first_page.max(1);
    let last = req.last_page.unwrap_or(total).min(total);
    if first > last {
        return Err(AnalyzeError::new(ErrorCode::BadInput, "page range is empty"));
    }
    if req.max_pages > 0 && last - first + 1 > req.max_pages {
        return Err(AnalyzeError::new(ErrorCode::BadInput, "page range exceeds max_pages"));
    }
    Ok((first, last))
}

fn panic_to_failed(page: u32) -> PageResult {
    failed_page(page, ErrorCode::Internal, 0)
}

impl Analyzer for OarAnalyzer {
    fn analyze(&mut self, req: &Analyze, cancelled: Cancelled, sink: Sink) -> Result<Done, AnalyzeError> {
        let is_pdf = req
            .input_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
        let data = read_input(&req.input_path)?;
        let mut structure: Option<OARStructure> = None;
        let mut failed: Vec<u32> = Vec::new();
        let mut emitted = 0u32;
        // Every page result carries the document's real length (set once it is
        // known: the PDF's page count, or 1 for an image).
        let doc_pages = std::cell::Cell::new(0u32);
        let mut emit = |mut r: PageResult, failed: &mut Vec<u32>, sink: Sink| -> Result<(), AnalyzeError> {
            if matches!(r.source, PageSource::Failed { .. }) {
                failed.push(r.page);
            }
            emitted += 1;
            r.doc_page_count = doc_pages.get();
            sink(r)
        };

        if is_pdf {
            // Untrusted input: a parser panic is a coded error, never a dead process.
            let internal = || AnalyzeError::new(ErrorCode::Internal, "the PDF parser failed");
            let pdf = catch_unwind(AssertUnwindSafe(|| Pdf::new(Arc::new(data))))
                .map_err(|_| internal())?
                .map_err(|_| AnalyzeError::new(ErrorCode::UnsupportedInput, "the PDF could not be opened"))?;
            let pages = catch_unwind(AssertUnwindSafe(|| pdf.pages())).map_err(|_| internal())?;
            doc_pages.set(pages.len() as u32);
            let (first, last) = page_range(req, pages.len() as u32)?;
            let cache = RenderCache::new();
            for no in first..=last {
                if cancelled() {
                    return Err(AnalyzeError::cancelled());
                }
                let page = &pages[(no - 1) as usize];
                let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<PageResult, AnalyzeError> {
                    let t = Instant::now();
                    if !req.force_ocr {
                        let tl = extract_page(page);
                        if use_text_layer(&tl) {
                            let blocks = tl
                                .paragraphs
                                .into_iter()
                                .enumerate()
                                .map(|(i, p)| Block {
                                    id: i as u32,
                                    kind: BlockKind::Text,
                                    bbox: clamp_box(p.bbox, tl.width_pt as f32, tl.height_pt as f32),
                                    order: Some(i as u32 + 1),
                                    text: Some(p.text),
                                    cells: None,
                                    confidence: 1.0,
                                    crop_file: None,
                                    model: None,
                                    vlm_fallback: false,
                                    lines: Vec::new(),
                                })
                                .collect();
                            return Ok(PageResult {
                                page: no,
                                width: tl.width_pt,
                                height: tl.height_pt,
                                source: PageSource::TextLayer,
                                blocks,
                                render_ms: 0,
                                analyze_ms: ms(t),
                                peak_working_set_mb: crate::platform::peak_working_set_mb(),
                                doc_page_count: 0, // filled in by `emit`
                            });
                        }
                    }
                    let Some(img) = render_pdf_page(page, &cache, req) else {
                        return Ok(failed_page(no, ErrorCode::Render, ms(t)));
                    };
                    let render_ms = ms(t);
                    if structure.is_none() {
                        structure = Some(build_structure(req)?);
                    }
                    Ok(ocr_page(structure.as_ref().unwrap(), img, req, no, render_ms))
                }));
                match outcome {
                    Ok(Ok(r)) => emit(r, &mut failed, &mut *sink)?,
                    Ok(Err(job)) => return Err(job),
                    Err(_) => emit(panic_to_failed(no), &mut failed, &mut *sink)?,
                }
            }
        } else {
            doc_pages.set(1);
            let (first, _) = page_range(req, 1)?;
            debug_assert_eq!(first, 1);
            if cancelled() {
                return Err(AnalyzeError::cancelled());
            }
            let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<PageResult, AnalyzeError> {
                let t = Instant::now();
                let img = downscale(decode_image(&data)?, req.max_side);
                let render_ms = ms(t);
                if structure.is_none() {
                    structure = Some(build_structure(req)?);
                }
                Ok(ocr_page(structure.as_ref().unwrap(), img, req, 1, render_ms))
            }));
            match outcome {
                Ok(Ok(r)) => emit(r, &mut failed, &mut *sink)?,
                Ok(Err(job)) => return Err(job),
                Err(_) => emit(panic_to_failed(1), &mut failed, &mut *sink)?,
            }
        }
        Ok(Done { pages: emitted, failed_pages: failed })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;
    use infield_ocr_protocol::decode_rgb_crop;

    fn request(dir: &Path) -> Analyze {
        let none = std::path::PathBuf::from("none");
        Analyze {
            job_id: "j".into(),
            input_path: none.clone(),
            first_page: 1,
            last_page: None,
            mode: infield_ocr_protocol::AnalyzeMode::Fast,
            models: FastModels {
                layout: none.clone(),
                det: none.clone(),
                rec: none.clone(),
                rec_dict: none,
                table: None,
                table_dict: None,
                orientation: None,
            },
            threads: 1,
            max_side: 3000,
            det_limit: 1280,
            render_dpi: 200,
            force_ocr: false,
            crop_dir: dir.to_path_buf(),
            max_pages: 0,
            emit_crops: true,
            emit_lines: false,
        }
    }

    fn block(id: u32, kind: BlockKind, bbox: [f32; 4], text: Option<&str>) -> Block {
        Block {
            id,
            kind,
            bbox,
            order: Some(id + 1),
            text: text.map(str::to_string),
            cells: None,
            confidence: 0.9,
            crop_file: None,
            model: None,
            vlm_fallback: false,
            lines: Vec::new(),
        }
    }

    fn page_image() -> RgbImage {
        RgbImage::from_fn(200, 300, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 7]))
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }

    #[test]
    fn emit_crops_writes_a_crop_per_text_like_block_and_keeps_the_fast_text() {
        let dir = tempfile::tempdir().unwrap();
        let kinds = [
            BlockKind::DocTitle,
            BlockKind::ParagraphTitle,
            BlockKind::Text,
            BlockKind::List,
            BlockKind::Table,
            BlockKind::Formula,
            BlockKind::Caption,
            BlockKind::Other,
        ];
        let mut blocks: Vec<Block> = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| block(i as u32, *k, [10.0, 10.0 + 30.0 * i as f32, 150.0, 30.0 + 30.0 * i as f32], Some("fast text")))
            .collect();
        let before = blocks.clone();
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 3);
        assert_eq!(blocks.len(), before.len(), "no extra block when layout found 2 or more regions");
        for (b, was) in blocks.iter().zip(&before) {
            // No text dropped: the fast text is the fallback.
            assert_eq!((&b.text, &b.cells, b.kind, b.bbox, b.order), (&was.text, &was.cells, was.kind, was.bbox, was.order));
            let name = b.crop_file.as_deref().expect("a crop per text-like block");
            assert_eq!(name, format!("p0003-b{:03}.rgb", b.id));
            let bytes = std::fs::read(dir.path().join(name)).unwrap();
            let (w, h, rgb) = decode_rgb_crop(&bytes).unwrap();
            assert_eq!((w, h), (140, 20), "the crop is the block's box");
            assert_eq!(rgb.len(), 140 * 20 * 3);
            // The first pixel is the page pixel at the box's top-left corner.
            let (x0, y0) = (b.bbox[0] as u32, b.bbox[1] as u32);
            assert_eq!(&rgb[..3], &[(x0 % 256) as u8, (y0 % 256) as u8, 7]);
        }
    }

    #[test]
    fn charts_and_seals_keep_png_crops_like_figures() {
        let dir = tempfile::tempdir().unwrap();
        let mut figure = block(0, BlockKind::Figure, [10.0, 10.0, 100.0, 100.0], None);
        figure.crop_file = Some("p0001-b000.png".into());
        let mut chart = block(1, BlockKind::Chart, [10.0, 110.0, 100.0, 200.0], None);
        chart.crop_file = Some("p0001-b001.png".into());
        let seal = block(2, BlockKind::Seal, [110.0, 10.0, 190.0, 90.0], None);
        let header = block(3, BlockKind::Header, [10.0, 210.0, 190.0, 230.0], Some("running head"));
        let mut blocks = vec![figure, chart, seal, header];
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 1);
        assert_eq!(blocks[0].crop_file.as_deref(), Some("p0001-b000.png"));
        assert_eq!(blocks[1].crop_file.as_deref(), Some("p0001-b001.png"));
        assert_eq!(blocks[2].crop_file, None, "a seal gets no raw crop");
        assert_eq!(blocks[3].crop_file, None, "a header keeps its fast text");
        // None of them is in the crop list, so the page is read whole: the page crop is the only raw file.
        assert_eq!(files_in(dir.path()), vec!["p0001-page.rgb".to_string()], "no region crop for any of them");
        assert_eq!(blocks.len(), 5);
    }

    #[test]
    fn fewer_than_two_regions_adds_one_full_page_crop_block() {
        for existing in [0usize, 1] {
            let dir = tempfile::tempdir().unwrap();
            let mut blocks: Vec<Block> =
                (0..existing).map(|i| block(i as u32, BlockKind::Text, [10.0, 10.0, 100.0, 40.0], Some("flyer"))).collect();
            add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 1);
            assert_eq!(blocks.len(), existing + 1);
            let page = blocks.last().unwrap();
            assert_eq!((page.kind, page.bbox), (BlockKind::Other, [0.0, 0.0, 200.0, 300.0]));
            assert_eq!(page.text, None);
            assert_eq!(page.crop_file.as_deref(), Some("p0001-page.rgb"), "the page crop has its own name");
            let bytes = std::fs::read(dir.path().join(page.crop_file.as_deref().unwrap())).unwrap();
            assert_eq!(decode_rgb_crop(&bytes).map(|(w, h, _)| (w, h)), Ok((200, 300)));
            // The block that was there keeps its text.
            assert!(blocks.iter().take(existing).all(|b| b.text.as_deref() == Some("flyer")));
        }
    }

    #[test]
    fn one_crop_list_block_among_furniture_and_figures_still_gets_the_page_crop() {
        let dir = tempfile::tempdir().unwrap();
        let mut figure = block(4, BlockKind::Figure, [10.0, 150.0, 190.0, 250.0], None);
        figure.crop_file = Some("p0001-b004.png".into());
        let mut blocks = vec![
            block(0, BlockKind::Text, [10.0, 40.0, 190.0, 140.0], Some("body")),
            block(1, BlockKind::Header, [10.0, 5.0, 190.0, 20.0], Some("running head")),
            block(2, BlockKind::Footer, [10.0, 280.0, 150.0, 295.0], Some("footer")),
            block(3, BlockKind::PageNumber, [160.0, 280.0, 190.0, 295.0], Some("7")),
            figure,
        ];
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 1);
        assert_eq!(blocks.len(), 6, "one page crop block is added");
        let page = blocks.last().unwrap();
        assert_eq!((page.id, page.kind, page.crop_file.as_deref()), (5, BlockKind::Other, Some("p0001-page.rgb")));
        assert_eq!(blocks[0].crop_file.as_deref(), Some("p0001-b000.rgb"), "the text keeps its own crop");
    }

    #[test]
    fn two_crop_list_blocks_get_no_page_crop() {
        let dir = tempfile::tempdir().unwrap();
        let mut blocks = vec![
            block(0, BlockKind::Text, [10.0, 10.0, 190.0, 60.0], Some("body")),
            block(1, BlockKind::Table, [10.0, 70.0, 190.0, 200.0], Some("a b")),
            block(2, BlockKind::Footer, [10.0, 280.0, 150.0, 295.0], Some("footer")),
        ];
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 1);
        assert_eq!(blocks.len(), 3);
        assert!(!files_in(dir.path()).iter().any(|n| n.ends_with("-page.rgb")));
    }

    #[test]
    fn running_text_inside_a_table_counts_toward_the_two_regions() {
        let dir = tempfile::tempdir().unwrap();
        let mut blocks = vec![
            block(0, BlockKind::Table, [10.0, 10.0, 190.0, 200.0], None),
            block(1, BlockKind::Text, [20.0, 20.0, 100.0, 40.0], Some("a cell")),
        ];
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 1);
        assert_eq!(blocks.len(), 2, "two crop-list blocks: no page crop");
    }

    #[test]
    fn region_crops_use_the_protocol_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut blocks = vec![
            block(0, BlockKind::Text, [10.0, 10.0, 190.0, 60.0], Some("a")),
            block(17, BlockKind::Caption, [10.0, 70.0, 190.0, 100.0], Some("b")),
        ];
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 12);
        for b in &blocks {
            assert_eq!(b.crop_file, Some(infield_ocr_protocol::region_crop_name(12, b.id)));
        }
    }

    #[test]
    fn crop_files_appear_only_after_rename() {
        let dir = tempfile::tempdir().unwrap();
        let mut blocks = vec![
            block(0, BlockKind::Text, [10.0, 10.0, 100.0, 40.0], Some("a")),
            block(1, BlockKind::Text, [10.0, 50.0, 100.0, 80.0], Some("b")),
        ];
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 2);
        let names = files_in(dir.path());
        assert_eq!(names, vec!["p0002-b000.rgb".to_string(), "p0002-b001.rgb".to_string()]);
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "no temp file is left behind");
        // A crop that cannot be written leaves the block without a crop_file, never a partial name.
        let blocked = tempfile::tempdir().unwrap();
        let not_a_dir = blocked.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let mut one = vec![block(0, BlockKind::Text, [10.0, 10.0, 100.0, 40.0], Some("a")), block(1, BlockKind::Text, [10.0, 50.0, 100.0, 80.0], Some("b"))];
        add_rgb_crops(&mut one, &page_image(), &request(&not_a_dir), 1);
        assert!(one.iter().all(|b| b.crop_file.is_none() && b.text.is_some()));
    }

    #[test]
    fn a_retried_page_replaces_the_crops_the_first_attempt_left() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path());
        let first_page = RgbImage::from_pixel(200, 300, Rgb([1, 2, 3]));
        let second_page = RgbImage::from_pixel(200, 300, Rgb([200, 100, 50]));
        let make = || {
            vec![
                block(0, BlockKind::Text, [10.0, 10.0, 100.0, 40.0], Some("a")),
                block(1, BlockKind::Text, [10.0, 50.0, 100.0, 80.0], Some("b")),
            ]
        };
        let mut first = make();
        add_rgb_crops(&mut first, &first_page, &req, 4);
        assert!(first.iter().all(|b| b.crop_file.is_some()));
        // The same page again (the final files are still there): every block gets its crop, and the
        // files hold the second attempt's pixels.
        let mut second = make();
        add_rgb_crops(&mut second, &second_page, &req, 4);
        for b in &second {
            let name = b.crop_file.as_deref().expect("a retried page keeps its crops");
            let bytes = std::fs::read(dir.path().join(name)).unwrap();
            let (_, _, rgb) = decode_rgb_crop(&bytes).unwrap();
            assert_eq!(&rgb[..3], &[200, 100, 50]);
        }
        assert_eq!(files_in(dir.path()).len(), 2, "no temp file is left behind");
    }

    #[test]
    fn a_box_that_is_too_small_gets_no_crop() {
        let dir = tempfile::tempdir().unwrap();
        let mut blocks = vec![block(0, BlockKind::Text, [10.0, 10.0, 12.0, 12.0], Some("dot")), block(1, BlockKind::Text, [10.0, 50.0, 100.0, 80.0], Some("b"))];
        add_rgb_crops(&mut blocks, &page_image(), &request(dir.path()), 1);
        assert_eq!(blocks[0].crop_file, None);
        assert!(blocks[1].crop_file.is_some());
    }

    // ----- crop bounds: checked before anything is allocated

    #[test]
    fn crop_rect_refuses_boxes_without_usable_area_and_never_overflows() {
        let img = page_image(); // 200 x 300
        let refused = [
            [f32::NAN, 0.0, 100.0, 100.0],
            [0.0, f32::NAN, 100.0, 100.0],
            [0.0, 0.0, f32::NAN, 100.0],
            [10.0, 10.0, 10.0, 100.0],       // zero width
            [10.0, 10.0, 100.0, 10.0],       // zero height
            [100.0, 10.0, 50.0, 100.0],      // ends before it starts
            [10.0, 100.0, 100.0, 50.0],
            [10.0, 10.0, 13.0, 100.0],       // 3 px wide: under the minimum side
            [200.0, 10.0, 300.0, 100.0],     // starts at the right edge: outside the page
            [10.0, 300.0, 100.0, 400.0],     // starts at the bottom edge
            [500.0, 500.0, 900.0, 900.0],    // wholly outside
            [-50.0, -50.0, -10.0, -10.0],    // wholly before the page
            [1e10, 0.0, 2e10, 100.0],        // a saturating cast would overflow `x0 + 4`
            [f32::INFINITY, 0.0, f32::INFINITY, 100.0],
            [f32::MAX, f32::MAX, f32::MAX, f32::MAX],
        ];
        for b in refused {
            assert_eq!(crop_rect(&img, b), None, "{b:?}");
        }
        // Boxes that stick out of the page are clamped to it, not refused.
        assert_eq!(crop_rect(&img, [-20.0, -5.0, 1e9, 1e9]), Some((0, 0, 200, 300)));
        assert_eq!(crop_rect(&img, [190.0, 290.0, 230.0, 330.0]), Some((190, 290, 10, 10)));
    }

    #[test]
    fn a_box_without_area_writes_nothing_and_creates_no_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let img = page_image();
        for b in [[f32::NAN, 0.0, 10.0, 10.0], [1e10, 0.0, 2e10, 100.0], [10.0, 10.0, 13.0, 100.0]] {
            let (png_dir, rgb_dir) = (tmp.path().join("png"), tmp.path().join("rgb"));
            assert_eq!(write_crop(&img, b, &png_dir, "a.png"), None);
            assert_eq!(write_rgb_crop(&img, b, &rgb_dir, "a.rgb"), None);
            assert!(!png_dir.exists() && !rgb_dir.exists(), "{b:?}: refused before any file work");
        }
    }

    #[test]
    fn a_crop_over_the_protocol_cap_is_refused_before_the_pixels_are_copied() {
        // 5000 x 5000 x 3 = 75 MB > the 64 MiB cap.
        let big = RgbImage::new(5000, 5000);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(write_rgb_crop(&big, [0.0, 0.0, 5000.0, 5000.0], dir.path(), "p.rgb"), None);
        assert!(files_in(dir.path()).is_empty());
        // The same page at a size that fits is written.
        assert!(write_rgb_crop(&big, [0.0, 0.0, 3000.0, 2000.0], dir.path(), "q.rgb").is_some());
    }

    // ----- emit_crops reaches the pipeline (blocks_from, the same call the analyzer makes)

    fn structure(elements: &[(LayoutElementType, [f32; 4], Option<&str>)]) -> oar_ocr::domain::structure::StructureResult {
        use oar_ocr::processors::BoundingBox;
        let mut result = oar_ocr::domain::structure::StructureResult::new("page", 0);
        for (i, (kind, b, text)) in elements.iter().enumerate() {
            let mut e = LayoutElement::new(BoundingBox::from_coords(b[0], b[1], b[2], b[3]), kind.clone(), 0.9);
            e.text = text.map(str::to_string);
            e.order_index = Some(i as u32 + 1);
            result.layout_elements.push(e);
        }
        result
    }

    fn two_text_regions_and_a_figure() -> oar_ocr::domain::structure::StructureResult {
        structure(&[
            (LayoutElementType::Text, [10.0, 10.0, 150.0, 40.0], Some("first")),
            (LayoutElementType::Text, [10.0, 60.0, 150.0, 90.0], Some("second")),
            (LayoutElementType::Image, [10.0, 110.0, 120.0, 200.0], None),
        ])
    }

    #[test]
    fn emit_crops_off_writes_no_raw_crops_and_names_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut req = request(dir.path());
        req.emit_crops = false;
        let blocks = blocks_from(&two_text_regions_and_a_figure(), &page_image(), &req, 5);
        assert_eq!(blocks.len(), 3, "no extra block");
        let text: Vec<_> = blocks.iter().filter(|b| b.kind == BlockKind::Text).collect();
        assert_eq!(text.len(), 2);
        assert!(text.iter().all(|b| b.crop_file.is_none() && b.text.is_some()));
        // The figure keeps its PNG crop, exactly as before; nothing else was written.
        assert_eq!(blocks[2].crop_file.as_deref(), Some("p0005-b002.png"));
        assert_eq!(files_in(dir.path()), vec!["p0005-b002.png".to_string()]);
    }

    #[test]
    fn emit_crops_on_writes_the_crops_and_the_blocks_name_them() {
        let dir = tempfile::tempdir().unwrap();
        let req = request(dir.path()); // emit_crops: true
        let blocks = blocks_from(&two_text_regions_and_a_figure(), &page_image(), &req, 5);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].crop_file.as_deref(), Some("p0005-b000.rgb"));
        assert_eq!(blocks[1].crop_file.as_deref(), Some("p0005-b001.rgb"));
        assert_eq!(blocks[2].crop_file.as_deref(), Some("p0005-b002.png"), "the figure stays a PNG");
        assert_eq!(
            files_in(dir.path()),
            vec!["p0005-b000.rgb".to_string(), "p0005-b001.rgb".to_string(), "p0005-b002.png".to_string()]
        );
        // Every named file exists and decodes; the fast text is untouched.
        let bytes = std::fs::read(dir.path().join("p0005-b000.rgb")).unwrap();
        assert_eq!(decode_rgb_crop(&bytes).map(|(w, h, _)| (w, h)), Ok((140, 30)));
        assert_eq!(blocks[0].text.as_deref(), Some("first"));
        assert_eq!(blocks[1].text.as_deref(), Some("second"));
    }

    #[test]
    fn emit_crops_off_adds_no_full_page_block_for_a_flyer() {
        let flyer = structure(&[(LayoutElementType::Text, [10.0, 10.0, 150.0, 40.0], Some("flyer"))]);
        let dir = tempfile::tempdir().unwrap();
        let mut off = request(dir.path());
        off.emit_crops = false;
        assert_eq!(blocks_from(&flyer, &page_image(), &off, 1).len(), 1);
        assert!(files_in(dir.path()).is_empty());
        let on = request(dir.path());
        let blocks = blocks_from(&flyer, &page_image(), &on, 1);
        assert_eq!(blocks.len(), 2, "one region: the full-page crop block is added");
        assert_eq!(blocks[1].crop_file.as_deref(), Some("p0001-page.rgb"));
    }

    // ----- the rename-failure fallback in write_rgb_crop

    #[cfg(windows)]
    fn hold_open_without_delete_sharing(path: &Path) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ only: others may read the file but cannot delete, rename over or write it.
        std::fs::OpenOptions::new().read(true).share_mode(1).open(path).unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn an_undeletable_crop_with_the_same_bytes_is_reused() {
        let dir = tempfile::tempdir().unwrap();
        let img = page_image();
        let b = [10.0, 10.0, 100.0, 40.0];
        assert_eq!(write_rgb_crop(&img, b, dir.path(), "p.rgb").as_deref(), Some("p.rgb"));
        let held = hold_open_without_delete_sharing(&dir.path().join("p.rgb"));
        // The retry cannot replace the file (it is open elsewhere), but it holds this very crop.
        assert_eq!(write_rgb_crop(&img, b, dir.path(), "p.rgb").as_deref(), Some("p.rgb"));
        drop(held);
        assert_eq!(files_in(dir.path()), vec!["p.rgb".to_string()], "no temp file is left behind");
    }

    #[cfg(windows)]
    #[test]
    fn an_undeletable_crop_with_different_bytes_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = RgbImage::from_pixel(200, 300, Rgb([1, 2, 3]));
        let second = RgbImage::from_pixel(200, 300, Rgb([9, 9, 9]));
        let b = [10.0, 10.0, 100.0, 40.0];
        assert!(write_rgb_crop(&first, b, dir.path(), "p.rgb").is_some());
        let before = std::fs::read(dir.path().join("p.rgb")).unwrap();
        let held = hold_open_without_delete_sharing(&dir.path().join("p.rgb"));
        // Another page's pixels: the stale file must not be returned as this block's crop.
        assert_eq!(write_rgb_crop(&second, b, dir.path(), "p.rgb"), None);
        drop(held);
        assert_eq!(std::fs::read(dir.path().join("p.rgb")).unwrap(), before, "the old file is left as it was");
        assert_eq!(files_in(dir.path()), vec!["p.rgb".to_string()], "no temp file is left behind");
    }

    #[test]
    fn a_destination_that_cannot_be_replaced_or_read_gives_no_crop() {
        // A directory sits where the file should go: it cannot be removed as a file, nothing can be
        // renamed onto it, and it cannot be read back.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("p.rgb").join("inner")).unwrap();
        assert_eq!(write_rgb_crop(&page_image(), [10.0, 10.0, 100.0, 40.0], dir.path(), "p.rgb"), None);
        assert!(!files_in(dir.path()).iter().any(|n| n.ends_with(".tmp")), "no temp file is left behind");
    }

    // ----- line boxes (`emit_lines`)

    fn cand(bbox: [f32; 4], text: &str) -> LineCandidate {
        LineCandidate { bbox, text: text.to_string(), confidence: 0.8 }
    }

    fn two_blocks() -> Vec<Block> {
        vec![
            block(0, BlockKind::Text, [10.0, 10.0, 150.0, 60.0], Some("top")),
            block(1, BlockKind::Text, [10.0, 100.0, 150.0, 160.0], Some("bottom")),
        ]
    }

    fn texts(b: &Block) -> Vec<&str> {
        b.lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn a_line_goes_to_the_block_it_overlaps_most() {
        let mut blocks = two_blocks();
        attach_lines(
            &mut blocks,
            vec![
                cand([12.0, 12.0, 140.0, 30.0], "in the first"),
                cand([12.0, 110.0, 140.0, 130.0], "in the second"),
                cand([12.0, 35.0, 140.0, 55.0], "also first"),
            ],
            200.0,
            300.0,
        );
        assert_eq!(texts(&blocks[0]), vec!["in the first", "also first"], "kept in detection order");
        assert_eq!(texts(&blocks[1]), vec!["in the second"]);
        assert_eq!(blocks[0].lines[0].bbox, [12.0, 12.0, 140.0, 30.0]);
        assert_eq!(blocks[0].lines[0].confidence, 0.8);
    }

    #[test]
    fn between_overlapping_blocks_the_larger_intersection_wins_and_a_tie_goes_to_the_first() {
        let mut blocks = vec![
            block(0, BlockKind::Text, [0.0, 0.0, 100.0, 100.0], Some("a")),
            block(1, BlockKind::Text, [0.0, 80.0, 100.0, 200.0], Some("b")),
        ];
        attach_lines(
            &mut blocks,
            vec![
                cand([0.0, 75.0, 100.0, 110.0], "25 in a, 30 in b"),
                cand([0.0, 70.0, 100.0, 110.0], "30 in a, 30 in b"),
                cand([0.0, 60.0, 100.0, 110.0], "40 in a, 30 in b"),
            ],
            200.0,
            300.0,
        );
        assert_eq!(texts(&blocks[0]), vec!["30 in a, 30 in b", "40 in a, 30 in b"]);
        assert_eq!(texts(&blocks[1]), vec!["25 in a, 30 in b"]);
    }

    #[test]
    fn a_line_that_touches_no_block_is_dropped() {
        let mut blocks = two_blocks();
        attach_lines(
            &mut blocks,
            vec![cand([12.0, 70.0, 140.0, 90.0], "between"), cand([160.0, 10.0, 190.0, 60.0], "beside")],
            200.0,
            300.0,
        );
        assert!(blocks.iter().all(|b| b.lines.is_empty()));
    }

    #[test]
    fn empty_blank_and_boxless_lines_are_dropped() {
        let mut blocks = two_blocks();
        attach_lines(
            &mut blocks,
            vec![
                cand([12.0, 12.0, 140.0, 30.0], ""),
                cand([12.0, 12.0, 140.0, 30.0], "  \t\n "),
                cand([12.0, 12.0, 12.0, 30.0], "zero width"),
                cand([12.0, 30.0, 140.0, 30.0], "zero height"),
                cand([140.0, 12.0, 12.0, 30.0], "inverted"),
                cand([f32::NAN, 12.0, 140.0, 30.0], "nan"),
                cand([12.0, f32::INFINITY, 140.0, 30.0], "inf"),
                cand([12.0, 12.0, 140.0, 30.0], "kept"),
            ],
            200.0,
            300.0,
        );
        assert_eq!(texts(&blocks[0]), vec!["kept"]);
    }

    #[test]
    fn a_box_sticking_out_of_the_page_is_clamped_to_it() {
        let mut blocks = vec![block(0, BlockKind::Text, [0.0, 0.0, 200.0, 300.0], Some("page"))];
        attach_lines(&mut blocks, vec![cand([-30.0, -5.0, 260.0, 400.0], "wide"), cand([500.0, 500.0, 900.0, 900.0], "far")], 200.0, 300.0);
        assert_eq!(texts(&blocks[0]), vec!["wide"], "a box wholly outside the page has no area once clamped");
        assert_eq!(blocks[0].lines[0].bbox, [0.0, 0.0, 200.0, 300.0]);
    }

    #[test]
    fn confidence_is_kept_inside_zero_to_one() {
        let mut blocks = two_blocks();
        let mut a = cand([12.0, 12.0, 140.0, 30.0], "a");
        a.confidence = f32::NAN;
        let mut b = cand([12.0, 32.0, 140.0, 50.0], "b");
        b.confidence = 7.0;
        attach_lines(&mut blocks, vec![a, b], 200.0, 300.0);
        assert_eq!(blocks[0].lines[0].confidence, 0.0);
        assert_eq!(blocks[0].lines[1].confidence, 1.0);
    }

    #[test]
    fn a_page_keeps_at_most_2000_lines_and_a_line_at_most_1000_characters() {
        let mut blocks = vec![block(0, BlockKind::Text, [0.0, 0.0, 200.0, 300.0], Some("page"))];
        let many: Vec<LineCandidate> = (0..2500).map(|i| cand([1.0, 1.0, 100.0, 20.0], &format!("line {i}"))).collect();
        attach_lines(&mut blocks, many, 200.0, 300.0);
        assert_eq!(blocks[0].lines.len(), MAX_LINES_PER_PAGE);
        assert_eq!(MAX_LINES_PER_PAGE, 2000);
        assert_eq!(blocks[0].lines[0].text, "line 0");
        assert_eq!(blocks[0].lines[1999].text, "line 1999", "the first 2000 in detection order");

        // 1,500 two-byte characters: cut at 1,000 characters, on a character boundary.
        let mut blocks = vec![block(0, BlockKind::Text, [0.0, 0.0, 200.0, 300.0], Some("page"))];
        attach_lines(&mut blocks, vec![cand([1.0, 1.0, 100.0, 20.0], &"é".repeat(1500))], 200.0, 300.0);
        assert_eq!(blocks[0].lines[0].text.chars().count(), MAX_LINE_CHARS);
        assert_eq!(MAX_LINE_CHARS, 1000);
    }

    #[test]
    fn the_cap_counts_only_lines_that_were_kept() {
        let mut blocks = vec![block(0, BlockKind::Text, [0.0, 0.0, 200.0, 300.0], Some("page"))];
        let mut many: Vec<LineCandidate> = (0..3000).map(|_| cand([1.0, 1.0, 100.0, 20.0], "")).collect();
        many.push(cand([1.0, 1.0, 100.0, 20.0], "real"));
        attach_lines(&mut blocks, many, 200.0, 300.0);
        assert_eq!(texts(&blocks[0]), vec!["real"]);
    }

    fn with_regions(
        mut result: oar_ocr::domain::structure::StructureResult,
        regions: &[([f32; 4], &str)],
    ) -> oar_ocr::domain::structure::StructureResult {
        use oar_ocr::domain::TextRegion;
        use oar_ocr::processors::BoundingBox;
        let list = regions
            .iter()
            .map(|(b, t)| {
                let mut r = TextRegion::new(BoundingBox::from_coords(b[0], b[1], b[2], b[3]));
                r.text = Some(std::sync::Arc::from(*t));
                r.confidence = Some(0.9);
                r
            })
            .collect();
        result.text_regions = Some(list);
        result
    }

    #[test]
    fn blocks_from_attaches_the_text_regions_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let result = with_regions(
            two_text_regions_and_a_figure(),
            &[([12.0, 12.0, 140.0, 30.0], "line one"), ([12.0, 62.0, 140.0, 80.0], "line two")],
        );
        let mut off = request(dir.path());
        off.emit_crops = false;
        off.emit_lines = false;
        let without = blocks_from(&result, &page_image(), &off, 1);
        assert!(without.iter().all(|b| b.lines.is_empty()));
        // The flag off equals a page that had no text regions at all, block for block.
        let plain = blocks_from(&two_text_regions_and_a_figure(), &page_image(), &off, 1);
        assert_eq!(without, plain, "flag off: byte-identical blocks");

        let mut on = off.clone();
        on.emit_lines = true;
        let with = blocks_from(&result, &page_image(), &on, 1);
        assert_eq!(texts(&with[0]), vec!["line one"]);
        assert_eq!(texts(&with[1]), vec!["line two"]);
        assert!(with[2].lines.is_empty());
        // Nothing else about the blocks changed.
        let strip = |mut v: Vec<Block>| {
            v.iter_mut().for_each(|b| b.lines.clear());
            v
        };
        assert_eq!(strip(with), plain);
    }

    #[test]
    fn blocks_from_uses_the_detection_polygon_box_and_the_full_page_block_gets_no_lines() {
        use oar_ocr::domain::TextRegion;
        use oar_ocr::processors::{BoundingBox, Point};
        let dir = tempfile::tempdir().unwrap();
        let mut result = structure(&[(LayoutElementType::Text, [10.0, 10.0, 150.0, 40.0], Some("flyer"))]);
        // A slightly rotated line: the polygon's axis-aligned box is what is used, not `bounding_box`.
        let mut r = TextRegion::new(BoundingBox::from_coords(0.0, 0.0, 1.0, 1.0));
        r.dt_poly = Some(BoundingBox::new(vec![
            Point::new(20.0, 14.0),
            Point::new(120.0, 12.0),
            Point::new(121.0, 30.0),
            Point::new(21.0, 32.0),
        ]));
        r.text = Some(std::sync::Arc::from("tilted"));
        r.confidence = Some(0.7);
        result.text_regions = Some(vec![r]);
        let mut req = request(dir.path()); // emit_crops: true adds a full-page block for a flyer
        req.emit_lines = true;
        let blocks = blocks_from(&result, &page_image(), &req, 1);
        assert_eq!(blocks.len(), 2, "the flyer gets its full-page crop block");
        assert_eq!(blocks[0].lines.len(), 1);
        assert_eq!(blocks[0].lines[0].bbox, [20.0, 12.0, 121.0, 32.0]);
        assert_eq!(blocks[0].lines[0].confidence, 0.7);
        assert!(blocks[1].lines.is_empty(), "lines are attached before the full-page block exists");
    }

    // ----- EXIF orientation at decode

    /// A 40 x 20 JPEG, left half red and right half blue, carrying the EXIF orientation `exif`
    /// (spliced in as an APP1 segment right after the SOI marker).
    fn jpeg_with_orientation(exif: Option<u16>) -> Vec<u8> {
        let img = RgbImage::from_fn(40, 20, |x, _| if x < 20 { Rgb([255, 0, 0]) } else { Rgb([0, 0, 255]) });
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95)
            .encode_image(&img)
            .unwrap();
        let Some(o) = exif else { return jpeg };
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0\x08\0\0\0"); // little-endian, first IFD at 8
        tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count
        tiff.extend_from_slice(&o.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]); // value padding
        tiff.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
        let mut seg = b"Exif\0\0".to_vec();
        seg.extend_from_slice(&tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((seg.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&seg);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn red(p: &Rgb<u8>) -> bool {
        p[0] > 200 && p[2] < 80
    }
    fn blue(p: &Rgb<u8>) -> bool {
        p[2] > 200 && p[0] < 80
    }

    #[test]
    fn an_exif_6_photo_is_decoded_upright() {
        // Orientation 6 = rotate 90 clockwise: the 40 x 20 landscape becomes 20 x 40, red half on top.
        let img = decode_image(&jpeg_with_orientation(Some(6))).unwrap();
        assert_eq!((img.width(), img.height()), (20, 40));
        assert!(red(img.get_pixel(10, 5)), "{:?}", img.get_pixel(10, 5));
        assert!(blue(img.get_pixel(10, 35)), "{:?}", img.get_pixel(10, 35));
    }

    #[test]
    fn exif_orientations_8_and_3_and_the_absent_tag() {
        let img = decode_image(&jpeg_with_orientation(Some(8))).unwrap(); // rotate 90 counter-clockwise
        assert_eq!((img.width(), img.height()), (20, 40));
        assert!(blue(img.get_pixel(10, 5)) && red(img.get_pixel(10, 35)));
        let img = decode_image(&jpeg_with_orientation(Some(3))).unwrap(); // upside down
        assert_eq!((img.width(), img.height()), (40, 20));
        assert!(blue(img.get_pixel(5, 10)) && red(img.get_pixel(35, 10)));
        for plain in [None, Some(1)] {
            let img = decode_image(&jpeg_with_orientation(plain)).unwrap();
            assert_eq!((img.width(), img.height()), (40, 20), "no rotation: {plain:?}");
            assert!(red(img.get_pixel(5, 10)) && blue(img.get_pixel(35, 10)));
        }
    }

    #[test]
    fn decode_image_still_refuses_what_it_refused_before() {
        assert_eq!(decode_image(b"not an image").unwrap_err().code, ErrorCode::UnsupportedInput);
        let mut png = Vec::new();
        RgbImage::new(20_001, 1).write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png).unwrap();
        assert_eq!(decode_image(&png).unwrap_err().code, ErrorCode::UnsupportedInput, "over the side cap");
    }
}
