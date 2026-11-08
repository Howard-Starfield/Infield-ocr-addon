//! PDF text-layer check and extraction (hayro).
//!
//! A page has "real text" when it carries visible, Unicode-mapped, printable
//! glyphs at a meaningful density. Invisible text (render mode 3) marks an old
//! OCR layer on a scan and never counts. Such pages go through OCR instead.
//! The density threshold was checked against a test corpus: born-digital
//! pages far above it, scanned pages (with or without an old OCR layer) at 0.
//!
//! The text layer is accepted only for a plainly single-column page: no vertical
//! cut at any level, and every row one piece apart from at most a running header
//! and footer row. Everything else is flagged `ambiguous` and the caller runs OCR
//! instead (the layout model orders the regions). Multi-column text is never
//! assembled from the text layer; the XY-cut is a detector.

use hayro::hayro_interpret::{
    BlendMode, ClipPath, Context as PdfCtx, DrawMode, DrawProps, Image, ImageDrawProps, InterpreterCache,
    InterpreterSettings, SoftMask, TransformExt, font::GlyphRun, interpret_page,
};
use hayro::hayro_syntax::Pdf;
use hayro::kurbo::{BezPath, Point, Rect};
use std::collections::HashSet;
use std::sync::Arc;

/// Minimum visible non-space characters on a page.
pub const MIN_CHARS: usize = 20;
/// Minimum visible characters per 1000 pt² of page area (about 25 on US Letter).
pub const MIN_CHARS_PER_KPT2: f32 = 0.05;
/// Minimum share of glyphs that map to Unicode, and of mapped chars that print.
pub const MIN_RATIO: f32 = 0.9;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextStats {
    /// Non-space characters from visible glyphs.
    pub visible_chars: usize,
    /// Non-space characters from invisible (render mode 3) glyphs.
    pub invisible_chars: usize,
    /// Visible glyphs without a Unicode mapping.
    pub unmapped_glyphs: usize,
    /// Visible mapped characters that are printable (no controls, U+FFFD or private use).
    pub printable_chars: usize,
    pub area_pt2: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Paragraph {
    pub text: String,
    /// `[x0, y0, x1, y1]` in points, origin at the top-left of the page.
    pub bbox: [f32; 4],
}

#[derive(Clone, Debug)]
pub struct PageText {
    pub paragraphs: Vec<Paragraph>,
    /// The reading order could not be trusted (a table-like or unseparable layout): run OCR instead.
    pub ambiguous: bool,
    pub stats: TextStats,
    pub width_pt: u32,
    pub height_pt: u32,
}

/// The text layer can stand in for OCR: real text whose layout read back unambiguously.
pub fn use_text_layer(page: &PageText) -> bool {
    is_real_text(&page.stats) && !page.ambiguous
}

pub fn is_real_text(s: &TextStats) -> bool {
    if s.visible_chars < MIN_CHARS || s.area_pt2 <= 0.0 {
        return false;
    }
    let density = s.visible_chars as f32 / (s.area_pt2 / 1000.0);
    let mapped = s.visible_chars as f32 / (s.visible_chars + s.unmapped_glyphs) as f32;
    let printable = s.printable_chars as f32 / s.visible_chars as f32;
    density >= MIN_CHARS_PER_KPT2 && mapped >= MIN_RATIO && printable >= MIN_RATIO
}

struct Glyph {
    x: f64,
    y: f64,
    /// Font size in points.
    size: f64,
    /// Horizontal advance in points, when the font reports one.
    advance: Option<f64>,
    text: String,
}

#[derive(Default)]
struct Collector {
    glyphs: Vec<Glyph>,
    invisible_chars: usize,
    unmapped_glyphs: usize,
    /// Glyphs already taken, by position and text. hayro calls `draw_glyph_run` twice for
    /// the same run in fill-and-stroke render modes (2 and 6); stroke-only text (mode 1)
    /// comes once and is kept.
    seen: HashSet<(i64, i64, String, bool)>,
}

impl Collector {
    /// `true` the first time a glyph is seen at this position (0.01 pt resolution). Invisible
    /// and visible glyphs are tracked apart: an invisible glyph never hides a visible one.
    fn first_time(&mut self, x: f64, y: f64, text: &str, invisible: bool) -> bool {
        self.seen.insert(((x * 100.0).round() as i64, (y * 100.0).round() as i64, text.to_string(), invisible))
    }
}

impl<'a> hayro::hayro_interpret::Device<'a> for Collector {
    fn draw_path(&mut self, _: &BezPath, _: DrawProps<'a>, _: &DrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'a>>, _: BlendMode) {}
    fn draw_glyph_run(&mut self, run: &GlyphRun<'_, 'a>, props: DrawProps<'a>, mode: &DrawMode) {
        let invisible = matches!(mode, DrawMode::Invisible);
        for g in run.glyphs() {
            let m = props.transform * g.transform();
            let p = m * Point::new(0.0, 0.0);
            let c = m.as_coeffs();
            // Glyph transforms are per 1000 units of em.
            let size = (c[0] * c[3] - c[1] * c[2]).abs().sqrt() * 1000.0;
            if !(p.x.is_finite() && p.y.is_finite() && size.is_finite()) {
                continue; // a broken transform: no usable position
            }
            let mapped = g.as_unicode().map(|u| match u {
                hayro::hayro_interpret::hayro_cmap::BfString::Char(c) => c.to_string(),
                hayro::hayro_interpret::hayro_cmap::BfString::String(s) => s,
            });
            let Some(s) = mapped.filter(|s| !s.is_empty()) else {
                if !invisible && self.first_time(p.x, p.y, "", false) {
                    self.unmapped_glyphs += 1;
                }
                continue;
            };
            if !self.first_time(p.x, p.y, &s, invisible) {
                continue; // the second callback of a fill-and-stroke run
            }
            if invisible {
                self.invisible_chars += s.chars().filter(|c| !c.is_whitespace()).count();
                continue;
            }
            let advance = match &**g {
                hayro::hayro_interpret::font::Glyph::Outline(o) => o.advance_width(),
                _ => None,
            }
            .map(|a| a as f64 * size / 1000.0);
            self.glyphs.push(Glyph { x: p.x, y: p.y, size, advance, text: s });
        }
    }
    fn draw_image(&mut self, _: Image<'a, '_>, _: ImageDrawProps<'a>) {}
    fn pop_clip(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

fn cmp(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

/// A run of glyphs on one baseline with no wide gap inside.
struct Seg {
    text: String,
    x0: f64,
    x1: f64,
    /// Baseline.
    y: f64,
    size: f64,
}

impl Seg {
    fn top(&self) -> f64 {
        self.y - self.size
    }
}

/// `(word gap, segment break)` between two neighbours on a baseline. Wider than a quarter
/// em beyond the previous glyph's advance (0.85 em between origins when the font reports no
/// advance) is a word gap; wider than one em (1.6 without an advance) ends the segment.
fn gap_kind(prev: &Glyph, g: &Glyph) -> (bool, bool) {
    let gap = g.x - prev.x;
    match prev.advance {
        Some(a) => (gap - a > g.size * 0.25, gap - a > g.size * 1.0),
        None => (gap > g.size * 0.85, gap > g.size * 1.6),
    }
}

/// Group glyphs by baseline, then split each baseline into segments at wide horizontal gaps,
/// so that glyphs of different columns are never joined into one line.
fn segments(mut glyphs: Vec<Glyph>) -> Vec<Seg> {
    glyphs.sort_by(|a, b| cmp(a.y, b.y));
    let mut rows: Vec<Vec<Glyph>> = Vec::new();
    for g in glyphs {
        match rows.last_mut() {
            Some(r) if (r[0].y - g.y).abs() < g.size.max(4.0) * 0.5 => r.push(g),
            _ => rows.push(vec![g]),
        }
    }
    let mut segs = Vec::new();
    for mut r in rows {
        r.sort_by(|a, b| cmp(a.x, b.x));
        let mut cur: Option<(Seg, usize)> = None; // segment, index of its last glyph in r
        for (i, g) in r.iter().enumerate() {
            let (space, split) = match cur.as_ref() {
                Some((_, last)) => gap_kind(&r[*last], g),
                None => (false, false),
            };
            if split || cur.is_none() {
                if let Some((seg, _)) = cur.take() {
                    segs.push(seg);
                }
                cur = Some((Seg { text: String::new(), x0: g.x, x1: g.x, y: g.y, size: g.size }, i));
            }
            let (seg, last) = cur.as_mut().unwrap();
            if space && !g.text.starts_with(' ') && !seg.text.ends_with(' ') {
                seg.text.push(' ');
            }
            seg.text.push_str(&g.text);
            seg.x1 = g.x + g.advance.unwrap_or(g.size * 0.5);
            seg.size = seg.size.max(g.size);
            *last = i;
        }
        segs.extend(cur.map(|(s, _)| s));
    }
    for s in &mut segs {
        s.text = s.text.trim().to_string();
    }
    segs.retain(|s| !s.text.is_empty());
    segs
}

/// Recursion limit of the XY-cut. Real pages nest a handful of cuts; a crafted layout
/// could otherwise overflow the stack (an abort, not a panic). Reaching it marks the page ambiguous.
const MAX_CUT_DEPTH: usize = 32;

/// Detector, not assembler. Looks for a vertical white gutter first, then cuts on the widest
/// horizontal white gaps (top to bottom). A vertical cut means columns or table cells:
/// the page is flagged `ambiguous` and the walk stops, since the page goes to OCR anyway.
/// A region that cannot be cut is one column of lines.
fn xy_cut(segs: Vec<Seg>, gutter: f64, depth: usize, groups: &mut Vec<Vec<Seg>>, ambiguous: &mut bool) {
    if segs.len() < 2 {
        groups.push(segs);
        return;
    }
    if depth >= MAX_CUT_DEPTH {
        *ambiguous = true;
        groups.push(segs);
        return;
    }
    // Vertical cut: a gap in the x projection at least one gutter wide.
    let mut order: Vec<usize> = (0..segs.len()).collect();
    order.sort_by(|a, b| cmp(segs[*a].x0, segs[*b].x0));
    let mut reach = segs[order[0]].x1;
    for &i in &order[1..] {
        if segs[i].x0 - reach >= gutter {
            *ambiguous = true;
            groups.push(segs);
            return;
        }
        reach = reach.max(segs[i].x1);
    }
    // Horizontal cut. Segments are merged as intervals [top, baseline], so a tall one bridges
    // the rows beside it. Only the widest white gaps (at least three quarters of the widest)
    // cut.
    let mut segs = segs;
    segs.sort_by(|a, b| cmp(a.top(), b.top()));
    let mut gaps: Vec<f64> = Vec::new(); // gap before segs[i], or NaN inside a run
    let mut bottom = f64::NEG_INFINITY;
    for (i, s) in segs.iter().enumerate() {
        let gap = s.top() - bottom;
        gaps.push(if i > 0 && gap >= gutter * 0.5 { gap } else { f64::NAN });
        bottom = if i == 0 { s.y } else { bottom.max(s.y) };
    }
    let widest = gaps.iter().copied().filter(|g| g.is_finite()).fold(0.0, f64::max);
    let mut bands: Vec<Vec<Seg>> = vec![Vec::new()];
    for (s, gap) in segs.into_iter().zip(gaps) {
        if gap.is_finite() && gap >= widest * 0.75 {
            bands.push(Vec::new());
        }
        bands.last_mut().unwrap().push(s);
    }
    if bands.len() > 1 {
        for b in bands {
            xy_cut(b, gutter, depth + 1, groups, ambiguous);
        }
        return;
    }
    groups.push(bands.pop().unwrap_or_default());
}

struct Line {
    text: String,
    x0: f64,
    x1: f64,
    y: f64,
    size: f64,
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| cmp(*a, *b));
    v.get(v.len() / 2).copied().unwrap_or(0.0)
}

/// Backstop for a group that holds columns the cut did not see: are its lines two or more
/// clusters by left edge, each with at least two lines, where the left cluster ends before
/// the next begins?
fn has_column_clusters(lines: &[Line], gutter: f64) -> bool {
    let mut order: Vec<&Line> = lines.iter().collect();
    order.sort_by(|a, b| cmp(a.x0, b.x0));
    let mut clusters: Vec<Vec<&Line>> = Vec::new();
    for l in order {
        match clusters.last_mut() {
            Some(c) if l.x0 - c.last().map_or(l.x0, |p| p.x0) < gutter => c.push(l),
            _ => clusters.push(vec![l]),
        }
    }
    let big: Vec<&Vec<&Line>> = clusters.iter().filter(|c| c.len() >= 2).collect();
    big.windows(2).any(|w| {
        let left_end = median(w[0].iter().map(|l| l.x1).collect());
        let right_start = median(w[1].iter().map(|l| l.x0).collect());
        left_end <= right_start
    })
}

/// Lines of one column, then paragraphs by vertical gap.
fn paragraphs_of(mut group: Vec<Seg>, gutter: f64, ambiguous: &mut bool, out: &mut Vec<Paragraph>) {
    group.sort_by(|a, b| cmp(a.y, b.y).then(cmp(a.x0, b.x0)));
    let lines: Vec<Line> =
        group.into_iter().map(|s| Line { text: s.text, x0: s.x0, x1: s.x1, y: s.y, size: s.size }).collect();
    if has_column_clusters(&lines, gutter) {
        *ambiguous = true;
    }
    let mut cur: Option<(Paragraph, f64, f64)> = None; // paragraph, last baseline, last size
    for l in lines {
        let top = (l.y - l.size) as f32;
        match cur.as_mut() {
            Some((p, last_y, last_size)) if l.y - *last_y <= last_size.max(l.size) * 1.8 => {
                p.text.push('\n');
                p.text.push_str(&l.text);
                p.bbox = [p.bbox[0].min(l.x0 as f32), p.bbox[1], p.bbox[2].max(l.x1 as f32), l.y as f32];
                *last_y = l.y;
                *last_size = l.size;
            }
            _ => {
                if let Some((p, _, _)) = cur.take() {
                    out.push(p);
                }
                let p = Paragraph { text: l.text, bbox: [l.x0 as f32, top, l.x1 as f32, l.y as f32] };
                cur = Some((p, l.y, l.size));
            }
        }
    }
    out.extend(cur.map(|(p, _, _)| p));
}

/// A running header or footer: one row of pieces, joined left to right.
fn furniture_paragraph(mut row: Vec<Seg>) -> Paragraph {
    row.sort_by(|a, b| cmp(a.x0, b.x0));
    let (x0, x1) = (row[0].x0, row.iter().fold(f64::NEG_INFINITY, |m, s| m.max(s.x1)));
    let (y, size) = (row[0].y, row.iter().fold(0.0, |m: f64, s| m.max(s.size)));
    let text = row.into_iter().map(|s| s.text).collect::<Vec<_>>().join(" ");
    Paragraph { text, bbox: [x0 as f32, (y - size) as f32, x1 as f32, y as f32] }
}

/// A first or last row with pieces is a running header or footer only when it is detached:
/// at most two pieces; the rest of the page has at least three single-piece lines; and the
/// row lies more than 1.5 median line pitches away from the body. Otherwise it is table
/// evidence (a table or a wrapped cell beside a price), and the page goes to OCR.
fn is_detached_furniture(rows: &[Vec<Seg>], idx: usize) -> bool {
    let row = &rows[idx];
    if row.len() != 2 {
        return false; // one piece is ordinary body text; three or more is a table row
    }
    let body: Vec<&Vec<Seg>> = rows.iter().enumerate().filter(|(i, _)| *i != idx).map(|(_, r)| r).collect();
    if body.iter().filter(|r| r.len() == 1).count() < 3 {
        return false;
    }
    let ys: Vec<f64> = body.iter().map(|r| r[0].y).collect();
    let pitch = median(ys.windows(2).map(|w| (w[1] - w[0]).abs()).collect());
    let nearest = ys.iter().map(|y| (y - row[0].y).abs()).fold(f64::INFINITY, f64::min);
    pitch > 0.0 && nearest > 1.5 * pitch
}

/// Reading-order paragraphs, and whether the page may not be read from its text layer.
///
/// The text layer is accepted only for a plainly single-column page: apart from at most the
/// first and the last row of the page (a running header and footer), every row is one piece
/// (no horizontal gap over about 1 em inside a line), and no vertical white gutter cuts the
/// page at any level. Anything else is left to OCR and its layout model.
fn assemble(glyphs: Vec<Glyph>) -> (Vec<Paragraph>, bool) {
    // Rows by baseline, top to bottom.
    let mut segs = segments(glyphs);
    segs.sort_by(|a, b| cmp(a.y, b.y).then(cmp(a.x0, b.x0)));
    let mut rows: Vec<Vec<Seg>> = Vec::new();
    for s in segs {
        match rows.last_mut() {
            Some(r) if (r[0].y - s.y).abs() < s.size.max(4.0) * 0.5 => r.push(s),
            _ => rows.push(vec![s]),
        }
    }
    // A first or last row may keep its pieces only as a detached running header or footer.
    let (mut header, mut footer) = (None, None);
    let n = rows.len();
    let exempt_footer = n >= 2 && is_detached_furniture(&rows, n - 1);
    let exempt_header = n >= 2 && is_detached_furniture(&rows, 0);
    if exempt_footer {
        footer = rows.pop();
    }
    if exempt_header {
        header = Some(rows.remove(0));
    }
    let mut ambiguous = rows.iter().any(|r| r.len() > 1);
    let body: Vec<Seg> = rows.into_iter().flatten().collect();
    let mut sizes: Vec<f64> = body.iter().map(|s| s.size).collect();
    sizes.sort_by(|a, b| cmp(*a, *b));
    let gutter = sizes.get(sizes.len() / 2).copied().unwrap_or(10.0).max(6.0);
    let mut groups = Vec::new();
    xy_cut(body, gutter, 0, &mut groups, &mut ambiguous);
    let mut out = Vec::new();
    out.extend(header.map(furniture_paragraph));
    for g in groups {
        paragraphs_of(g, gutter, &mut ambiguous, &mut out);
    }
    out.extend(footer.map(furniture_paragraph));
    (out, ambiguous)
}

/// Collect the text layer of one page.
pub fn extract_page(page: &hayro::hayro_syntax::page::Page<'_>) -> PageText {
    let (w, h) = page.render_dimensions();
    let cache = InterpreterCache::new();
    let mut ctx = PdfCtx::new(
        page.initial_transform(true).to_kurbo(),
        Rect::new(0.0, 0.0, w as f64, h as f64),
        &cache,
        page.xref(),
        InterpreterSettings::default(),
    );
    let mut dev = Collector::default();
    interpret_page(page, &mut ctx, &mut dev);
    let mut stats = TextStats {
        invisible_chars: dev.invisible_chars,
        unmapped_glyphs: dev.unmapped_glyphs,
        area_pt2: w * h,
        ..Default::default()
    };
    for g in &dev.glyphs {
        for c in g.text.chars().filter(|c| !c.is_whitespace()) {
            stats.visible_chars += 1;
            let private = matches!(c, '\u{E000}'..='\u{F8FF}');
            if !c.is_control() && c != '\u{FFFD}' && !private {
                stats.printable_chars += 1;
            }
        }
    }
    let (paragraphs, ambiguous) = assemble(dev.glyphs);
    PageText { paragraphs, ambiguous, stats, width_pt: w.round() as u32, height_pt: h.round() as u32 }
}

/// Parse `data` and collect the text layer of page `index` (0-based).
pub fn inspect_pdf(data: Vec<u8>, index: usize) -> Result<PageText, String> {
    let pdf = Pdf::new(Arc::new(data)).map_err(|e| format!("cannot open pdf: {e:?}"))?;
    let pages = pdf.pages();
    let page = pages.get(index).ok_or_else(|| format!("page {index} out of range"))?;
    Ok(extract_page(page))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(x0: f64, x1: f64, y: f64) -> Seg {
        Seg { text: "a line of running text".into(), x0, x1, y, size: 10.0 }
    }

    fn tall(x0: f64, x1: f64, top: f64, bottom: f64) -> Seg {
        Seg { text: "a line of running text".into(), x0, x1, y: bottom, size: bottom - top }
    }

    /// A region that can only be cut vertically: a tall piece on the left next to an inner
    /// region that can only be cut horizontally: a bar across the top over another such
    /// region, and so on. Every level is a nested cut.
    fn v_region(k: usize, ox: f64, oy: f64) -> Vec<Seg> {
        if k == 0 {
            return vec![seg(ox, ox + 30.0, oy)];
        }
        let inner = h_region(k - 1, ox + 50.0, oy);
        let top = inner.iter().map(|s| s.top()).fold(f64::INFINITY, f64::min);
        let bottom = inner.iter().map(|s| s.y).fold(f64::NEG_INFINITY, f64::max);
        let mut v = vec![tall(ox, ox + 20.0, top, bottom)];
        v.extend(inner);
        v
    }

    fn h_region(k: usize, ox: f64, oy: f64) -> Vec<Seg> {
        if k == 0 {
            return vec![seg(ox, ox + 30.0, oy)];
        }
        let mut v = vec![seg(ox, 100_000.0, oy)];
        v.extend(v_region(k - 1, ox, oy + 30.0));
        v
    }

    fn staircase(levels: usize) -> Vec<Seg> {
        v_region(levels, 0.0, 0.0)
    }

    #[test]
    fn a_deep_staircase_stops_at_the_depth_cap_and_is_ambiguous() {
        let (mut groups, mut ambiguous) = (Vec::new(), false);
        let segs = staircase(60);
        let total = segs.len();
        xy_cut(segs, 10.0, 0, &mut groups, &mut ambiguous);
        assert!(ambiguous);
        assert_eq!(groups.iter().map(Vec::len).sum::<usize>(), total, "no segment may be lost");
    }

    #[test]
    fn the_depth_cap_flags_the_page_even_without_a_cut() {
        let segs = vec![seg(0.0, 100.0, 0.0), seg(0.0, 100.0, 30.0)];
        let (mut groups, mut ambiguous) = (Vec::new(), false);
        xy_cut(segs, 10.0, MAX_CUT_DEPTH, &mut groups, &mut ambiguous);
        assert!(ambiguous);
        assert_eq!(groups.len(), 1);
    }
}
