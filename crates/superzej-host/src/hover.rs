//! The hover / signature / code-action preview overlay (roadmap item 532).
//!
//! A read-only floating popup summoned on a selected symbol (`h` in the Symbols
//! section). It shows the language server's hover documentation (markdown
//! flattened to styled terminal lines), the signature(s), and any offered code
//! actions. It is purely informational — dismissed by any key — consistent with
//! superzej's "navigate, don't edit in place" stance.

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{Line, Tok, seg};
use termwiz::surface::Surface;

/// A built hover popup: a title (the symbol) and its rendered content lines.
#[derive(Debug, Clone)]
pub struct HoverPopup {
    pub title: String,
    pub lines: Vec<Line>,
}

/// Wrap width for prose inside the popup (the layer adds border + padding).
const WRAP: usize = 72;

impl HoverPopup {
    /// Assemble the popup from the LSP results for one symbol.
    pub fn build(
        symbol: &str,
        hover_md: Option<&str>,
        signatures: &[String],
        actions: &[String],
    ) -> HoverPopup {
        let mut lines: Vec<Line> = Vec::new();

        if let Some(md) = hover_md.filter(|m| !m.trim().is_empty()) {
            lines.extend(markdown_to_lines(md, WRAP));
        }

        if !signatures.is_empty() {
            push_section(&mut lines, "signature");
            for s in signatures {
                lines.push(Line::segs(vec![seg(Tok::Slot(S::Text), s.clone())]));
            }
        }

        if !actions.is_empty() {
            push_section(&mut lines, "code actions");
            for a in actions {
                lines.push(Line::segs(vec![seg(
                    Tok::Slot(S::Ghost2),
                    format!("• {a}"),
                )]));
            }
        }

        if lines.is_empty() {
            lines.push(Line::segs(vec![seg(
                Tok::Slot(S::Ghost2),
                "no hover information".to_string(),
            )]));
        }

        HoverPopup {
            title: symbol.to_string(),
            lines,
        }
    }

    /// Draw the popup centered over `screen`.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let rows = self
            .lines
            .len()
            .clamp(1, screen.rows.saturating_sub(4).max(1));
        let spec = LayerSpec {
            title: self.title.clone(),
            badge: Some(" h ".into()),
            cols: WRAP + 2,
            rows,
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            ..LayerSpec::default()
        };
        if let Some(inner) = open_layer(surface, screen, &spec) {
            crate::seg::draw_lines(surface, inner, &self.lines, Tok::Slot(S::Panel));
        }
    }
}

fn push_section(lines: &mut Vec<Line>, label: &str) {
    if !lines.is_empty() {
        lines.push(Line::Blank);
    }
    lines.push(Line::segs(vec![seg(
        Tok::Slot(S::Accent),
        format!("── {label} ──"),
    )]));
}

/// Convert hover markdown into styled terminal lines: fenced code blocks render
/// in an accent color verbatim; prose has its inline markers stripped and is
/// word-wrapped to `width`. Pure (no I/O) and unit-tested.
pub fn markdown_to_lines(md: &str, width: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut in_code = false;
    for raw in md.lines() {
        let trimmed = raw.trim_end();
        if trimmed.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(Line::segs(vec![seg(
                Tok::Slot(S::Accent),
                trimmed.to_string(),
            )]));
            continue;
        }
        if trimmed.trim().is_empty() {
            out.push(Line::Blank);
            continue;
        }
        let (prefix, body) = strip_markers(trimmed);
        for (i, chunk) in wrap(&body, width.saturating_sub(prefix.len()))
            .into_iter()
            .enumerate()
        {
            let lead = if i == 0 {
                prefix.clone()
            } else {
                " ".repeat(prefix.len())
            };
            out.push(Line::segs(vec![seg(
                Tok::Slot(S::Text),
                format!("{lead}{chunk}"),
            )]));
        }
    }
    out
}

/// Strip a line's leading markdown markers, returning a display prefix (e.g. the
/// bullet) and the marker-free remainder (also drops inline `*`/`` ` `` runs).
fn strip_markers(line: &str) -> (String, String) {
    let t = line.trim_start();
    let (prefix, rest) = if let Some(r) = t.strip_prefix("# ") {
        (String::new(), r)
    } else if let Some(r) = t.strip_prefix("## ") {
        (String::new(), r)
    } else if let Some(r) = t.strip_prefix("### ") {
        (String::new(), r)
    } else if let Some(r) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
        ("• ".to_string(), r)
    } else if let Some(r) = t.strip_prefix("> ") {
        ("┃ ".to_string(), r)
    } else {
        (String::new(), t)
    };
    let cleaned: String = rest.chars().filter(|&c| c != '*' && c != '`').collect();
    (prefix, cleaned)
}

/// Greedy word-wrap to `width` columns (never narrower than 8).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match line {
                Line::Segs(segs) => segs.iter().map(|s| s.text.clone()).collect(),
                Line::Split { l, r } => {
                    let lt: String = l.iter().map(|s| s.text.clone()).collect();
                    let rt: String = r.iter().map(|s| s.text.clone()).collect();
                    format!("{lt}{rt}")
                }
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn code_blocks_render_verbatim_without_fences() {
        let md = "doc\n```rust\nfn x() -> u8\n```";
        let lines = markdown_to_lines(md, 72);
        let t = texts(&lines);
        assert!(t.contains(&"doc".to_string()));
        assert!(t.contains(&"fn x() -> u8".to_string()));
        assert!(!t.iter().any(|l| l.contains("```")));
    }

    #[test]
    fn prose_strips_markers_and_wraps() {
        let lines = markdown_to_lines("# Title\n- **bold** item", 72);
        let t = texts(&lines);
        assert!(t.contains(&"Title".to_string()));
        assert!(t.iter().any(|l| l.starts_with("• ") && l.contains("bold")));
        assert!(!t.iter().any(|l| l.contains('*')));
    }

    #[test]
    fn long_prose_wraps_to_width() {
        let long = "word ".repeat(40);
        let lines = markdown_to_lines(&long, 20);
        assert!(lines.len() > 1);
        for l in texts(&lines) {
            assert!(l.chars().count() <= 20, "line too wide: {l:?}");
        }
    }

    #[test]
    fn build_assembles_sections() {
        let p = HoverPopup::build(
            "greet",
            Some("Greets the user"),
            &["fn greet() -> u8".to_string()],
            &["Import greet".to_string()],
        );
        assert_eq!(p.title, "greet");
        let t = texts(&p.lines);
        assert!(t.iter().any(|l| l.contains("Greets the user")));
        assert!(t.iter().any(|l| l.contains("signature")));
        assert!(t.iter().any(|l| l.contains("fn greet")));
        assert!(t.iter().any(|l| l.contains("code actions")));
        assert!(t.iter().any(|l| l.contains("Import greet")));
    }

    #[test]
    fn build_handles_no_info() {
        let p = HoverPopup::build("x", None, &[], &[]);
        let t = texts(&p.lines);
        assert!(t.iter().any(|l| l.contains("no hover information")));
    }
}
