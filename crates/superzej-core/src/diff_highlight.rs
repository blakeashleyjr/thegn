//! Inline syntax-highlighting of git diffs using `syntect`.
//!
//! Replaces the `delta` binary with a pure-Rust highlighting pipeline.
//! Each code line (added / removed / context) is highlighted according to the
//! source file's language, preserving diff prefixes so the structural diff
//! information remains visible.

use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color, FontStyle, Style, Theme};
use syntect::parsing::SyntaxSet;

// ---------------------------------------------------------------------------
// Global, lazy-loaded resources (loaded once per process).
// ---------------------------------------------------------------------------

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<Theme> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn theme() -> &'static Theme {
    THEME.get_or_init(|| {
        // Use the same default theme that bat and delta use.
        let ts = syntect::highlighting::ThemeSet::load_defaults();
        ts.themes
            .into_iter()
            .find(|(n, _)| *n == "base16-ocean.dark")
            .map(|(_, t)| t)
            .unwrap_or_default()
    })
}

// ---------------------------------------------------------------------------
// Diff background colours: the theme green/red tinted toward the storm-blue
// base (BG0), so added/removed gutters sit on the same surface as the panels
// rather than a flat green/red on black.
// ---------------------------------------------------------------------------

const BG_ADDED: Color = Color {
    r: 28,
    g: 46,
    b: 40,
    a: 255,
};
const BG_REMOVED: Color = Color {
    r: 52,
    g: 30,
    b: 40,
    a: 255,
};
// Theme GREEN (121;227;165) / RED (247;118;142).
const FG_PREFIX_ADD: &str = "\x1b[38;2;121;227;165m";
const FG_PREFIX_REMOVE: &str = "\x1b[38;2;247;118;142m";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Pre-load the syntect syntax + theme sets (the lazy first load costs
/// ~100-300ms). Call from a background thread at startup so the first
/// drill-in never stalls the user.
pub fn warm() {
    let _ = syntax_set();
    let _ = theme();
}

/// Apply syntax highlighting to a single file's raw git diff output.
///
/// `diff_text` is the raw diff (e.g. from `git diff` without `--color`).
/// `file_path` is used to determine the language for syntax highlighting.
pub fn highlight_diff(diff_text: &str, file_path: &str) -> String {
    let ss = syntax_set();
    let theme = theme();

    // Determine the syntax to use from the file path.
    let path = std::path::Path::new(file_path);
    let syntax = ss
        .find_syntax_by_extension(path.extension().and_then(|e| e.to_str()).unwrap_or(""))
        .or_else(|| {
            // Fall back to whole filename (Makefile, Dockerfile, etc.).
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| ss.find_syntax_by_extension(n))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut out = String::new();

    for line in diff_text.lines() {
        if line.is_empty() {
            out.push('\n');
            continue;
        }

        let (prefix, content) = line.split_at(1);

        // Treat `--- a/...` and `+++ b/...` as headers, not code lines.
        if (line.starts_with("--- ") || line.starts_with("+++ ")) && line.len() > 4 {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        match prefix {
            "+" | "-" | " " => {
                // ---- code line ----
                let bg = match prefix {
                    "+" => Some(BG_ADDED),
                    "-" => Some(BG_REMOVED),
                    _ => None,
                };
                let prefix_fg = match prefix {
                    "+" => FG_PREFIX_ADD,
                    "-" => FG_PREFIX_REMOVE,
                    _ => "",
                };

                // Highlight the content via syntect.
                let ranges = highlighter.highlight_line(content, ss).unwrap_or_default();

                // Emit prefix with diff colour.
                out.push_str(prefix_fg);
                out.push_str(prefix);
                out.push_str("\x1b[0m");

                // Emit highlighted content with diff background.
                for (style, text) in &ranges {
                    out.push_str(&style_ansi(style, bg));
                    out.push_str(text);
                }
                out.push_str("\x1b[0m\n");
            }
            _ => {
                // ---- header / hunk-header / metadata line ----
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// Apply syntax highlighting to a whole file (no diff structure): every line
/// is colored per the file's language. Returns ANSI text for the host's
/// span-based panel renderer (the Files tab's preview drill-in).
pub fn highlight_file(text: &str, file_path: &str) -> String {
    let ss = syntax_set();
    let theme = theme();

    let path = std::path::Path::new(file_path);
    let syntax = ss
        .find_syntax_by_extension(path.extension().and_then(|e| e.to_str()).unwrap_or(""))
        .or_else(|| {
            path.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| ss.find_syntax_by_extension(n))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text());

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut out = String::new();
    for line in text.lines() {
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        let ranges = highlighter.highlight_line(line, ss).unwrap_or_default();
        for (style, chunk) in &ranges {
            out.push_str(&style_ansi(style, None));
            out.push_str(chunk);
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

// ---------------------------------------------------------------------------
// Word-level / intra-line diff (item 601).
// ---------------------------------------------------------------------------

/// One run of characters within a diff line, tagged by whether it differs from
/// the paired line. The host's panel renderer maps `changed` runs to a brighter
/// emphasis tint over the line's add/remove background, so only the bytes that
/// actually changed pop — not the whole line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSeg {
    pub text: String,
    pub changed: bool,
}

/// Append `text` to `segs`, coalescing with the previous run when it carries
/// the same `changed` flag — keeps the segment list minimal for the renderer.
fn push_word_seg(segs: &mut Vec<WordSeg>, text: &str, changed: bool) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = segs.last_mut()
        && last.changed == changed
    {
        last.text.push_str(text);
        return;
    }
    segs.push(WordSeg {
        text: text.to_string(),
        changed,
    });
}

/// Word-level intra-line diff between a removed line `old` and the paired added
/// line `new`. Returns `(old_segs, new_segs)`: the old side tags deleted runs
/// `changed`, the new side tags inserted runs `changed`, and runs common to
/// both are `changed: false`. Tokenized on word boundaries, so a single changed
/// word in an otherwise-equal line emphasizes just that word.
///
/// A side with no content (e.g. `old` empty on a pure insertion) returns an
/// empty `Vec` for that side.
pub fn word_diff(old: &str, new: &str) -> (Vec<WordSeg>, Vec<WordSeg>) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_words(old, new);
    let mut old_segs: Vec<WordSeg> = Vec::new();
    let mut new_segs: Vec<WordSeg> = Vec::new();
    for change in diff.iter_all_changes() {
        let val = change.value();
        match change.tag() {
            ChangeTag::Equal => {
                push_word_seg(&mut old_segs, val, false);
                push_word_seg(&mut new_segs, val, false);
            }
            ChangeTag::Delete => push_word_seg(&mut old_segs, val, true),
            ChangeTag::Insert => push_word_seg(&mut new_segs, val, true),
        }
    }
    (old_segs, new_segs)
}

// ---------------------------------------------------------------------------
// Helper — build ANSI escape sequence for a syntect style + optional diff bg.
// ---------------------------------------------------------------------------

fn style_ansi(style: &Style, bg: Option<Color>) -> String {
    let mut ansi = String::new();

    // Background (from diff, added over what syntect might set).
    if let Some(c) = bg {
        ansi.push_str(&format!("\x1b[48;2;{};{};{}m", c.r, c.g, c.b));
    }

    // Foreground (from syntax highlighting).
    let fg = style.foreground;
    ansi.push_str(&format!("\x1b[38;2;{};{};{}m", fg.r, fg.g, fg.b));

    // Font style.
    if style.font_style.contains(FontStyle::BOLD) {
        ansi.push_str("\x1b[1m");
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        ansi.push_str("\x1b[3m");
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        ansi.push_str("\x1b[4m");
    }

    ansi
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_each_line_kind() {
        let diff = "\
diff --git a/x.rs b/x.rs
--- a/x.rs
+++ b/x.rs
@@ -1,2 +1,2 @@
 fn main() {}
-let removed = 1;
+let added = 2;

";
        let out = highlight_diff(diff, "x.rs");
        // headers/hunk lines pass through verbatim.
        assert!(out.contains("@@ -1,2 +1,2 @@"));
        assert!(out.contains("--- a/x.rs"));
        assert!(out.contains("+++ b/x.rs"));
        // added/removed gutters get their diff foreground colour.
        assert!(out.contains(FG_PREFIX_ADD));
        assert!(out.contains(FG_PREFIX_REMOVE));
        // added line carries the added background.
        assert!(out.contains("\x1b[48;2;28;46;40m"));
        // blank line preserved.
        assert!(out.contains('\n'));
    }

    #[test]
    fn unknown_extension_falls_back_to_plain() {
        // Exercises the extension → whole-filename → plain-text fallback chain
        // without panicking. (Token content can be split by ANSI codes, so we
        // assert on the structural markers rather than exact substrings.)
        let out = highlight_diff("+hello\n", "file.unknownext");
        assert!(out.contains(FG_PREFIX_ADD) && out.ends_with("\x1b[0m\n"));
        // whole-filename fallback path (Makefile has no extension).
        let out2 = highlight_diff(" all:\n", "Makefile");
        assert!(!out2.is_empty());
    }

    #[test]
    fn warm_loads_the_lazy_sets() {
        warm();
        // After warming, highlighting is immediate and well-formed.
        assert!(highlight_file("fn x() {}\n", "a.rs").contains("\x1b[38;2;"));
    }

    #[test]
    fn highlight_file_colors_lines_and_preserves_blanks() {
        let out = highlight_file("fn main() {}\n\nlet x = 1;\n", "x.rs");
        // Each non-empty line ends with a reset; blank lines survive.
        assert!(out.contains("\x1b[38;2;")); // syntect fg colors present
        assert!(out.contains("\x1b[0m\n"));
        assert_eq!(out.lines().count(), 3);
        assert!(out.lines().nth(1).unwrap().is_empty());
        // Unknown extensions fall back to plain text without panicking.
        let plain = highlight_file("hello\n", "f.unknownext");
        assert!(plain.ends_with("\x1b[0m\n"));
    }

    // ---- word_diff (item 601) -------------------------------------------

    /// The full text of a side, ignoring tags — must always reconstruct the
    /// input so we never drop or duplicate characters.
    fn joined(segs: &[WordSeg]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect()
    }
    /// Concatenated text of only the `changed` runs.
    fn changed_text(segs: &[WordSeg]) -> String {
        segs.iter()
            .filter(|s| s.changed)
            .map(|s| s.text.as_str())
            .collect()
    }

    #[test]
    fn word_diff_identical_lines_have_no_emphasis() {
        let (old, new) = word_diff("let x = 1;", "let x = 1;");
        assert_eq!(joined(&old), "let x = 1;");
        assert_eq!(joined(&new), "let x = 1;");
        assert!(old.iter().all(|s| !s.changed));
        assert!(new.iter().all(|s| !s.changed));
        // Coalesced into a single equal run on each side.
        assert_eq!(old.len(), 1);
        assert_eq!(new.len(), 1);
    }

    #[test]
    fn word_diff_single_word_change_emphasizes_only_that_word() {
        let (old, new) = word_diff("let x = 1;", "let x = 2;");
        // Both sides reconstruct fully.
        assert_eq!(joined(&old), "let x = 1;");
        assert_eq!(joined(&new), "let x = 2;");
        // Only the differing region is flagged — the changed run carries the
        // new digit, and the long shared prefix is NOT emphasized. (We assert
        // containment, not an exact token, so the test isn't coupled to how
        // `similar` groups adjacent punctuation like `1;`.)
        assert!(changed_text(&old).contains('1'));
        assert!(changed_text(&new).contains('2'));
        assert!(!changed_text(&old).contains("let"));
        assert!(!changed_text(&new).contains("let"));
        // The shared prefix survives as a non-emphasized run.
        assert!(
            old.iter()
                .any(|s| !s.changed && s.text.contains("let x = "))
        );
    }

    #[test]
    fn word_diff_insertion_only_flags_new_side_only() {
        let (old, new) = word_diff("foo", "foo bar");
        assert_eq!(joined(&old), "foo");
        assert_eq!(joined(&new), "foo bar");
        assert_eq!(changed_text(&old), ""); // nothing removed
        assert!(changed_text(&new).contains("bar"));
        assert!(old.iter().all(|s| !s.changed));
    }

    #[test]
    fn word_diff_deletion_only_flags_old_side_only() {
        let (old, new) = word_diff("foo bar", "foo");
        assert_eq!(joined(&old), "foo bar");
        assert_eq!(joined(&new), "foo");
        assert!(changed_text(&old).contains("bar"));
        assert_eq!(changed_text(&new), "");
        assert!(new.iter().all(|s| !s.changed));
    }

    #[test]
    fn word_diff_empty_sides_return_empty_vecs() {
        // Pure insertion: old side has no segments at all.
        let (old, new) = word_diff("", "abc");
        assert!(old.is_empty());
        assert_eq!(joined(&new), "abc");
        assert_eq!(changed_text(&new), "abc");
        // Pure deletion: symmetric.
        let (old2, new2) = word_diff("abc", "");
        assert!(new2.is_empty());
        assert_eq!(changed_text(&old2), "abc");
        // Both empty: nothing on either side.
        let (old3, new3) = word_diff("", "");
        assert!(old3.is_empty() && new3.is_empty());
    }

    #[test]
    fn word_diff_respects_word_and_punctuation_boundaries() {
        // Changing inside a call: the function name stays, the arg changes.
        let (old, new) = word_diff("draw(a, b)", "draw(a, c)");
        assert_eq!(joined(&old), "draw(a, b)");
        assert_eq!(joined(&new), "draw(a, c)");
        assert!(changed_text(&old).contains('b'));
        assert!(changed_text(&new).contains('c'));
        // The function name is shared and must not be emphasized on either side.
        assert!(!changed_text(&old).contains("draw"));
        assert!(!changed_text(&new).contains("draw"));
        // The unchanged head is preserved as a non-emphasized run.
        assert!(new.iter().any(|s| !s.changed && s.text.contains("draw")));
    }

    #[test]
    fn style_ansi_covers_font_styles_and_bg() {
        let style = Style {
            foreground: Color {
                r: 10,
                g: 20,
                b: 30,
                a: 255,
            },
            background: Color {
                r: 0,
                g: 0,
                b: 0,
                a: 255,
            },
            font_style: FontStyle::BOLD | FontStyle::ITALIC | FontStyle::UNDERLINE,
        };
        let with_bg = style_ansi(&style, Some(BG_ADDED));
        assert!(with_bg.contains("\x1b[48;2;28;46;40m")); // bg
        assert!(with_bg.contains("\x1b[38;2;10;20;30m")); // fg
        assert!(
            with_bg.contains("\x1b[1m")
                && with_bg.contains("\x1b[3m")
                && with_bg.contains("\x1b[4m")
        );
        // No-bg, no-font-style path.
        let plain = style_ansi(
            &Style {
                foreground: Color {
                    r: 1,
                    g: 2,
                    b: 3,
                    a: 255,
                },
                background: Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                },
                font_style: FontStyle::empty(),
            },
            None,
        );
        assert!(!plain.contains("\x1b[48"));
        assert!(plain.contains("\x1b[38;2;1;2;3m"));
    }
}
