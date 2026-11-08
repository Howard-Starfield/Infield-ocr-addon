mod common;
use common::{make_pdf, text_lines};
use infield_ocr::text_layer::{inspect_pdf, is_real_text};

#[test]
fn a_page_with_visible_text_is_real_text() {
    let content = text_lines(&[
        "The quick brown fox jumps over the lazy dog.",
        "Pack my box with five dozen liquor jugs.",
    ]);
    let page = inspect_pdf(make_pdf(&content, false), 0).unwrap();
    assert!(is_real_text(&page.stats), "{:?}", page.stats);
    assert_eq!(page.paragraphs.len(), 1);
    let text = &page.paragraphs[0].text;
    assert!(text.contains("quick brown fox"), "{text:?}");
    assert!(text.contains("liquor jugs"), "{text:?}");
    assert_eq!((page.width_pt, page.height_pt), (612, 792));
}

#[test]
fn an_image_only_page_is_not_real_text() {
    let content = "q 612 0 0 792 0 0 cm /Im1 Do Q\n";
    let page = inspect_pdf(make_pdf(content, true), 0).unwrap();
    assert!(!is_real_text(&page.stats));
    assert!(page.paragraphs.is_empty());
}

#[test]
fn invisible_render_mode_3_text_is_not_real_text() {
    // A scan with an old OCR layer: the image plus text drawn with `3 Tr`.
    let content = "q 612 0 0 792 0 0 cm /Im1 Do Q\n\
                   BT /F1 12 Tf 3 Tr 72 720 Td (An old ocr layer that must not count as real text.) Tj ET\n";
    let page = inspect_pdf(make_pdf(content, true), 0).unwrap();
    assert!(page.stats.invisible_chars > 20, "{:?}", page.stats);
    assert_eq!(page.stats.visible_chars, 0);
    assert!(!is_real_text(&page.stats));
    assert!(page.paragraphs.is_empty());
}

#[test]
fn a_few_stray_characters_are_not_real_text() {
    let page = inspect_pdf(make_pdf(&text_lines(&["Page 3"]), true), 0).unwrap();
    assert!(!is_real_text(&page.stats), "{:?}", page.stats);
}

#[test]
fn a_gap_between_blocks_starts_a_new_paragraph() {
    let content = "BT /F1 12 Tf 72 720 Td (First paragraph, line one of the text.) Tj 0 -16 Td \
                   (First paragraph, line two of the text.) Tj ET\n\
                   BT /F1 12 Tf 72 600 Td (Second paragraph is far below the first one.) Tj ET\n";
    let page = inspect_pdf(make_pdf(content, false), 0).unwrap();
    assert_eq!(page.paragraphs.len(), 2, "{:?}", page.paragraphs);
    assert!(page.paragraphs[0].text.contains("line two"));
    assert!(page.paragraphs[1].text.starts_with("Second paragraph"));
    // Boxes are top-left origin points, in reading order.
    assert!(page.paragraphs[0].bbox[1] < page.paragraphs[1].bbox[1]);
}

#[test]
fn a_missing_page_is_an_error() {
    assert!(inspect_pdf(make_pdf(&text_lines(&["x"]), false), 3).is_err());
    assert!(inspect_pdf(b"not a pdf".to_vec(), 0).is_err());
}

#[test]
fn non_finite_boxes_are_clamped_into_the_page() {
    use infield_ocr::pipeline::clamp_box;
    let b = clamp_box([f32::NAN, -5.0, f32::INFINITY, 9e9], 100.0, 200.0);
    assert_eq!(b, [0.0, 0.0, 0.0, 200.0]);
    assert!(serde_json::to_string(&b).is_ok());
}

