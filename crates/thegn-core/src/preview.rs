//! Pure content-type routing + document models for the preview pane.
//!
//! The host's preview pane routes a previewed file to a *render route* by this
//! module's [`route_for`]: plain text (tree-sitter, AF 396) and images (the
//! graphics path, AF 399) are the pre-existing routes; CSV, Jupyter, Mermaid,
//! and PDF are the document-viewer additions (AF 775). Everything here is pure
//! (no I/O, no termwiz) so the extension/sniff mapping and the CSV/Jupyter
//! document models are unit-tested in the core-coverage gate; the host owns the
//! off-loop read/rasterize and the actual rendering.

use std::path::Path;

/// The render route a previewed file takes.
///
/// Routing is extension-first with a content sniff only to disambiguate the
/// text/binary fallback: a file with no (or an unknown) extension is `Text` when
/// it looks like UTF-8 text and `Unknown` (unpreviewable) when it looks binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewRoute {
    /// Plain text through the tree-sitter route (AF 396).
    Text,
    /// A raster image through the graphics route (AF 399).
    Image,
    /// A comma/tab-separated table ([`CsvTable`]).
    Csv,
    /// A Jupyter notebook ([`Notebook`]).
    Jupyter,
    /// A Mermaid diagram (rasterized to the graphics route, source as fallback).
    Mermaid,
    /// A PDF document (rasterized to the graphics route, text as fallback).
    Pdf,
    /// Not previewable (binary with no recognized type).
    Unknown,
}

/// Lowercased final extension of `path`, if any.
fn ext_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// Heuristic: does `head` (the first bytes of the file) look like binary?
///
/// A NUL byte is the canonical text/binary discriminator — the same rule the
/// existing text preview uses to reject binaries.
fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
}

/// Map a previewed file to its [`PreviewRoute`].
///
/// `head` is a prefix of the file's bytes (may be empty when unavailable); it is
/// consulted only for the extension-less / unknown-extension fallback.
pub fn route_for(path: &Path, head: &[u8]) -> PreviewRoute {
    match ext_of(path).as_deref() {
        Some("csv" | "tsv") => PreviewRoute::Csv,
        Some("ipynb") => PreviewRoute::Jupyter,
        Some("mmd" | "mermaid") => PreviewRoute::Mermaid,
        Some("pdf") => PreviewRoute::Pdf,
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "ico" | "avif") => {
            PreviewRoute::Image
        }
        // Known-text extensions never need a sniff.
        Some(
            "txt" | "md" | "markdown" | "rs" | "toml" | "json" | "yaml" | "yml" | "js" | "ts"
            | "py" | "go" | "c" | "h" | "cpp" | "hpp" | "sh" | "lock" | "cfg" | "ini" | "html"
            | "css" | "xml" | "sql",
        ) => PreviewRoute::Text,
        // No / unrecognized extension: fall back on a content sniff.
        _ => {
            if head.is_empty() || !looks_binary(head) {
                PreviewRoute::Text
            } else {
                PreviewRoute::Unknown
            }
        }
    }
}

/// The field delimiter for a CSV/TSV path: tab for `.tsv`, comma otherwise.
fn delimiter_for(path: &Path) -> u8 {
    match ext_of(path).as_deref() {
        Some("tsv") => b'\t',
        _ => b',',
    }
}

// ── CSV table model ───────────────────────────────────────────────────────────

/// A parsed, bounded CSV/TSV table: rows of string cells plus per-column display
/// widths, ready for the host to render as a scrollable grid.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CsvTable {
    /// Rows of cells. The first row is treated as the header by the renderer;
    /// this model does not itself distinguish it.
    pub rows: Vec<Vec<String>>,
    /// Column count = the widest row's field count (short rows render blanks).
    pub cols: usize,
    /// Per-column display width, each clamped to [`MAX_COL_WIDTH`].
    pub col_widths: Vec<usize>,
    /// True when parsing stopped at [`MAX_ROWS`] (more rows exist on disk).
    pub truncated: bool,
}

