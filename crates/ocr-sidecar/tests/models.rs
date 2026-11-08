//! Model-dependent tests. All `#[ignore]`: run with
//! `OCR_MODELS_DIR=<dir with the fast models>` and `OCR_CORPUS_DIR=<corpus dir>` (a `manifest.json`
//! plus the pages it lists) and `--ignored`. The two PDF tests also need `OCR_BORN_DIGITAL_PDF` and
//! `OCR_SCANNED_PDF`.
//!
//! The corpus pages carry research-only licences and are not part of this repository: nothing here
//! prints or stores document text, only counts and sizes.

use infield_ocr_protocol::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const EXE: &str = env!("CARGO_BIN_EXE_infield-ocr");

/// Corpus peak working set budget in MiB: the measured corpus peak plus 10%
/// (the budget allows at most 1300). Measured with a release build, orientation
/// on, one process per document, 41 documents: max 942 MiB (942 * 1.1 = 1036).
const PEAK_BUDGET_MIB: u32 = 1036;

fn models_dir() -> PathBuf {
    PathBuf::from(std::env::var("OCR_MODELS_DIR").expect("set OCR_MODELS_DIR"))
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(std::env::var("OCR_CORPUS_DIR").expect("set OCR_CORPUS_DIR"))
}

/// A PDF the caller names through an environment variable (the corpus PDFs are not shipped).
fn pdf_from_env(name: &str) -> PathBuf {
    PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("set {name}")))
}

fn fast_models(orientation: bool) -> FastModels {
    let m = models_dir();
    FastModels {
        layout: m.join("pp-doclayout_plus-l.onnx"),
        det: m.join("pp-ocrv5_mobile_det.onnx"),
        rec: m.join("pp-ocrv5_mobile_rec.onnx"),
        rec_dict: m.join("ppocrv5_dict.txt"),
        table: Some(m.join("slanet_plus.onnx")),
        table_dict: Some(m.join("table_structure_dict_ch.txt")),
        orientation: orientation.then(|| m.join("pp-lcnet_x1_0_doc_ori.onnx")),
    }
}

struct Session {
    child: Child,
    out_seq: u32,
    in_seq: u32,
}

impl Session {
    fn start() -> Session {
        let child = Command::new(EXE)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut s = Session { child, out_seq: 0, in_seq: 0 };
        s.send(FrameKind::Hello, &Hello { protocol: PROTOCOL_VERSION, app_version: "test".into() });
        assert_eq!(s.recv().kind, FrameKind::HelloOk);
        s
    }
    fn send<T: serde::Serialize>(&mut self, kind: FrameKind, h: &T) {
        let stdin = self.child.stdin.as_mut().unwrap();
        write_frame(stdin, kind, self.out_seq, &serde_json::to_vec(h).unwrap(), &[]).unwrap();
        self.out_seq += 1;
    }
    fn recv(&mut self) -> Frame {
        let f = read_frame(self.child.stdout.as_mut().unwrap(), self.in_seq).unwrap();
        self.in_seq += 1;
        f
    }
}

struct Run {
    pages: Vec<PageResult>,
    done: Done,
}

/// One process per document, like the app does.
fn run_document(input: &Path, max_pages: u32, orientation: bool, crop_dir: &Path) -> Run {
    run_document_with(input, max_pages, fast_models(orientation), crop_dir)
}

fn run_document_with(input: &Path, max_pages: u32, models: FastModels, crop_dir: &Path) -> Run {
    run_document_full(input, max_pages, models, crop_dir, false)
}

fn run_document_full(input: &Path, max_pages: u32, models: FastModels, crop_dir: &Path, emit_crops: bool) -> Run {
    let mut s = Session::start();
    s.send(
        FrameKind::Analyze,
        &Analyze {
            job_id: "test".into(),
            input_path: input.to_path_buf(),
            first_page: 1,
            last_page: Some(max_pages),
            mode: AnalyzeMode::Fast,
            models,
            threads: 4,
            max_side: 3000,
            det_limit: 1280,
            render_dpi: 200,
            force_ocr: false,
            crop_dir: crop_dir.to_path_buf(),
            max_pages: 0,
            emit_crops,
        },
    );
    let mut pages = Vec::new();
    let done = loop {
        let f = s.recv();
        match f.kind {
            FrameKind::PageResult => pages.push(decode_page_result(&f).unwrap()),
            FrameKind::Done => break serde_json::from_slice(&f.header).unwrap(),
            FrameKind::Error => {
                let e: ErrorFrame = serde_json::from_slice(&f.header).unwrap();
                panic!("sidecar error {:?}: {}", e.code, e.message);
            }
            k => panic!("unexpected frame {k:?}"),
        }
    };
    s.send(FrameKind::Shutdown, &serde_json::json!({}));
    s.child.stdin.take();
    let _ = s.child.wait();
    Run { pages, done }
}