/// A line of text placed with an absolute text matrix.
fn at(x: u32, y: u32, size: u32, text: &str) -> String {
    format!("BT /F1 {size} Tf 1 0 0 1 {x} {y} Tm ({text}) Tj ET
")
}

fn use_text_layer_ok(page: &infield_ocr::text_layer::PageText) -> bool {
    infield_ocr::text_layer::use_text_layer(page)
}

/// The text layer is for plainly single-column pages only: anything else goes to OCR.
fn assert_goes_to_ocr(content: &str, what: &str) {
    let page = inspect_pdf(make_pdf(content, false), 0).unwrap();
    assert!(page.ambiguous, "{what}: must be flagged ambiguous");
    assert!(!use_text_layer_ok(&page), "{what}: must not use the text layer");
}

fn assert_text_layer(content: &str, what: &str) {
    let page = inspect_pdf(make_pdf(content, false), 0).unwrap();
    assert!(!page.ambiguous, "{what}: wrongly flagged ambiguous");
    assert!(use_text_layer_ok(&page), "{what}: should use the text layer");
}

fn columns(rows: u32, pitch: u32) -> String {
    let mut c = String::new();
    for i in 0..rows {
        let y = 700 - i * pitch;
        c.push_str(&at(72, y, 10, &format!("Left column line {i} has enough words to read")));
        c.push_str(&at(330, y, 10, &format!("Right column line {i} has enough words to read")));
    }
    c
}

#[test]
fn two_aligned_columns_go_to_ocr() {
    assert_goes_to_ocr(&columns(8, 14), "aligned columns");
}

#[test]
fn columns_with_a_spanning_title_go_to_ocr_at_any_leading() {
    for pitch in [12, 14, 15, 16, 18, 24] {
        let mut c = at(150, 750, 18, "A title spanning the whole page width");
        c.push_str(&columns(8, pitch));
        assert_goes_to_ocr(&c, &format!("title + columns, pitch {pitch}"));
    }
    // Tight title: the gap under it is no wider than the leading.
    for (title_y, pitch) in [(712u32, 15u32), (716, 14), (710, 24), (730, 16)] {
        let mut c = at(150, title_y, 18, "A title spanning the whole page width");
        for i in 0..8u32 {
            let y = 690 - i * pitch;
            c.push_str(&at(72, y, 10, &format!("Left column line {i} has enough words to read")));
            c.push_str(&at(330, y, 10, &format!("Right column line {i} has enough words to read")));
        }
        assert_goes_to_ocr(&c, &format!("tight title {title_y}, pitch {pitch}"));
    }
}

#[test]
fn alternating_body_gaps_under_a_title_go_to_ocr() {
    // Gaps between rows alternate 8 and 5.5 pt; the gap under the title is 10 pt.
    let mut c = at(150, 710, 18, "A title spanning the whole page width");
    let mut y = 690.0f64;
    for i in 0..8u32 {
        let yy = y.round() as u32;
        c.push_str(&at(72, yy, 10, &format!("Left column line {i} has enough words to read")));
        c.push_str(&at(330, yy, 10, &format!("Right column line {i} has enough words to read")));
        y -= if i % 2 == 0 { 18.0 } else { 15.5 };
    }
    assert_goes_to_ocr(&c, "alternating gaps");
}

#[test]
fn double_spaced_columns_with_unaligned_baselines_go_to_ocr() {
    for offset in [6u32, 7, 9] {
        let mut c = at(150, 750, 18, "A title spanning the whole page width");
        for i in 0..8u32 {
            let y = 700 - i * 24;
            c.push_str(&at(72, y, 10, &format!("Left column line {i} has enough words to read")));
            c.push_str(&at(330, y - offset, 10, &format!("Right column line {i} has enough words to read")));
        }
        assert_goes_to_ocr(&c, &format!("unaligned by {offset} pt"));
    }
}

#[test]
fn columns_cut_into_paragraphs_go_to_ocr() {
    // Three paragraphs of three lines per column, with a gap between paragraphs.
    let mut c = at(150, 750, 18, "A title spanning the whole page width");
    let mut y = 700u32;
    for p in 0..3u32 {
        for l in 0..3u32 {
            c.push_str(&at(72, y, 10, &format!("Left paragraph {p} line {l} has enough words")));
            c.push_str(&at(330, y, 10, &format!("Right paragraph {p} line {l} has enough words")));
            y -= 14;
        }
        y -= 12; // paragraph gap
    }
    assert_goes_to_ocr(&c, "columns with paragraph gaps");
}

#[test]
fn a_table_like_page_goes_to_ocr() {
    // Three aligned columns of short cells.
    let mut c = String::new();
    for i in 0..8u32 {
        let y = 700 - i * 14;
        c.push_str(&at(72, y, 10, &format!("Item {i}")));
        c.push_str(&at(250, y, 10, &format!("Qty {i}")));
        c.push_str(&at(420, y, 10, &format!("Price {i}.00")));
    }
    assert_goes_to_ocr(&c, "three-column table");
}

#[test]
fn short_table_rows_between_prose_go_to_ocr() {
    // A heading, a two-column table of short cells, then ordinary prose in the same column.
    let mut c = at(72, 740, 14, "Quarterly results by region");
    for i in 0..6u32 {
        c.push_str(&at(72, 715 - i * 14, 10, &format!("Region {i}")));
        c.push_str(&at(200, 715 - i * 14, 10, &format!("{}", 100 + i)));
    }
    for l in 0..5u32 {
        c.push_str(&at(72, 620 - l * 14, 10, &format!("Prose line {l} follows the table with some more words")));
    }
    assert_goes_to_ocr(&c, "table rows between prose");
}

#[test]
fn a_plain_single_column_page_with_a_header_and_footer_uses_the_text_layer() {
    let mut c = at(72, 760, 8, "10/18/26, 4:50 PM");
    c.push_str(&at(450, 760, 8, "Sample Book"));
    c.push_str(&at(72, 700, 16, "A chapter heading"));
    for p in 0..3u32 {
        for l in 0..4u32 {
            c.push_str(&at(72, 670 - p * 70 - l * 14, 10, &format!("Paragraph {p} line {l} runs the full width of the text block here")));
        }
    }
    c.push_str(&at(72, 40, 8, "https://example.org/sample"));
    c.push_str(&at(520, 40, 8, "1/9"));
    assert_text_layer(&c, "single column with running header and footer");
}

#[test]
fn fill_and_stroke_text_is_not_doubled() {
    for mode in [2, 6] {
        let content = format!("BT /F1 12 Tf {mode} Tr 72 720 Td (Fill and stroke text must not double.) Tj ET
");
        let page = inspect_pdf(make_pdf(&content, false), 0).unwrap();
        assert_eq!(page.paragraphs.len(), 1, "mode {mode}");
        assert_eq!(page.paragraphs[0].text, "Fill and stroke text must not double.", "mode {mode}");
        assert_eq!(page.stats.visible_chars, 31, "mode {mode}: {:?}", page.stats);
    }
}

#[test]
fn stroke_only_text_is_kept() {
    let content = "BT /F1 12 Tf 1 Tr 72 720 Td (Stroke only outline text stays.) Tj ET
";
    let page = inspect_pdf(make_pdf(content, false), 0).unwrap();
    assert_eq!(page.paragraphs[0].text, "Stroke only outline text stays.");
    assert_eq!(page.stats.visible_chars, 27);
}

#[test]
fn an_invisible_glyph_does_not_hide_a_visible_one_at_the_same_spot() {
    let content = "BT /F1 12 Tf 3 Tr 72 720 Td (Some text that is long enough to count.) Tj ET
                   BT /F1 12 Tf 0 Tr 72 720 Td (Some text that is long enough to count.) Tj ET
";
    let page = inspect_pdf(make_pdf(content, false), 0).unwrap();
    assert_eq!(page.stats.visible_chars, 32, "{:?}", page.stats);
    assert_eq!(page.stats.invisible_chars, 32);
    assert_eq!(page.paragraphs[0].text, "Some text that is long enough to count.");
}

#[test]
fn offset_baseline_columns_with_a_bridging_line_go_to_ocr() {
    // Right column half a line lower than the left, so no row has side-by-side pieces and
    // no white band exists; a long URL spans the gutter, so no vertical cut either.
    for offset in [5u32, 7, 9] {
        let mut c = String::new();
        for i in 0..12u32 {
            let y = 720 - i * 14;
            c.push_str(&at(72, y, 10, &format!("Left column line {i} has enough words to read")));
            c.push_str(&at(330, y - offset, 10, &format!("Right column line {i} has enough words to read")));
        }
        c.push_str(&at(72, 720 - 5 * 14 - 3, 10, "http://example.org/a/very/long/address/that/runs/across/the/whole/text/width/of/the/page/x"));
        assert_goes_to_ocr(&c, &format!("offset {offset} with a bridging line"));
    }
}

/// Prose lines above and below, so the rows under test are neither the first nor the last of the page.
fn with_prose_around(middle: &str, middle_top: u32) -> String {
    let mut c = String::new();
    for l in 0..3u32 {
        c.push_str(&at(72, 740 - l * 14, 12, &format!("Opening prose line {l} that runs the whole width of the text block")));
    }
    c.push_str(middle);
    for l in 0..3u32 {
        c.push_str(&at(72, middle_top - 40 - l * 14, 12, &format!("Closing prose line {l} that runs the whole width of the text block")));
    }
    c
}

#[test]
fn narrow_font_two_cell_rows_on_a_larger_page_go_to_ocr() {
    // 8 pt cells on a 12 pt page; the gap between the cells is over 1 em of the cells' size.
    let mut t = String::new();
    for i in 0..5u32 {
        t.push_str(&at(72, 650 - i * 10, 8, &format!("Item {i}")));
        t.push_str(&at(150, 650 - i * 10, 8, &format!("{}.50", 10 + i)));
    }
    assert_goes_to_ocr(&with_prose_around(&t, 650), "8 pt two-cell rows");
}

#[test]
fn a_sparse_table_with_one_amount_goes_to_ocr() {
    let mut t = at(72, 650, 12, "Jan");
    t.push_str(&at(72, 636, 12, "Feb"));
    t.push_str(&at(300, 636, 12, "120"));
    t.push_str(&at(72, 622, 12, "Mar"));
    assert_goes_to_ocr(&with_prose_around(&t, 622), "sparse table");
}

#[test]
fn a_wrapped_description_beside_one_price_goes_to_ocr() {
    for price_offset in [0u32, 6, 9] {
        let mut t = String::new();
        for l in 0..3u32 {
            t.push_str(&at(72, 650 - l * 14, 12, &format!("Description line {l} of the wrapped item text")));
        }
        t.push_str(&at(420, 636 - price_offset, 12, "$12.00"));
        assert_goes_to_ocr(&with_prose_around(&t, 622), &format!("wrapped cell, price offset {price_offset}"));
    }
}

#[test]
fn a_page_of_one_piece_lines_only_has_no_exceptions_to_make() {
    // Header and footer rows with pieces are allowed, nothing else is.
    let mut c = at(72, 770, 8, "left header");
    c.push_str(&at(480, 770, 8, "right header"));
    for l in 0..6u32 {
        c.push_str(&at(72, 700 - l * 14, 11, &format!("Body line {l} is a single piece that runs across the text block")));
    }
    c.push_str(&at(72, 30, 8, "left footer"));
    c.push_str(&at(480, 30, 8, "right footer"));
    assert_text_layer(&c, "header + body + footer");
}

#[test]
fn a_table_only_page_goes_to_ocr_whatever_its_size() {
    // No prose around: the first and last rows must not be taken for a header and footer.
    for rows in [2u32, 3, 4, 6] {
        let mut c = String::new();
        for i in 0..rows {
            let y = 700 - i * 14;
            c.push_str(&at(72, y, 12, ["Jan", "Feb", "Mar", "Apr", "May", "Jun"][i as usize]));
            c.push_str(&at(300, y, 12, &format!("{}.00", 100 + i)));
        }
        assert_goes_to_ocr(&c, &format!("{rows}-row table only"));
    }
}

#[test]
fn a_table_row_at_the_edge_of_the_page_goes_to_ocr() {
    // A two-piece row right at the top (adjacent to the body), and one right at the bottom.
    let body = |from: u32| -> String {
        (0..4u32)
            .map(|l| at(72, from - l * 14, 12, &format!("Body prose line {l} that runs the whole width of the text")))
            .collect()
    };
    let mut top = at(72, 740, 12, "Total");
    top.push_str(&at(400, 740, 12, "$12.00"));
    top.push_str(&body(726));
    assert_goes_to_ocr(&top, "table row adjacent to the body, at the top");

    let mut bottom = body(740);
    bottom.push_str(&at(72, 684, 12, "Total"));
    bottom.push_str(&at(400, 684, 12, "$12.00"));
    assert_goes_to_ocr(&bottom, "table row adjacent to the body, at the bottom");
}

#[test]
fn a_price_on_the_first_line_of_a_wrapped_description_goes_to_ocr() {
    let mut c = at(72, 700, 12, "Description line 0 of the wrapped item text");
    c.push_str(&at(420, 700, 12, "$12.00"));
    c.push_str(&at(72, 686, 12, "Description line 1 of the wrapped item text"));
    c.push_str(&at(72, 672, 12, "Description line 2 of the wrapped item text"));
    assert_goes_to_ocr(&c, "price on the first row");
}

#[test]
fn a_header_with_three_pieces_is_not_exempt() {
    // Furniture has at most two pieces; a three-piece top row is table-like.
    let mut c = at(72, 770, 8, "left");
    c.push_str(&at(250, 770, 8, "centre"));
    c.push_str(&at(480, 770, 8, "right"));
    for l in 0..6u32 {
        c.push_str(&at(72, 700 - l * 14, 11, &format!("Body line {l} is a single piece that runs across the text block")));
    }
    assert_goes_to_ocr(&c, "three-piece header");
}

#[test]
fn a_detached_running_header_over_prose_keeps_the_text_layer() {
    let mut c = at(72, 770, 8, "Chapter title");
    c.push_str(&at(500, 770, 8, "12"));
    for l in 0..6u32 {
        c.push_str(&at(72, 700 - l * 14, 11, &format!("Body line {l} is a single piece that runs across the text block")));
    }
    assert_text_layer(&c, "detached header over prose");
}