/// Row cap: a preview is bounded so a huge CSV never balloons memory/scrollback.
pub const MAX_ROWS: usize = 10_000;
/// Per-column display-width cap so one wide field can't dominate the grid.
pub const MAX_COL_WIDTH: usize = 40;

impl CsvTable {
    /// Parse CSV/TSV `content` for `path` (delimiter picked from the extension).
    ///
    /// RFC-4180-ish: double-quoted fields may contain the delimiter, newlines,
    /// and escaped quotes (`""`). Bounded at [`MAX_ROWS`].
    pub fn parse(path: &Path, content: &str) -> CsvTable {
        Self::parse_with_delim(content, delimiter_for(path))
    }

    fn parse_with_delim(content: &str, delim: u8) -> CsvTable {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut row: Vec<String> = Vec::new();
        // Accumulate field content as raw bytes, decoding to UTF-8 at field end:
        // the CSV structural bytes (delimiter, `"`, CR, LF) are all ASCII and so
        // never collide with a UTF-8 multibyte continuation byte.
        let mut field: Vec<u8> = Vec::new();
        let mut in_quotes = false;
        let mut truncated = false;
        let bytes = content.as_bytes();
        let mut i = 0;

        let take_field = |field: &mut Vec<u8>| -> String {
            String::from_utf8_lossy(&std::mem::take(field)).into_owned()
        };

        macro_rules! end_field {
            () => {
                row.push(take_field(&mut field))
            };
        }
        // Push the current field+row; trip `truncated` once the row cap is hit.
        macro_rules! end_row {
            () => {{
                end_field!();
                rows.push(std::mem::take(&mut row));
                if rows.len() >= MAX_ROWS {
                    truncated = true;
                }
            }};
        }

        while i < bytes.len() {
            let b = bytes[i];
            if in_quotes {
                if b == b'"' {
                    if bytes.get(i + 1) == Some(&b'"') {
                        field.push(b'"');
                        i += 2;
                        continue;
                    }
                    in_quotes = false;
                    i += 1;
                    continue;
                }
                field.push(b);
                i += 1;
                continue;
            }
            match b {
                b'"' => in_quotes = true,
                _ if b == delim => end_field!(),
                b'\n' => {
                    end_row!();
                    if truncated {
                        break;
                    }
                }
                b'\r' => {} // swallow CR (CRLF and lone CR both normalize)
                _ => field.push(b),
            }
            i += 1;
        }
        // Trailing field/row with no terminating newline.
        if !truncated && (!field.is_empty() || !row.is_empty()) {
            end_row!();
        }

        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut col_widths = vec![0usize; cols];
        for r in &rows {
            for (c, cell) in r.iter().enumerate() {
                let w = cell.chars().count().min(MAX_COL_WIDTH);
                if w > col_widths[c] {
                    col_widths[c] = w;
                }
            }
        }
        CsvTable {
            rows,
            cols,
            col_widths,
            truncated,
        }
    }

    /// Cell at `(row, col)`, or `""` when a short row has no such column.
    pub fn cell(&self, row: usize, col: usize) -> &str {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .map(String::as_str)
            .unwrap_or("")
    }
}

// ── Jupyter notebook model ────────────────────────────────────────────────────

/// A classified notebook cell kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// A code cell — the host highlights its source via the text route (AF 396).
    Code,
    /// A markdown cell — rendered as text.
    Markdown,
    /// A raw cell — rendered as text verbatim.
    Raw,
}

/// One notebook cell: its kind, joined source, and a count of image outputs
/// (which the host routes to the graphics path, AF 399).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCell {
    pub kind: CellKind,
    pub source: String,
    /// Number of image/rich outputs the cell produced (code cells only).
    pub image_outputs: usize,
}

/// An ordered notebook: cells in document order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notebook {
    pub cells: Vec<NotebookCell>,
}