fn manifest() -> Vec<serde_json::Value> {
    let p = corpus_dir().join("manifest.json");
    serde_json::from_slice(&std::fs::read(p).expect("corpus manifest")).unwrap()
}

#[test]
#[ignore = "needs OCR_MODELS_DIR and the corpus"]
fn e2e_tables_populate_cells() {
    let tmp = tempfile::tempdir().unwrap();
    let mut tables = 0;
    for e in manifest().iter().filter(|e| e["has_table"] == true) {
        let file = corpus_dir().join(e["file"].as_str().unwrap());
        let run = run_document(&file, 1, true, tmp.path());
        for b in run.pages.iter().flat_map(|p| &p.blocks).filter(|b| b.kind == BlockKind::Table) {
            let g = b.cells.as_ref().unwrap_or_else(|| panic!("{}: a table block has no cells", e["id"]));
            assert!(!g.cells.is_empty() && g.rows >= 1 && g.cols >= 1, "{}: empty grid", e["id"]);
            assert!(g.cells.iter().all(|c| c.row < g.rows && c.col < g.cols && c.row_span >= 1 && c.col_span >= 1));
            tables += 1;
        }
    }
    assert!(tables >= 3, "only {tables} tables with cells across the table pages");
}

#[test]
#[ignore = "needs OCR_MODELS_DIR, OCR_BORN_DIGITAL_PDF and OCR_SCANNED_PDF"]
fn born_digital_pdf_uses_the_text_layer_and_scans_do_not() {
    let tmp = tempfile::tempdir().unwrap();
    let digital = run_document(&pdf_from_env("OCR_BORN_DIGITAL_PDF"), 3, true, tmp.path());
    assert!(!digital.pages.is_empty());
    // Body pages take the text layer. A cover page whose pieces sit side by side is flagged
    // ambiguous and goes through OCR (layout analysis) instead.
    assert!(digital.pages.iter().filter(|p| p.page >= 2).all(|p| p.source == PageSource::TextLayer));
    assert!(digital.pages.iter().filter(|p| p.source == PageSource::TextLayer).count() >= 2);
    let scan = run_document(&pdf_from_env("OCR_SCANNED_PDF"), 3, true, tmp.path());
    assert!(scan.pages.iter().all(|p| p.source == PageSource::Ocr));
}

#[test]
#[ignore = "needs OCR_MODELS_DIR and the corpus"]
fn figure_crops_are_renamed_into_place() {
    let tmp = tempfile::tempdir().unwrap();
    let mut crops = 0;
    for e in manifest().iter().take(12) {
        let file = corpus_dir().join(e["file"].as_str().unwrap());
        let run = run_document(&file, 1, false, tmp.path());
        for b in run.pages.iter().flat_map(|p| &p.blocks) {
            match (&b.kind, &b.crop_file) {
                (BlockKind::Figure | BlockKind::Chart, Some(name)) => {
                    assert!(tmp.path().join(name).is_file());
                    crops += 1;
                }
                (BlockKind::Figure | BlockKind::Chart, None) => {}
                (_, c) => assert!(c.is_none()),
            }
        }
    }
    assert!(crops > 0, "no figure crop was written");

    // With orientation on, boxes of a rotated page stay inside the image that was analyzed.
    let page = manifest().iter().find(|e| e["has_table"] == true).map(|e| corpus_dir().join(e["file"].as_str().unwrap())).unwrap();
    let rotated = image::open(&page).unwrap().rotate180();
    let rpath = tmp.path().join("rotated.png");
    rotated.save(&rpath).unwrap();
    let (w, h) = (rotated.width().min(3000) as f32, rotated.height().min(3000) as f32);
    let run = run_document(&rpath, 1, true, tmp.path());
    for p in &run.pages {
        assert_eq!((p.width as f32 <= w + 1.0, p.height as f32 <= h + 1.0), (true, true));
        for b in &p.blocks {
            assert!(b.bbox[0] <= b.bbox[2] && b.bbox[1] <= b.bbox[3], "inverted box");
            assert!(b.bbox[2] <= p.width as f32 && b.bbox[3] <= p.height as f32, "box outside the page");
        }
    }
    let leftovers = std::fs::read_dir(tmp.path()).unwrap().flatten().filter(|d| d.path().extension().is_some_and(|x| x == "tmp")).count();
    assert_eq!(leftovers, 0);
}

