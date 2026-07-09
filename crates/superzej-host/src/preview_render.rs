//! Pure rendering of document models ([`superzej_core::preview`]) into preview
//! lines for the existing Files-preview text path (AF 775).
//!
//! CSV tables and Jupyter notebooks render as aligned text through the same
//! `FilePreview` line list the plain-text route uses — no new render substrate,
//! no graphics. (Image / Mermaid / PDF-page rasters take the graphics path in
//! [`crate::graphics`] instead.) Kept pure + unit-tested here; the host just
//! feeds the resulting lines to the preview pane.

use superzej_core::preview::{CellKind, CsvTable, Notebook};

/// Column separator in the rendered CSV grid.
const SEP: &str = " │ ";

/// Render a [`CsvTable`] as an aligned text grid: header row, an underline, then
/// data rows. Cells wider than their column are truncated with an ellipsis.
pub fn csv_lines(table: &CsvTable) -> Vec<String> {
    if table.rows.is_empty() {
        return vec!["(empty table)".to_string()];
    }
    let widths = &table.col_widths;
    let mut out = Vec::with_capacity(table.rows.len() + 2);

    let render_row = |cells: &[String]| -> String {
        (0..table.cols)
            .map(|c| {
                let cell = cells.get(c).map(String::as_str).unwrap_or("");
                pad_or_truncate(cell, widths.get(c).copied().unwrap_or(0))
            })
            .collect::<Vec<_>>()
            .join(SEP)
    };

    let mut iter = table.rows.iter();
    if let Some(header) = iter.next() {
        out.push(render_row(header));
        // Underline the header, matching the grid layout (─ under cells, ┼ under separators).
        let underline = widths
            .iter()
            .map(|w| "─".repeat(*w))
            .collect::<Vec<_>>()
            .join("─┼─");
        out.push(underline);
    }
    for row in iter {
        out.push(render_row(row));
    }
    if table.truncated {
        out.push(format!("… (truncated at {} rows)", table.rows.len()));
    }
    out
}

/// Pad `s` with spaces to display width `w`, or truncate to `w` with a trailing
/// `…` when it is longer. Width is counted in chars (CSV cells are short).
fn pad_or_truncate(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len == w {
        s.to_string()
    } else if len < w {
        format!("{s}{}", " ".repeat(w - len))
    } else if w == 0 {
        String::new()
    } else {
        let kept: String = s.chars().take(w.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// Render a [`Notebook`] as ordered cells: a header banner per cell, the cell
/// source, and an image-output note for code cells that produced images.
pub fn notebook_lines(nb: &Notebook) -> Vec<String> {
    if nb.cells.is_empty() {
        return vec!["(empty notebook)".to_string()];
    }
    let mut out = Vec::new();
    for (i, cell) in nb.cells.iter().enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        let banner = match cell.kind {
            CellKind::Code => format!("── In [{}] ──────── code", i + 1),
            CellKind::Markdown => format!("── [{}] ─────────── markdown", i + 1),
            CellKind::Raw => format!("── [{}] ─────────── raw", i + 1),
        };
        out.push(banner);
        out.extend(cell.source.split('\n').map(str::to_string));
        if cell.image_outputs > 0 {
            let n = cell.image_outputs;
            let plural = if n == 1 { "" } else { "s" };
            out.push(format!(
                "   [{n} image output{plural} — shown via graphics]"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn csv_renders_aligned_header_underline_and_rows() {
        let t = CsvTable::parse(Path::new("a.csv"), "name,age\nalice,30\nbob,7\n");
        let lines = csv_lines(&t);
        // header, underline, 2 data rows
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "name  │ age");
        assert!(lines[1].contains('┼'), "header underline uses ┼ joins");
        assert_eq!(lines[2], "alice │ 30 ");
        assert_eq!(lines[3], "bob   │ 7  ");
    }

    #[test]
    fn csv_pads_short_cells_to_column_width() {
        let t = CsvTable::parse(Path::new("a.csv"), "hi\nhello world\n");
        // col width = max(2, 11) = 11 → "hi" padded to 11.
        let lines = csv_lines(&t);
        assert_eq!(lines[0], "hi         ");
    }

    #[test]
    fn csv_truncates_cell_wider_than_capped_column() {
        // A cell >MAX_COL_WIDTH (40): the column caps at 40 and the cell is
        // truncated to 39 chars + an ellipsis.
        let long = "x".repeat(60);
        let t = CsvTable::parse(Path::new("a.csv"), &format!("{long}\n"));
        let lines = csv_lines(&t);
        assert_eq!(lines[0].chars().count(), 40);
        assert!(lines[0].ends_with('…'));
    }

    #[test]
    fn csv_empty_is_placeholder() {
        let t = CsvTable::parse(Path::new("a.csv"), "");
        assert_eq!(csv_lines(&t), vec!["(empty table)".to_string()]);
    }

    #[test]
    fn notebook_renders_cells_in_order_with_banners() {
        let nb = Notebook::parse(
            "{\"cells\":[\
             {\"cell_type\":\"markdown\",\"source\":\"# Title\"},\
             {\"cell_type\":\"code\",\"source\":\"print(1)\",\"outputs\":[]}]}",
        )
        .unwrap();
        let lines = notebook_lines(&nb);
        assert!(lines[0].contains("markdown"));
        assert_eq!(lines[1], "# Title");
        assert_eq!(lines[2], ""); // blank between cells
        assert!(lines[3].contains("In [2]") && lines[3].contains("code"));
        assert_eq!(lines[4], "print(1)");
    }

    #[test]
    fn notebook_notes_image_outputs() {
        let nb = Notebook::parse(
            "{\"cells\":[{\"cell_type\":\"code\",\"source\":\"plot()\",\"outputs\":[\
             {\"data\":{\"image/png\":\"x\"}}]}]}",
        )
        .unwrap();
        let lines = notebook_lines(&nb);
        assert!(lines.last().unwrap().contains("1 image output"));
    }

    #[test]
    fn notebook_empty_is_placeholder() {
        let nb = Notebook { cells: vec![] };
        assert_eq!(notebook_lines(&nb), vec!["(empty notebook)".to_string()]);
    }
}
