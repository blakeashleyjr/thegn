//! Navigation targets: every worktree (DB), recent repo (DB), and live zellij
//! tab — the universal-navigator superpower. All synchronous and cheap, so they
//! load the instant `@` is typed.

use crate::db::Db;
use crate::palette::item::{Action, Row, RowKind};
use crate::{theme, util, zellij};
use std::path::Path;

/// Build navigation rows: worktrees, then recent repos, then open tabs.
pub fn rows() -> Vec<Row> {
    let mut out = Vec::new();

    if let Ok(db) = Db::open() {
        if let Ok(wts) = db.worktrees() {
            for w in wts {
                let label = if w.branch.is_empty() {
                    w.tab_name.clone()
                } else {
                    w.branch.clone()
                };
                out.push(Row {
                    glyph: "⎇".into(),
                    hue: theme::PURPLE,
                    label,
                    detail: tilde(&w.worktree),
                    haystack: format!("{} {} {}", w.branch, w.tab_name, w.worktree),
                    kind: RowKind::Worktree,
                    action: Action::GotoTab(w.tab_name.clone()),
                    frecency_key: Some(format!("wt:{}", w.worktree)),
                    preview_path: Some(std::path::PathBuf::from(&w.worktree)),
                });
            }
        }
        if let Ok(repos) = db.recent_repos(30) {
            for path in repos {
                out.push(Row {
                    glyph: "□".into(),
                    hue: theme::BLUE,
                    label: basename(&path),
                    detail: tilde(&path),
                    haystack: path.clone(),
                    kind: RowKind::Repo,
                    action: Action::OpenRepo(path.clone()),
                    frecency_key: Some(format!("repo:{path}")),
                    preview_path: Some(std::path::PathBuf::from(&path)),
                });
            }
        }
    }

    out.extend(tab_rows(zellij::tab_names()));
    out
}

/// Build "jump to open tab" rows from a list of live tab names. Split out so the
/// loop is testable without a running zellij session.
fn tab_rows(names: Vec<String>) -> Vec<Row> {
    names
        .into_iter()
        .map(|name| Row {
            glyph: "▦".into(),
            hue: theme::TEAL,
            label: name.clone(),
            detail: "tab".into(),
            haystack: name.clone(),
            kind: RowKind::Tab,
            action: Action::GotoTab(name),
            frecency_key: None,
            preview_path: None,
        })
        .collect()
}

fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// Replace a leading `$HOME` with `~` for compact display.
fn tilde(path: &str) -> String {
    let home = util::home();
    let home = home.to_string_lossy();
    match path.strip_prefix(home.as_ref()) {
        Some(rest) => format!("~{rest}"),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::item::RowKind;
    use crate::palette::testutil;

    #[test]
    fn basename_and_tilde_helpers() {
        assert_eq!(basename("/a/b/repo"), "repo");
        assert_eq!(basename("bare"), "bare");
        let home = util::home();
        let inside = format!("{}/work/x", home.to_string_lossy());
        assert_eq!(tilde(&inside), "~/work/x");
        assert_eq!(tilde("/elsewhere"), "/elsewhere");
    }

    #[test]
    fn rows_surface_db_worktrees_and_repos() {
        testutil::sandbox();
        let db = crate::db::Db::open().unwrap();
        db.put_worktree(
            "repo/feature-x",
            "/repos/myrepo",
            "/wt/feature-x",
            "feature/x",
            None,
        )
        .unwrap();
        db.touch_repo("/repos/myrepo", "myrepo").unwrap();

        let rows = rows();
        // The worktree appears as a GotoTab row labelled by its branch.
        let wt = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.label == "feature/x")
            .expect("worktree row");
        assert!(matches!(wt.action, Action::GotoTab(ref t) if t == "repo/feature-x"));
        assert_eq!(
            wt.preview_path.as_deref(),
            Some(std::path::Path::new("/wt/feature-x"))
        );
        // The repo appears as an OpenRepo row labelled by basename.
        assert!(rows
            .iter()
            .any(|r| r.kind == RowKind::Repo && r.label == "myrepo"));
    }

    #[test]
    fn worktree_with_empty_branch_falls_back_to_tab_name() {
        testutil::sandbox();
        let db = crate::db::Db::open().unwrap();
        db.put_worktree("repo/home", "/repos/r2", "/wt/home", "", None)
            .unwrap();
        let rows = rows();
        assert!(rows
            .iter()
            .any(|r| r.kind == RowKind::Worktree && r.label == "repo/home"));
    }

    #[test]
    fn tab_rows_map_names_to_goto_actions() {
        let rows = tab_rows(vec!["a/b".into(), "c/d".into()]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, RowKind::Tab);
        assert!(matches!(rows[1].action, Action::GotoTab(ref t) if t == "c/d"));
    }
}