/// Memory budget: one process per document, orientation on. Prints the per-document peak
/// and the maximum; set `OCR_PEAK_REPORT=<file>` to also write them as JSON.
#[test]
#[ignore = "needs OCR_MODELS_DIR and the corpus; measure on an otherwise quiet machine"]
fn corpus_peak_is_within_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let mut rows = Vec::new();
    let mut worst = 0u32;
    for e in manifest() {
        let file = corpus_dir().join(e["file"].as_str().unwrap());
        let run = run_document(&file, 3, true, tmp.path());
        assert!(run.done.failed_pages.is_empty(), "{}: pages failed", e["id"]);
        let peak = run.pages.iter().map(|p| p.peak_working_set_mb).max().unwrap_or(0);
        println!("peak {peak:5} MiB  {}", e["id"]);
        worst = worst.max(peak);
        rows.push(serde_json::json!({"id": e["id"], "peak_mib": peak}));
    }
    println!("corpus max peak working set: {worst} MiB (budget {PEAK_BUDGET_MIB})");
    if let Ok(p) = std::env::var("OCR_PEAK_REPORT") {
        std::fs::write(p, serde_json::to_vec_pretty(&serde_json::json!({"max_mib": worst, "documents": rows})).unwrap()).unwrap();
    }
    assert!(worst <= PEAK_BUDGET_MIB, "peak {worst} MiB is over the {PEAK_BUDGET_MIB} MiB budget");
}

/// `emit_crops` runs the same fast pass: the same boxes, kinds, order and text on every block,
/// and one raw RGB crop file per text-like block.
#[test]
#[ignore = "needs OCR_MODELS_DIR and the corpus"]
fn emit_crops_boxes_equal_the_fast_boxes_on_5_pages() {
    let fast_dir = tempfile::tempdir().unwrap();
    let crop_dir = tempfile::tempdir().unwrap();
    let mut pages_checked = 0;
    for e in manifest().iter().take(5) {
        let file = corpus_dir().join(e["file"].as_str().unwrap());
        let fast = run_document_full(&file, 1, fast_models(true), fast_dir.path(), false);
        let crops = run_document_full(&file, 1, fast_models(true), crop_dir.path(), true);
        assert_eq!(fast.pages.len(), crops.pages.len(), "{}", e["id"]);
        for (f, c) in fast.pages.iter().zip(&crops.pages) {
            // The only extra block is the full-page crop, added when layout found fewer than 2 regions.
            let extra = if f.blocks.len() < 2 { 1 } else { 0 };
            assert_eq!(c.blocks.len(), f.blocks.len() + extra, "{}", e["id"]);
            for (fb, cb) in f.blocks.iter().zip(&c.blocks) {
                assert_eq!((fb.id, fb.kind, fb.bbox, fb.order), (cb.id, cb.kind, cb.bbox, cb.order), "{}", e["id"]);
                assert_eq!((&fb.text, &fb.cells), (&cb.text, &cb.cells), "{}: no text is dropped", e["id"]);
                if matches!(cb.kind, BlockKind::Figure | BlockKind::Chart | BlockKind::Seal) {
                    assert!(cb.crop_file.as_deref().is_none_or(|n| n.ends_with(".png")), "{}", e["id"]);
                } else if let Some(name) = &cb.crop_file {
                    let bytes = std::fs::read(crop_dir.path().join(name)).unwrap();
                    let (w, h, rgb) = decode_rgb_crop(&bytes).unwrap();
                    assert_eq!(rgb.len(), w as usize * h as usize * 3);
                }
            }
            pages_checked += 1;
        }
    }
    assert_eq!(pages_checked, 5);
    let temps = std::fs::read_dir(crop_dir.path()).unwrap().flatten().filter(|d| d.path().extension().is_some_and(|x| x == "tmp")).count();
    assert_eq!(temps, 0, "crops appear only after the rename");
}