impl Notebook {
    /// Parse a `.ipynb` document. Returns `Err` on malformed JSON or a missing
    /// `cells` array; unknown cell types classify as [`CellKind::Raw`].
    pub fn parse(content: &str) -> Result<Notebook, String> {
        let v: serde_json::Value =
            serde_json::from_str(content).map_err(|e| format!("invalid notebook JSON: {e}"))?;
        let cells_json = v
            .get("cells")
            .and_then(|c| c.as_array())
            .ok_or_else(|| "notebook has no `cells` array".to_string())?;
        let mut cells = Vec::with_capacity(cells_json.len());
        for c in cells_json {
            let kind = match c.get("cell_type").and_then(|t| t.as_str()) {
                Some("code") => CellKind::Code,
                Some("markdown") => CellKind::Markdown,
                _ => CellKind::Raw,
            };
            let source = join_source(c.get("source"));
            let image_outputs = if kind == CellKind::Code {
                count_image_outputs(c.get("outputs"))
            } else {
                0
            };
            cells.push(NotebookCell {
                kind,
                source,
                image_outputs,
            });
        }
        Ok(Notebook { cells })
    }
}

/// `source` in `.ipynb` is either a string or an array of line-strings; join to
/// one string preserving order.
fn join_source(v: Option<&serde_json::Value>) -> String {
    match v {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(lines)) => lines
            .iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Count outputs that carry an image MIME bundle (`image/png`, `image/jpeg`, …)
/// so the host knows how many graphics-route panes a cell needs.
fn count_image_outputs(v: Option<&serde_json::Value>) -> usize {
    let Some(outputs) = v.and_then(|o| o.as_array()) else {
        return 0;
    };
    outputs
        .iter()
        .filter(|o| {
            o.get("data")
                .and_then(|d| d.as_object())
                .is_some_and(|d| d.keys().any(|k| k.starts_with("image/")))
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn routes_by_extension() {
        assert_eq!(route_for(&p("a.csv"), b""), PreviewRoute::Csv);
        assert_eq!(route_for(&p("a.tsv"), b""), PreviewRoute::Csv);
        assert_eq!(route_for(&p("nb.ipynb"), b""), PreviewRoute::Jupyter);
        assert_eq!(route_for(&p("d.mmd"), b""), PreviewRoute::Mermaid);
        assert_eq!(route_for(&p("d.mermaid"), b""), PreviewRoute::Mermaid);
        assert_eq!(route_for(&p("doc.pdf"), b""), PreviewRoute::Pdf);
        assert_eq!(route_for(&p("i.PNG"), b""), PreviewRoute::Image);
        assert_eq!(route_for(&p("i.jpeg"), b""), PreviewRoute::Image);
        assert_eq!(route_for(&p("main.rs"), b""), PreviewRoute::Text);
    }

    #[test]
    fn unknown_extension_sniffs_text_vs_binary() {
        // No extension, textual bytes → Text.
        assert_eq!(route_for(&p("README"), b"hello world"), PreviewRoute::Text);
        // No extension, empty head → Text (optimistic).
        assert_eq!(route_for(&p("README"), b""), PreviewRoute::Text);
        // Unknown extension, binary bytes → Unknown.
        assert_eq!(route_for(&p("blob.xyz"), b"a\0b"), PreviewRoute::Unknown);
        // Unknown extension, text bytes → Text.
        assert_eq!(route_for(&p("blob.xyz"), b"plain"), PreviewRoute::Text);
    }

    #[test]
    fn csv_parses_simple_grid_and_widths() {
        let t = CsvTable::parse(&p("a.csv"), "name,age\nalice,30\nbob,7\n");
        assert_eq!(t.cols, 2);
        assert_eq!(t.rows.len(), 3);
        assert_eq!(t.cell(0, 0), "name");
        assert_eq!(t.cell(1, 0), "alice");
        assert_eq!(t.cell(2, 1), "7");
        // widths: col0 = max("name"=4,"alice"=5,"bob"=3)=5; col1 = max(3,2,1)=3
        assert_eq!(t.col_widths, vec![5, 3]);
        assert!(!t.truncated);
    }

    #[test]
    fn csv_handles_quotes_delimiters_and_newlines() {
        let t = CsvTable::parse(&p("a.csv"), "a,\"b,c\",\"line\none\"\n\"q\"\"x\",y,z\n");
        assert_eq!(t.cols, 3);
        assert_eq!(t.cell(0, 1), "b,c");
        assert_eq!(t.cell(0, 2), "line\none");
        assert_eq!(t.cell(1, 0), "q\"x");
    }

    #[test]
    fn csv_preserves_multibyte_utf8() {
        let t = CsvTable::parse(&p("a.csv"), "café,naïve\nüber,日本\n");
        assert_eq!(t.cell(0, 0), "café");
        assert_eq!(t.cell(0, 1), "naïve");
        assert_eq!(t.cell(1, 1), "日本");
        // width counts chars, not bytes
        assert_eq!(t.col_widths[0], 4); // "café"
    }

    #[test]
    fn tsv_uses_tab_delimiter() {
        let t = CsvTable::parse(&p("a.tsv"), "x\ty\n1\t2\n");
        assert_eq!(t.cols, 2);
        assert_eq!(t.cell(1, 1), "2");
    }

    #[test]
    fn csv_trailing_row_without_newline_and_short_rows() {
        let t = CsvTable::parse(&p("a.csv"), "a,b,c\n1,2");
        assert_eq!(t.cols, 3);
        assert_eq!(t.rows.len(), 2);
        // short row → missing column reads as ""
        assert_eq!(t.cell(1, 2), "");
    }

    #[test]
    fn csv_column_width_is_capped() {
        let wide = "x".repeat(100);
        let t = CsvTable::parse(&p("a.csv"), &format!("{wide}\n"));
        assert_eq!(t.col_widths, vec![MAX_COL_WIDTH]);
    }

    #[test]
    fn csv_row_cap_truncates() {
        let mut s = String::new();
        for i in 0..(MAX_ROWS + 50) {
            s.push_str(&format!("{i}\n"));
        }
        let t = CsvTable::parse(&p("a.csv"), &s);
        assert!(t.truncated);
        assert_eq!(t.rows.len(), MAX_ROWS);
    }

    #[test]
    fn empty_csv_is_empty_table() {
        let t = CsvTable::parse(&p("a.csv"), "");
        assert_eq!(t.cols, 0);
        assert!(t.rows.is_empty());
        assert_eq!(t.cell(0, 0), "");
    }

    #[test]
    fn notebook_orders_and_classifies_cells() {
        let nb = r##"{
            "cells": [
                {"cell_type": "markdown", "source": ["# Title\n", "text"]},
                {"cell_type": "code", "source": "print(1)", "outputs": []},
                {"cell_type": "raw", "source": "verbatim"},
                {"cell_type": "weird", "source": "x"}
            ]
        }"##;
        let n = Notebook::parse(nb).unwrap();
        assert_eq!(n.cells.len(), 4);
        assert_eq!(n.cells[0].kind, CellKind::Markdown);
        assert_eq!(n.cells[0].source, "# Title\ntext");
        assert_eq!(n.cells[1].kind, CellKind::Code);
        assert_eq!(n.cells[1].source, "print(1)");
        assert_eq!(n.cells[2].kind, CellKind::Raw);
        // unknown cell_type classifies as Raw
        assert_eq!(n.cells[3].kind, CellKind::Raw);
    }

    #[test]
    fn notebook_counts_image_outputs_on_code_cells() {
        let nb = r##"{
            "cells": [
                {"cell_type": "code", "source": "plot()", "outputs": [
                    {"output_type": "display_data", "data": {"image/png": "iVBOR..."}},
                    {"output_type": "stream", "text": "hi"},
                    {"output_type": "execute_result", "data": {"text/plain": "42"}}
                ]}
            ]
        }"##;
        let n = Notebook::parse(nb).unwrap();
        assert_eq!(n.cells[0].image_outputs, 1);
    }

    #[test]
    fn notebook_rejects_malformed() {
        assert!(Notebook::parse("not json").is_err());
        assert!(Notebook::parse(r#"{"nope": 1}"#).is_err());
    }
}
