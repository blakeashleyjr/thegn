//! Panel file-open / persistence helpers (extracted from the ratchet-pinned
//! `run.rs`). Pure decisions plus sub-ms `ui_state` upserts — safe on the
//! loop, same as before the extraction.

use crate::chrome::FrameModel;
use superzej_core::store::WorkspaceStore;

/// The editor invocation for a worktree-relative `path`, with the universal
/// `+N` line jump when a location is known. Shared by every panel open path
/// (changed files, review threads, failing tests).
pub(crate) fn editor_open_command(
    cfg: &superzej_core::config::Config,
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
    cursor: usize,
) -> Option<crate::panel::FileEntry> {
    let source: Vec<String> = if !model.panel.all_files.is_empty() {
        model.panel.all_files.clone()
    } else {
        model.panel.changes.iter().map(|c| c.path.clone()).collect()
    };
    let tree = crate::panel::build_file_tree(&source);
    crate::panel::file_tree_visible(&tree, collapsed)
        .into_iter()
        .nth(cursor)
        .map(|(_, e)| e.clone())
}

/// Back-compat shim for sites that only need a changed file path (Changes section).
pub(crate) fn changed_file_at(model: &FrameModel, cursor: usize) -> Option<String> {
    let paths: Vec<String> = model.panel.changes.iter().map(|c| c.path.clone()).collect();
    crate::panel::build_file_tree(&paths)
        .into_iter()
        .filter(|e| !e.is_dir)
        .nth(cursor)
        .map(|e| e.path)
}

/// Toggle a directory's collapsed state in `panel_ui.files_collapsed` and
/// persist to the DB.
pub(crate) fn toggle_files_collapse(panel_ui: &mut crate::panel::PanelUi, dir: &str) {
    if panel_ui.files_collapsed.contains(dir) {
        panel_ui.files_collapsed.remove(dir);
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = db.del_ui_state("panel.files.col", dir);
        }
    } else {
        panel_ui.files_collapsed.insert(dir.to_string());
        if let Ok(db) = superzej_core::db::Db::open() {
            let _ = db.set_ui_state("panel.files.col", dir, "1");
        }
    }
}

/// Persist the accordion's open section + wide mode + active tab (mirrors the
/// sidebar's inline `ui_state` writes — single-row upserts on a WAL handle,
/// sub-ms).
pub(crate) fn persist_panel_state(panel_ui: &crate::panel::PanelUi) {
    if let Ok(db) = superzej_core::db::Db::open() {
        let _ = db.set_ui_state("panel", "open", panel_ui.open.as_key());
        let _ = db.set_ui_state("panel", "width", panel_ui.width.as_key());
        let _ = db.set_ui_state("panel", "tab", panel_ui.tab.as_key());
    }
}
