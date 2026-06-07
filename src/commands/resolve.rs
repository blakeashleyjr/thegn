//! `superzej resolve-worktree --session <s> --tab <t>` — internal helper for the
//! panel plugin, which knows the focused (session, tab) but not its path
//! (zellij's PaneInfo carries no cwd). Prints the worktree path, or nothing.

use crate::db::{self, Db};
use crate::{repo, util};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn run(session: Option<String>, tab: Option<String>) -> Result<()> {
    let session = session.unwrap_or_else(db::session);
    let Some(tab) = tab else {
        return Ok(());
    };
    if let Some(path) = resolve_tab_worktree(&session, &tab) {
        crate::outln!("{path}");
    }
    Ok(())
}

/// The worktree path for a (session, tab) pair. Extra same-worktree tabs
/// ("{base} ·N", from `superzej new-tab`) resolve to their base tab's worktree.
/// Shared by `resolve-worktree` and `panel-snapshot`.
pub fn resolve_tab_worktree(session: &str, tab: &str) -> Option<String> {
    let db = Db::open().ok()?;
    if let Ok(Some(path)) = db.worktree_for_tab(session, tab) {
        return Some(path);
    }
    let base = crate::commands::new_tab::strip_page_suffix(tab);
    if base != tab {
        if let Ok(Some(path)) = db.worktree_for_tab(session, base) {
            return Some(path);
        }
    }
    None
}

/// Directory for a session/tab pair: worktree tabs (including ` ·N` pages)
/// resolve to their worktree path; `{slug}/home` tabs resolve to the registered
/// workspace repo path. This is broader than `resolve_tab_worktree` because
/// layout-scoped commands may run from home tabs too.
pub fn resolve_tab_dir(session: &str, tab: &str) -> Option<String> {
    let db = Db::open().ok()?;
    resolve_tab_dir_from_db(&db, session, tab).map(|p| p.to_string_lossy().into_owned())
}

fn resolve_tab_dir_from_db(db: &Db, session: &str, tab: &str) -> Option<PathBuf> {
    if let Ok(Some(path)) = db.worktree_for_tab(session, tab) {
        return Some(PathBuf::from(path));
    }
    let base = crate::commands::new_tab::strip_page_suffix(tab);
    if base != tab {
        if let Ok(Some(path)) = db.worktree_for_tab(session, base) {
            return Some(PathBuf::from(path));
        }
    }
    let slug = base.strip_suffix("/home")?;
    db.workspaces().ok()?.into_iter().find_map(|w| {
        let repo_path = Path::new(&w.repo_path);
        let mut base_slug = util::slugify(&repo::repo_name_from_path(repo_path));
        if base_slug.is_empty() {
            base_slug = "repo".to_string();
        }
        let stable_slug = db
            .slug_for_repo(&w.repo_path, &base_slug)
            .unwrap_or(base_slug);
        (stable_slug == slug).then(|| PathBuf::from(w.repo_path))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use std::path::{Path, PathBuf};

    #[test]
    fn resolve_tab_dir_covers_worktree_pages_home_and_misses() {
        let dbh = Db::open_memory().unwrap();
        dbh.put_workspace("/repos/app", "app").unwrap();
        dbh.put_worktree("app/feat", "/repos/app", "/wt/app-feat", "feat", None)
            .unwrap();
        let session = db::session();

        assert_eq!(
            resolve_tab_dir_from_db(&dbh, &session, "app/feat"),
            Some(PathBuf::from("/wt/app-feat"))
        );
        assert_eq!(
            resolve_tab_dir_from_db(&dbh, &session, "app/feat \u{b7}2"),
            Some(PathBuf::from("/wt/app-feat"))
        );
        assert_eq!(
            resolve_tab_dir_from_db(&dbh, &session, "app/home"),
            Some(Path::new("/repos/app").to_path_buf())
        );
        assert_eq!(resolve_tab_dir_from_db(&dbh, &session, "app/missing"), None);
    }
}
