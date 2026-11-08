use infield_ocr::table::grid_from_tokens;

fn toks(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn texts(v: &[&str]) -> Vec<Option<String>> {
    v.iter().map(|s| Some(s.to_string())).collect()
}

#[test]
fn a_plain_grid_is_derived_from_td_tokens() {
    let tokens = toks(&[
        "<tbody>", "<tr>", "<td></td>", "<td></td>", "</tr>", "<tr>", "<td></td>", "<td></td>", "</tr>", "</tbody>",
    ]);
    let g = grid_from_tokens(&tokens, &texts(&["a", "b", "c", "d"])).unwrap();
    assert_eq!((g.rows, g.cols), (2, 2));
    assert_eq!(g.cells.len(), 4);
    assert_eq!((g.cells[3].row, g.cells[3].col, g.cells[3].text.as_str()), (1, 1, "d"));
}

#[test]
fn colspan_and_rowspan_are_honoured() {
    // Row 0: [A (rowspan 2)] [B (colspan 2)]
    // Row 1:                 [C] [D]
    let tokens = toks(&[
        "<tr>", "<td", " rowspan=\"2\"", ">", "</td>", "<td", " colspan=\"2\"", ">", "</td>", "</tr>", "<tr>",
        "<td></td>", "<td></td>", "</tr>",
    ]);
    let g = grid_from_tokens(&tokens, &texts(&["A", "B", "C", "D"])).unwrap();
    assert_eq!((g.rows, g.cols), (2, 3));
    let c = |i: usize| (g.cells[i].row, g.cells[i].col, g.cells[i].row_span, g.cells[i].col_span);
    assert_eq!(c(0), (0, 0, 2, 1));
    assert_eq!(c(1), (0, 1, 1, 2));
    assert_eq!(c(2), (1, 1, 1, 1));
    assert_eq!(c(3), (1, 2, 1, 1));
}

#[test]
fn bare_td_tokens_and_missing_texts_are_tolerated() {
    let tokens = toks(&["<tr>", "<td>", "</td>", "<td>", "</td>", "</tr>"]);
    let g = grid_from_tokens(&tokens, &[Some("x".into())]).unwrap();
    assert_eq!(g.cells.len(), 2);
    assert_eq!(g.cells[0].text, "x");
    assert_eq!(g.cells[1].text, "");
}

#[test]
fn no_cells_means_no_grid_and_unknown_tokens_are_ignored() {
    assert!(grid_from_tokens(&toks(&["<tbody>", "</tbody>"]), &[]).is_none());
    let g = grid_from_tokens(&toks(&["<script>", "<tr>", "<td></td>", "</tr>"]), &texts(&["ok"])).unwrap();
    assert_eq!((g.rows, g.cols, g.cells.len()), (1, 1, 1));
}

#[test]
fn absurd_spans_are_clamped() {
    let tokens = toks(&["<tr>", "<td", " colspan=\"99999\"", ">", "</td>", "</tr>"]);
    let g = grid_from_tokens(&tokens, &texts(&["x"])).unwrap();
    assert!(g.cols <= 64 && g.cells[0].col_span <= 64);
}

#[test]
fn a_huge_table_is_truncated_not_overflowed() {
    let mut tokens = Vec::new();
    for _ in 0..3000 {
        tokens.push("<tr>".to_string());
        tokens.push("<td></td>".to_string());
        tokens.push("</tr>".to_string());
    }
    let g = grid_from_tokens(&tokens, &[]).unwrap();
    assert!(g.rows <= 1024 + 64 && g.cells.len() <= 1024);
    assert!(g.cells.iter().all(|c| c.row < g.rows && c.col < g.cols));
}
