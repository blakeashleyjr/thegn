//! Panel file-open / persistence helpers (extracted from the ratchet-pinned
//! `run.rs`). Pure decisions plus sub-ms `ui_state` upserts — safe on the
//! loop, same as before the extraction.

use crate::chrome::FrameModel;
use thegn_core::store::WorkspaceStore;

/// The editor invocation for a worktree-relative `path`, with the universal
/// `+N` line jump when a location is known. Shared by every panel open path
/// (changed files, review threads, failing tests).
pub(crate) fn editor_open_command(
    cfg: &thegn_core::config::Config,
    path: &str,
    line: Option<usize>,
) -> String {
    let editor = cfg
        .tool_command("editor")
        .unwrap_or("${EDITOR:-vi} .")
        .trim();
    let editor = editor.strip_suffix(" .").unwrap_or(editor);
    let quoted = path.replace('\'', r"'\''");
    match line {
        Some(l) => format!("{editor} +{l} '{quoted}'"),
        None => format!("{editor} '{quoted}'"),
    }
}

/// Parse a `path:line` failure location; bare messages yield `None`.
pub(crate) fn parse_file_line(at: &str) -> Option<(String, usize)> {
    let (path, line) = at.rsplit_once(':')?;
    let line: usize = line.trim().parse().ok()?;
    (!path.is_empty()).then(|| (path.to_string(), line))
}

/// The cursor-th row of the files accordion tree (dir or file), matching the
/// renderer's visible-row order exactly (collapsed subtrees excluded).
pub(crate) fn file_entry_at(
    model: &FrameModel,
    collapsed: &std::collections::HashSet<String>,
    filter: &str,
    cursor: usize,
) -> Option<crate::panel::FileEntry> {
    // Reuse the tree hydration pre-built off-loop (the renderer's source of
    // truth) instead of re-sorting the whole listing per keypress; the
    // changes-only fallback mirrors the renderer's while hydration is
    // in flight.
    let fallback;
    let tree: &[crate::panel::FileEntry] = if !model.panel.file_tree.is_empty() {
        &model.panel.file_tree
    } else {
        let paths: Vec<String> = model.panel.changes.iter().map(|c| c.path.clone()).collect();
        fallback = crate::panel::build_file_tree(&paths);
        &fallback
    };
    crate::panel::file_tree_visible_filtered(tree, collapsed, filter)
        .into_iter()
        .nth(cursor)
        .map(|(_, e)| e.clone())
}

/// Toggle a directory's collapsed state in `panel_ui.files_collapsed` and
/// persist to the DB.
pub(crate) fn toggle_files_collapse(panel_ui: &mut crate::panel::PanelUi, dir: &str) {
    let dir = dir.to_string();
    if panel_ui.files_collapsed.contains(&dir) {
        panel_ui.files_collapsed.remove(&dir);
        crate::db_task::persist(move |db| {
            let _ = db.del_ui_state("panel.files.col", &dir);
        });
    } else {
        panel_ui.files_collapsed.insert(dir.clone());
        crate::db_task::persist(move |db| {
            let _ = db.set_ui_state("panel.files.col", &dir, "1");
        });
    }
}

/// Persist the accordion's open section + wide mode + active tab (mirrors the
/// sidebar's inline `ui_state` writes — single-row upserts on a WAL handle).
/// Routed through the background writer so the loop never blocks on `Db::open`.
pub(crate) fn persist_panel_state(panel_ui: &crate::panel::PanelUi) {
    let (open, width, tab) = (
        panel_ui.open.as_key(),
        panel_ui.width.as_key(),
        panel_ui.tab.as_key(),
    );
    crate::db_task::persist(move |db| {
        let _ = db.set_ui_state("panel", "open", open);
        let _ = db.set_ui_state("panel", "width", width);
        let _ = db.set_ui_state("panel", "tab", tab);
        // Per-section width memory: one row per section under its own scope
        // (mirrors `panel.files.col`), so each section reopens at the width
        // it was last used at.
        let _ = db.set_ui_state("panel.width", open, width);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_command_uses_default_and_strips_dot_suffix() {
        let cfg = thegn_core::config::Config::default();
        // Default editor is "${EDITOR:-vi} ."; the trailing " ." is dropped and
        // the path is single-quoted.
        assert_eq!(
            editor_open_command(&cfg, "src/main.rs", None),
            "${EDITOR:-vi} 'src/main.rs'"
        );
    }

    #[test]
    fn editor_command_adds_line_jump() {
        let cfg = thegn_core::config::Config::default();
        assert_eq!(
            editor_open_command(&cfg, "src/main.rs", Some(42)),
            "${EDITOR:-vi} +42 'src/main.rs'"
        );
    }

    #[test]
    fn editor_command_escapes_single_quotes_in_path() {
        let cfg = thegn_core::config::Config::default();
        // A single quote in the path is shell-escaped as '\'' so the command
        // stays well-formed.
        assert_eq!(
            editor_open_command(&cfg, "a'b.rs", None),
            r"${EDITOR:-vi} 'a'\''b.rs'"
        );
    }

    #[test]
    fn parse_file_line_splits_path_and_line() {
        assert_eq!(
            parse_file_line("src/main.rs:42"),
            Some(("src/main.rs".to_string(), 42))
        );
        // rsplit means the last colon wins — a path containing colons is kept.
        assert_eq!(parse_file_line("a:1:2"), Some(("a:1".to_string(), 2)));
    }

    #[test]
    fn parse_file_line_rejects_bad_inputs() {
        assert_eq!(parse_file_line("just a message"), None);
        assert_eq!(parse_file_line("src/main.rs:notanumber"), None);
        assert_eq!(parse_file_line(":42"), None, "empty path");
        assert_eq!(parse_file_line(""), None);
    }
}
