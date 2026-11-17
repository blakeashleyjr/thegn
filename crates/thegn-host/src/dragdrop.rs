//! Pure drag-drop model for the Files-panel → pane affordance (AF 776, THE-135).
//!
//! A file row in the right panel's Files list can be dragged with the mouse
//! and released over a center terminal pane, which types the file's
//! repo-relative path at the prompt (shell-quoted, never executed). This
//! module holds the *pure* parts — the press/motion/release gesture
//! ([`FileDrag`]), the pane hit test ([`pane_at`]) and the payload
//! formatting ([`drop_payload`]) — so they are unit-tested; the run loop owns
//! the event plumbing and writes the payload through
//! `run::paste_text_into_pane` (one chunk, bracketed when the app asked,
//! markers neutralized). There is no OS drag source — in-process panes only.
//!
//! Input-injection hardening: a payload is refused outright when the path
//! holds ANY control character (C0, DEL, C1). Single quotes keep a shell from
//! interpreting the path, but a raw `\n`/`\r` at a non-bracketed prompt still
//! reaches the line editor (an Enter), and ESC/C1 bytes reach readline key
//! bindings — so quoting alone is not enough.

use crate::center::PaneId;
use crate::compositor::Rect;

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
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "markdown drop targets need an in-TUI editor (THE-135)"
        )
    )]
    Markdown,
    /// Anywhere with no drop affordance (chrome, the tree itself, …) — no-op.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the run loop resolves a missed drop before building a payload"
        )
    )]
    None,
}

/// The text to insert when `file` is dropped onto `target`, or `None` when the
/// target has no affordance, the path is empty, or the path holds any control
/// character (`char::is_control`: C0, DEL and C1 — see the module docs). The
/// path is used as given (repo-relative); it is never resolved,
/// canonicalized or looked up on disk.
pub fn drop_payload(target: DropTarget, file: &DragFile) -> Option<String> {
    if file.path.is_empty() || file.path.chars().any(char::is_control) {
        return None;
    }
    match target {
        DropTarget::Terminal => Some(shell_quote(&file.path)),
        DropTarget::Markdown => Some(markdown_link(&file.path)),
        DropTarget::None => None,
    }
}

/// A Files-row drag in progress: armed on the press (which still selects the
/// row), it becomes a drag only once the pointer leaves the press cell — a
/// release before that is a plain click and never inserts anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDrag {
    /// The pressed file's repo-relative path.
    pub path: String,
    /// The press cell `(x, y)`.
    pub press: (usize, usize),
    /// Whether the pointer has left the press cell since the press.
    pub moved: bool,
}

impl FileDrag {
    pub fn new(path: String, press: (usize, usize)) -> Self {
        Self {
            path,
            press,
            moved: false,
        }
    }

    /// Track a held-button motion sample. Returns `true` exactly once: on the
    /// sample that first leaves the press cell (the caller shows the drop
    /// prompt then). Returning to the press cell does not un-move the drag.
    pub fn on_motion(&mut self, x: usize, y: usize) -> bool {
        if self.moved || (x, y) == self.press {
            return false;
        }
        self.moved = true;
        true
    }

    /// The pane to drop into on release, given the pane under the pointer:
    /// only a drag that moved has a target (a motionless release is a click).
    pub fn release_target(&self, hit: Option<PaneId>) -> Option<PaneId> {
        hit.filter(|_| self.moved)
    }
}

/// The center pane whose frame contains `(x, y)`, from the run loop's laid-out
/// `(pane, frame rect, content rect)` list. The frame rect (title bar and
/// border included) is the drop zone: releasing on a pane's chrome still
/// means "that pane". Edges are half-open: inclusive at `x`/`y`, exclusive at
/// `x + cols` / `y + rows`. `crate::pane_drag` keeps its `contains` private
/// and only exposes gesture resolvers, so this is the one hit test here.
pub fn pane_at(frames: &[(PaneId, Rect, Rect)], x: usize, y: usize) -> Option<PaneId> {
    frames
        .iter()
        .find(|(_, f, _)| x >= f.x && x < f.x + f.cols && y >= f.y && y < f.y + f.rows)
        .map(|(id, _, _)| *id)
}

/// The path as shown in a status line: control/format characters replaced
/// (`display_cell`) and clipped to [`STATUS_PATH_MAX`] chars with a trailing
/// `…`, so a hostile or very long file name can neither drive the terminal
/// nor push the rest of the status off-screen.
pub fn status_path(path: &str) -> String {
    let safe = thegn_core::pipeline_report::display_cell(path);
    if safe.chars().count() <= STATUS_PATH_MAX {
        return safe;
    }
    let mut out: String = safe.chars().take(STATUS_PATH_MAX - 1).collect();
    out.push('…');
    out
}

