// The pure target-resolution + payload model (task 2.1) is complete and
// unit-tested; the mouse-seam wiring into the panel/pane hit-testing (task 2.2)
// is a focused follow-up, so these are used only by tests until then.
#![allow(dead_code)]
//! Pure drag-drop model for the file-tree → pane/markdown affordance (AF 776).
//!
//! The in-process TUI mouse seam lets a file be dragged from the sidebar tree
//! and released over another surface. This module holds the *pure* target-
//! resolution and payload-formatting logic (unit-tested); the mouse handler
//! ([`crate::handlers`]) owns the rect hit-testing and the actual PTY/editor
//! insertion. There is no OS drag source — this is in-process panes only.

/// A drag begun on a file row in the tree: the file's repo-relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragFile {
    pub path: String,
}

/// What a drag was released over, resolved from the surface under the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    /// A terminal pane — insert the file's (shell-safe) path.
    Terminal,
    /// A markdown preview or editor — insert a markdown link.
    Markdown,
    /// Anywhere with no drop affordance (chrome, the tree itself, …) — no-op.
    None,
}

/// The text to insert when `file` is dropped onto `target`, or `None` when the
/// target has no affordance (or the drag payload is empty).
pub fn drop_payload(target: DropTarget, file: &DragFile) -> Option<String> {
    if file.path.is_empty() {
        return None;
    }
    match target {
        DropTarget::Terminal => Some(shell_quote(&file.path)),
        DropTarget::Markdown => Some(markdown_link(&file.path)),
        DropTarget::None => None,
    }
}

/// Quote `path` for safe insertion at a shell prompt: bare when it holds only
/// shell-safe characters, otherwise single-quoted with embedded quotes escaped.
fn shell_quote(path: &str) -> String {
    let safe = |b: u8| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b'%' | b','
            )
    };
    if path.bytes().all(safe) {
        path.to_string()
    } else {
        // POSIX single-quote escaping: end quote, escaped quote, reopen quote.
        format!("'{}'", path.replace('\'', r"'\''"))
    }
}

/// A markdown link `[name](path)` where `name` is the final path segment.
fn markdown_link(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or(path);
    format!("[{name}]({path})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(p: &str) -> DragFile {
        DragFile {
            path: p.to_string(),
        }
    }

    #[test]
    fn terminal_drop_inserts_bare_safe_path() {
        assert_eq!(
            drop_payload(DropTarget::Terminal, &f("src/main.rs")),
            Some("src/main.rs".to_string())
        );
    }

    #[test]
    fn terminal_drop_quotes_paths_with_spaces_and_quotes() {
        assert_eq!(
            drop_payload(DropTarget::Terminal, &f("my docs/a b.txt")),
            Some("'my docs/a b.txt'".to_string())
        );
        assert_eq!(
            drop_payload(DropTarget::Terminal, &f("wei'rd.txt")),
            Some(r"'wei'\''rd.txt'".to_string())
        );
    }

    #[test]
    fn markdown_drop_inserts_link_with_basename() {
        assert_eq!(
            drop_payload(DropTarget::Markdown, &f("docs/guide/setup.md")),
            Some("[setup.md](docs/guide/setup.md)".to_string())
        );
    }

    #[test]
    fn markdown_drop_basename_of_bare_name() {
        assert_eq!(
            drop_payload(DropTarget::Markdown, &f("README")),
            Some("[README](README)".to_string())
        );
    }

    #[test]
    fn no_target_and_empty_path_are_noops() {
        assert_eq!(drop_payload(DropTarget::None, &f("x")), None);
        assert_eq!(drop_payload(DropTarget::Terminal, &f("")), None);
    }
}
