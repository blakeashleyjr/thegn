//! Panel file-open / persistence helpers (extracted from the ratchet-pinned
//! `run.rs`). Pure decisions plus sub-ms `ui_state` upserts — safe on the
//! loop, same as before the extraction.

use crate::chrome::FrameModel;
use thegn_core::store::WorkspaceStore;

/// The editor invocation for a worktree-relative `path`, with the universal
/// `+N` line jump when a location is known. Shared by every panel open path
/// (changed files, review threads, failing tests).
/// Plan the editor launch for `path` (at `line`) through the editor seam
/// (`thegn_core::editor`): `[editor] command` → `[[tools]] editor` →
/// `$VISUAL`/`$EDITOR` → `vi`, with the program's own line-jump syntax.
pub(crate) fn editor_launch(
    cfg: &thegn_core::config::Config,
    path: &str,
    line: Option<usize>,
) -> thegn_core::editor::EditorLaunch {
    let req = thegn_core::editor::OpenRequest {
        path,
        line,
        col: None,
    };
    thegn_core::editor::editor_for(cfg)
        .open(&req)
        .unwrap_or_else(|_| thegn_core::editor::launch_line("vi", &req))
}

/// Run an editor shell line as a detached, reaped process (windowed editors
/// and the explicit "open externally" keys).
pub(crate) fn spawn_editor_detached(command: &str, cwd: Option<&std::path::Path>) {
    let argv = thegn_core::shellinv::run_argv(&thegn_core::util::shell(), command);
    let mut c = std::process::Command::new(&argv[0]);
    c.args(&argv[1..]);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    crate::actions::spawn_detached_reaped(c);
}

/// Open `path` in the editor the way its placement wants: terminal editors
/// get a center tab (or, with `in_pane`, a split next to `focused`); windowed
/// editors are spawned detached so no dead pane is left behind. Returns
/// whether a tab/pane was opened (the caller then moves focus to the center).
#[allow(clippy::too_many_arguments)] // mirrors open_command_pane's shape + placement inputs
pub(crate) fn open_editor(
    session: &mut crate::session::Session,
    panes: &mut crate::panes::Panes,
    cfg: &thegn_core::config::Config,
    path: &str,
    line: Option<usize>,
    cwd: Option<&std::path::Path>,
    center: crate::compositor::Rect,
    in_pane: Option<u32>,
) -> bool {
    let launch = editor_launch(cfg, path, line);
    match launch.placement {
        thegn_core::editor::Placement::External => {
            spawn_editor_detached(&launch.command, cwd);
            false
        }
        thegn_core::editor::Placement::Pane => {
            match in_pane {
                Some(focused) => crate::actions::open_command_pane(
                    session,
                    panes,
                    focused,
                    &launch.command,
                    cwd,
                    center,
                ),
                None => {
                    crate::actions::open_command_tab(session, panes, &launch.command, cwd, center)
                }
            }
            true
        }
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
            let _ = db.del_ui_state("panel.files.col", &dir); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        });
    } else {
        panel_ui.files_collapsed.insert(dir.clone());
        crate::db_task::persist(move |db| {
            let _ = db.set_ui_state("panel.files.col", &dir, "1"); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
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
        let _ = db.set_ui_state("panel", "open", open); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        let _ = db.set_ui_state("panel", "width", width); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        let _ = db.set_ui_state("panel", "tab", tab); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        // Per-section width memory: one row per section under its own scope
        // (mirrors `panel.files.col`), so each section reopens at the width
        // it was last used at.
        let _ = db.set_ui_state("panel.width", open, width); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_command_goes_through_the_seam() {
        // A concrete `[editor] command` template is honoured verbatim (and is
        // environment-independent, so this test is hermetic).
        let mut cfg = thegn_core::config::Config::default();
        cfg.editor.command = "ed {path} +{line}".into();
        assert_eq!(
            editor_launch(&cfg, "src/main.rs", None).command,
            "ed src/main.rs +"
        );
        assert_eq!(
            editor_launch(&cfg, "src/main.rs", Some(42)).command,
            "ed src/main.rs +42"
        );
        // Quoting: a single quote in the path is shell-escaped as '\''.
        assert_eq!(
            editor_launch(&cfg, "a'b.rs", None).command,
            r"ed 'a'\''b.rs' +"
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
