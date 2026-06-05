//! `superzej open-worktree --path <wt>` — (internal) open an EXISTING managed
//! worktree as a zellij tab. The sidebar calls this when a worktree row that has
//! no live tab is selected: switch to its `{slug}/{branch}` tab if one is already
//! open, otherwise create it (the worktree-tab layout). Mirrors the non-in-place
//! open path in `new_worktree::run`, minus the git-worktree creation.

use crate::{msg, repo, util, zellij};
use anyhow::Result;
use std::path::Path;

pub fn run(path: String) -> Result<()> {
    let wt = Path::new(&path);
    if !wt.is_dir() {
        msg::warn(&format!("worktree path does not exist: {path}"));
        return Ok(());
    }
    let tab = tab_name(wt);

    if zellij::in_zellij() {
        if zellij::tab_names().iter().any(|t| t == &tab) {
            zellij::go_to_tab_name(&tab);
        } else if !zellij::new_tab(&tab, wt, Some("worktree-tab")) {
            zellij::new_tab(&tab, wt, None);
        }
    } else {
        msg::info(&format!("(not in zellij) worktree ready at {path}"));
    }
    Ok(())
}

/// The `{repo_slug}/{slugify(branch)}` tab name for a worktree, matching
/// `new_worktree`'s naming (so an already-open tab is found, not duplicated).
fn tab_name(wt: &Path) -> String {
    let root = repo::main_worktree(wt).unwrap_or_else(|| wt.to_path_buf());
    let slug = repo::repo_slug(&root);
    let branch = util::git_out(wt, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|| "detached".to_string());
    repo::branch_tab(&slug, &branch)
}
