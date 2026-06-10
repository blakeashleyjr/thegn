//! The unit of the result list: a `Row` (what the user sees) carrying an
//! `Action` (what Enter does) and a `haystack` (what nucleo fuzzy-matches). Rows
//! from every source share this type so the engine, list rendering, and preview
//! all speak one language.

use std::path::PathBuf;

/// What activating a row does. Dispatched by `dispatch.rs` *after* the iocraft
/// render loop exits and the terminal is restored — never while fullscreen.
#[derive(Debug, Clone)]
pub enum Action {
    // --- static commands (mirror the legacy `menu` actions) ---
    NewWorkspace,
    NewWorktree,
    NewPanel,
    NewTab,
    SwitchRepo,
    Dashboard,
    ToggleSidebar,
    TogglePanel,
    Tool(String), // lazygit | yazi | editor | diff | any configured tool
    CloseWorktree,
    PrOpen,
    PrCreate,
    PrStatus,
    PrApprove,
    PrMerge,
    PrRerun,
    Config,
    ThemePreview,
    // --- dynamic, context-aware ---
    OpenFile(PathBuf),
    OpenFileAt(PathBuf, usize),
    GotoTab(String),
    OpenRepo(String),
    Checkout(String),
}

/// The category of a row — drives the glyph, preview, and per-item action menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Command,
    File,
    Content,
    Worktree,
    Repo,
    Tab,
    Branch,
    Pr,
}

/// A single, fuzzy-matchable, renderable, dispatchable result row.
#[derive(Debug, Clone)]
pub struct Row {
    /// 1–2 cell identity glyph.
    pub glyph: String,
    /// Glyph color, as a theme "R;G;B".
    pub hue: &'static str,
    /// Primary text.
    pub label: String,
    /// Right-aligned secondary hint (keybind, path, kind, …).
    pub detail: String,
    /// What nucleo matches against (usually label, or a path for files).
    pub haystack: String,
    pub kind: RowKind,
    pub action: Action,
    /// A stable key for frecency tracking (commands/nav only).
    pub frecency_key: Option<String>,
    /// A filesystem path the preview pane can render even when the `action`
    /// carries none (e.g. a worktree row whose action is `GotoTab`).
    pub preview_path: Option<std::path::PathBuf>,
}

impl Row {
    /// A command/cheatsheet row: glyph + label + keybind hint.
    pub fn command(
        glyph: &str,
        hue: &'static str,
        label: &str,
        keybind: &str,
        action: Action,
        key: &str,
    ) -> Row {
        Row {
            glyph: glyph.to_string(),
            hue,
            label: label.to_string(),
            detail: keybind.to_string(),
            haystack: label.to_string(),
            kind: RowKind::Command,
            action,
            frecency_key: Some(key.to_string()),
            preview_path: None,
        }
    }

    /// Clone this row but swap in a different action (used to build per-item
    /// secondary actions while preserving the row's frecency key/preview).
    fn with(&self, glyph: &str, hue: &'static str, label: &str, action: Action) -> Row {
        Row {
            glyph: glyph.to_string(),
            hue,
            label: label.to_string(),
            detail: String::new(),
            haystack: label.to_string(),
            kind: RowKind::Command,
            action,
            frecency_key: self.frecency_key.clone(),
            preview_path: self.preview_path.clone(),
        }
    }
}

/// The per-item action menu (opened with Tab): context actions for `row`, the
/// first being its primary action. Each is itself a `Row`, so the normal list
/// selection/Enter machinery handles it unchanged.
pub fn secondary(row: &Row) -> Vec<Row> {
    use crate::theme::*;
    match row.kind {
        RowKind::File | RowKind::Content => vec![
            row.with("✎", TEAL, "Open in editor", row.action.clone()),
            row.with("⊞", PURPLE, "Reveal in yazi", Action::Tool("yazi".into())),
            row.with("±", AMBER, "Git diff", Action::Tool("diff".into())),
        ],
        RowKind::Worktree => vec![
            row.with("⎇", PURPLE, "Switch to worktree", row.action.clone()),
            row.with("⬡", GREEN, "PR — status / checks", Action::PrStatus),
            row.with("±", AMBER, "Git diff", Action::Tool("diff".into())),
            row.with("✕", RED, "Close worktree (+ tab)", Action::CloseWorktree),
        ],
        RowKind::Repo => vec![row.with("✦", BLUE, "Open as workspace", row.action.clone())],
        RowKind::Branch => vec![row.with("⎇", TEAL, "Switch to branch", row.action.clone())],
        _ => vec![row.with(&row.glyph, row.hue, &row.label, row.action.clone())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_of(kind: RowKind) -> Row {
        Row {
            glyph: "x".into(),
            hue: crate::theme::TEAL,
            label: "lbl".into(),
            detail: "d".into(),
            haystack: "lbl".into(),
            kind,
            action: Action::Dashboard,
            frecency_key: Some("k".into()),
            preview_path: None,
        }
    }

    #[test]
    fn command_row_sets_haystack_to_label_and_key() {
        let r = Row::command(
            "✦",
            crate::theme::TEAL,
            "New tab",
            "Alt-t",
            Action::NewTab,
            "new-tab",
        );
        assert_eq!(r.haystack, "New tab");
        assert_eq!(r.frecency_key.as_deref(), Some("new-tab"));
        assert_eq!(r.kind, RowKind::Command);
        assert!(r.preview_path.is_none());
    }

    #[test]
    fn secondary_actions_vary_by_kind() {
        assert_eq!(secondary(&row_of(RowKind::File)).len(), 3);
        assert_eq!(secondary(&row_of(RowKind::Content)).len(), 3);
        assert_eq!(secondary(&row_of(RowKind::Worktree)).len(), 4);
        assert_eq!(secondary(&row_of(RowKind::Repo)).len(), 1);
        assert_eq!(secondary(&row_of(RowKind::Branch)).len(), 1);
        // Other kinds collapse to just their primary action.
        assert_eq!(secondary(&row_of(RowKind::Tab)).len(), 1);
        assert_eq!(secondary(&row_of(RowKind::Command)).len(), 1);
    }

    #[test]
    fn secondary_rows_inherit_frecency_key_and_preview() {
        let mut base = row_of(RowKind::Worktree);
        base.preview_path = Some(std::path::PathBuf::from("/w"));
        for s in secondary(&base) {
            assert_eq!(s.frecency_key.as_deref(), Some("k"));
            assert_eq!(s.preview_path.as_deref(), Some(std::path::Path::new("/w")));
        }
    }
}