/// The crops add at most 10% to the fast pass's peak working set (the fast budget plus 10%).
#[test]
#[ignore = "needs OCR_MODELS_DIR and the corpus; measure on an otherwise quiet machine"]
fn emit_crops_peak_is_within_the_fast_budget_plus_10_percent() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut fast_worst, mut crops_worst) = (0u32, 0u32);
    for e in manifest().iter().take(12) {
        let file = corpus_dir().join(e["file"].as_str().unwrap());
        let peak = |emit: bool| {
            let run = run_document_full(&file, 1, fast_models(true), tmp.path(), emit);
            run.pages.iter().map(|p| p.peak_working_set_mb).max().unwrap_or(0)
        };
        fast_worst = fast_worst.max(peak(false));
        crops_worst = crops_worst.max(peak(true));
    }
    println!("peak working set: fast {fast_worst} MiB, with crops {crops_worst} MiB (limit {})", PEAK_BUDGET_MIB);
    assert!(crops_worst <= PEAK_BUDGET_MIB, "with crops {crops_worst} MiB is over the {PEAK_BUDGET_MIB} MiB budget");
    assert!(crops_worst as f32 <= fast_worst as f32 * 1.10 + 8.0, "crops add more than 10%: {fast_worst} -> {crops_worst}");
}

/// Without table models a detected table must not fail the page: its text comes back as
/// ordinary text blocks.
#[test]
#[ignore = "needs OCR_MODELS_DIR and the corpus"]
fn table_less_config_keeps_pages_that_contain_tables() {
    let tmp = tempfile::tempdir().unwrap();
    let mut checked = 0;
    for e in manifest().iter().filter(|e| e["has_table"] == true).take(4) {
        let file = corpus_dir().join(e["file"].as_str().unwrap());
        let mut models = fast_models(false);
        models.table = None;
        models.table_dict = None;
        let run = run_document_with(&file, 1, models, tmp.path());
        assert!(run.done.failed_pages.is_empty(), "{}: the page failed without table models", e["id"]);
        let blocks: Vec<_> = run.pages.iter().flat_map(|p| &p.blocks).collect();
        assert!(blocks.iter().all(|b| b.kind != BlockKind::Table), "{}: a table block without models", e["id"]);
        assert!(blocks.iter().any(|b| b.text.as_deref().is_some_and(|t| !t.trim().is_empty())), "{}: no text", e["id"]);
        checked += 1;
    }
    assert!(checked >= 3);
}

/// Prints (numbers only) how the text-layer check classifies each page of the two PDFs.
#[test]
#[ignore = "diagnostic; needs OCR_BORN_DIGITAL_PDF and OCR_SCANNED_PDF"]
fn text_layer_classification_of_corpus_pdfs() {
    for (name, var) in [("born-digital", "OCR_BORN_DIGITAL_PDF"), ("scanned", "OCR_SCANNED_PDF")] {
        let data = std::fs::read(pdf_from_env(var)).unwrap();
        for page in 0..3 {
            let t = infield_ocr::text_layer::inspect_pdf(data.clone(), page).unwrap();
            println!(
                "{name} p{} real={} ambiguous={} paragraphs={} visible={} invisible={}",
                page + 1,
                infield_ocr::text_layer::is_real_text(&t.stats),
                t.ambiguous,
                t.paragraphs.len(),
                t.stats.visible_chars,
                t.stats.invisible_chars
            );
        }
    }
}

/// Counts only: how many pages of every PDF in `OCR_BD_DIR` use the text layer, are flagged
/// ambiguous (go to OCR), or have no real text.
#[test]
#[ignore = "diagnostic; set OCR_BD_DIR to a folder of born-digital PDFs"]
fn text_layer_ambiguity_rate() {
    let dir = PathBuf::from(std::env::var("OCR_BD_DIR").expect("set OCR_BD_DIR"));
    let (mut total, mut used, mut amb, mut none) = (0, 0, 0, 0);
    for e in std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "pdf")) {
        let data = std::fs::read(e.path()).unwrap();
        let (mut n, mut u, mut a, mut z) = (0, 0, 0, 0);
        while let Ok(t) = infield_ocr::text_layer::inspect_pdf(data.clone(), n) {
            if !infield_ocr::text_layer::is_real_text(&t.stats) {
                z += 1;
            } else if t.ambiguous {
                a += 1;
            } else {
                u += 1;
            }
            n += 1;
        }
        println!("{:24} pages {n:3}  text layer {u:3}  ambiguous {a:3}  no real text {z:3}", e.file_name().to_string_lossy());
        total += n;
        used += u;
        amb += a;
        none += z;
    }
    println!("TOTAL pages {total}  text layer {used}  ambiguous {amb}  no real text {none}");
}