/// Longest path (in chars) a drag/drop status line shows before clipping.
pub const STATUS_PATH_MAX: usize = 60;

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
    #[test]
    fn terminal_drop_refuses_control_characters() {
        // C0 line endings (an Enter at a non-bracketed prompt), ESC (readline
        // bindings), DEL and a C1 CSI must never reach a PTY — even quoted.
        for p in [
            "a\nrm -rf ~",
            "a\rb",
            "a\u{1b}[200~b",
            "a\u{7f}b",
            "a\u{9b}b",
            "\t",
            "nul\0",
        ] {
            assert_eq!(drop_payload(DropTarget::Terminal, &f(p)), None, "{p:?}");
            assert_eq!(drop_payload(DropTarget::Markdown, &f(p)), None, "{p:?}");
        }
    }

    #[test]
    fn terminal_drop_keeps_leading_dash_paths_verbatim() {
        // A leading `-` is only ever an argument typed at the prompt; nothing
        // is executed, so the path is inserted as-is (no `./` rewriting).
        assert_eq!(
            drop_payload(DropTarget::Terminal, &f("-rf")),
            Some("-rf".to_string())
        );
        assert_eq!(
            drop_payload(DropTarget::Terminal, &f("-a b")),
            Some("'-a b'".to_string())
        );
    }

    #[test]
    fn terminal_drop_quotes_space_and_quote_name() {
        assert_eq!(
            drop_payload(DropTarget::Terminal, &f("a b'c.txt")),
            Some(r"'a b'\''c.txt'".to_string())
        );
    }

    #[test]
    fn terminal_drop_keeps_relative_path_unresolved() {
        assert_eq!(
            drop_payload(DropTarget::Terminal, &f("../x/./y.rs")),
            Some("../x/./y.rs".to_string())
        );
    }

    #[test]
    fn file_drag_without_motion_has_no_target() {
        let mut d = FileDrag::new("src/main.rs".into(), (5, 7));
        assert!(
            !d.on_motion(5, 7),
            "a sample on the press cell is not a move"
        );
        assert!(!d.moved);
        assert_eq!(d.release_target(Some(3)), None);
    }

    #[test]
    fn file_drag_moved_then_released_over_pane_targets_it() {
        let mut d = FileDrag::new("src/main.rs".into(), (5, 7));
        assert!(d.on_motion(6, 7), "first move reports the transition");
        assert!(!d.on_motion(9, 9), "later moves do not re-report");
        assert!(
            !d.on_motion(5, 7),
            "returning to the press cell stays moved"
        );
        assert!(d.moved);
        assert_eq!(d.release_target(Some(3)), Some(3));
    }

    #[test]
    fn file_drag_released_over_nothing_has_no_target() {
        let mut d = FileDrag::new("src/main.rs".into(), (5, 7));
        assert!(d.on_motion(5, 8));
        assert_eq!(d.release_target(None), None);
    }

    fn r(x: usize, y: usize, cols: usize, rows: usize) -> Rect {
        Rect { x, y, cols, rows }
    }

    #[test]
    fn pane_at_is_half_open_on_frame_rects() {
        // Two side-by-side panes: 1 at x 10..20, 2 at x 20..30, rows 2..12.
        let frames = [
            (1, r(10, 2, 10, 10), r(11, 3, 8, 8)),
            (2, r(20, 2, 10, 10), r(21, 3, 8, 8)),
        ];
        // Start edges inclusive (the frame/title cell counts).
        assert_eq!(pane_at(&frames, 10, 2), Some(1));
        assert_eq!(pane_at(&frames, 20, 2), Some(2));
        // End edges exclusive.
        assert_eq!(pane_at(&frames, 19, 11), Some(1));
        assert_eq!(pane_at(&frames, 30, 5), None);
        assert_eq!(pane_at(&frames, 15, 12), None);
        // Outside entirely (sidebar / panel / bars).
        assert_eq!(pane_at(&frames, 9, 5), None);
        assert_eq!(pane_at(&frames, 15, 1), None);
        assert_eq!(pane_at(&[], 0, 0), None);
    }

    #[test]
    fn status_path_neutralizes_controls_and_clips() {
        assert_eq!(status_path("src/main.rs"), "src/main.rs");
        assert!(!status_path("a\u{1b}b\nc").chars().any(char::is_control));
        let long = "x".repeat(STATUS_PATH_MAX + 10);
        let shown = status_path(&long);
        assert_eq!(shown.chars().count(), STATUS_PATH_MAX);
        assert!(shown.ends_with('…'));
        assert_eq!(
            status_path(&"y".repeat(STATUS_PATH_MAX)).chars().count(),
            STATUS_PATH_MAX
        );
    }
}
