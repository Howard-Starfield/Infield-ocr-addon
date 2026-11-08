//! Table grid from SLANet+ structure tokens.
//!
//! The grid is derived from a fixed token vocabulary (`<tr>`, `</tr>`, `<td>`,
//! `<td></td>`, ` colspan="N"`, ` rowspan="N"`, `>`, `</td>`). Model-produced
//! HTML is never passed through: unknown tokens are ignored and the cell text
//! comes from OCR, one entry per `<td>` in order.

use infield_ocr_protocol::{Cell, TableCells};
use std::collections::HashSet;

/// Spans and grids beyond this are clamped; a real table never needs more.
const MAX_SPAN: u16 = 64;
const MAX_CELLS: usize = 4096;
/// Row / column index limit; further cells are dropped (the table is truncated).
const MAX_GRID: u16 = 1024;
/// Bound on grid slots reserved by row spans.
const MAX_OCCUPIED: usize = 100_000;

fn span_value(token: &str, attr: &str) -> Option<u16> {
    let rest = token.trim().strip_prefix(attr)?.strip_prefix("=\"")?;
    let n: u32 = rest.split('"').next()?.parse().ok()?;
    Some(n.clamp(1, MAX_SPAN as u32) as u16)
}

pub fn grid_from_tokens(tokens: &[String], texts: &[Option<String>]) -> Option<TableCells> {
    let mut cells: Vec<Cell> = Vec::new();
    let mut occupied: HashSet<(u16, u16)> = HashSet::new();
    let (mut row, mut col) = (0u16, 0u16);
    let mut i = 0;
    while i < tokens.len() && cells.len() < MAX_CELLS {
        let t = tokens[i].as_str();
        match t {
            "<tr>" => col = 0,
            "</tr>" => row = row.saturating_add(1),
            _ if t.starts_with("<td") => {
                // Gather attributes up to the closing `>` (or a `<td ...>` token carrying them).
                let (mut rs, mut cs) = (1u16, 1u16);
                let mut j = i;
                loop {
                    let tj = tokens[j].as_str();
                    if let Some(v) = span_value(tj.trim_start_matches("<td"), "rowspan") {
                        rs = v;
                    }
                    if let Some(v) = span_value(tj.trim_start_matches("<td"), "colspan") {
                        cs = v;
                    }
                    if tj.ends_with('>') || j + 1 >= tokens.len() {
                        break;
                    }
                    j += 1;
                }
                while occupied.contains(&(row, col)) {
                    col = col.saturating_add(1);
                }
                if row >= MAX_GRID || col >= MAX_GRID {
                    break;
                }
                let text = texts.get(cells.len()).cloned().flatten().unwrap_or_default();
                cells.push(Cell { row, col, row_span: rs, col_span: cs, text });
                if rs > 1 && occupied.len() + (rs as usize * cs as usize) <= MAX_OCCUPIED {
                    for r in 1..rs {
                        for c in 0..cs {
                            occupied.insert((row.saturating_add(r), col.saturating_add(c)));
                        }
                    }
                }
                col = col.saturating_add(cs);
                i = j;
            }
            _ => {}
        }
        i += 1;
    }
    if cells.is_empty() {
        return None;
    }
    // Indices are below MAX_GRID and spans at most MAX_SPAN, so these cannot overflow.
    let rows = cells.iter().map(|c| c.row + c.row_span).max().unwrap_or(0);
    let cols = cells.iter().map(|c| c.col + c.col_span).max().unwrap_or(0);
    Some(TableCells { rows, cols, cells })
}
