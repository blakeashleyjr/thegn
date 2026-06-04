//! `superzej new-pane` — create a worktree for the current tab's repo and open a
//! pane in it that runs the agent picker. `--in-place` becomes the picker in this
//! pane (used by the workspace-tab layout).

use crate::config::Config;
use crate::db::Db;
use crate::{commands, msg, repo, worktree, zellij};
use anyhow::Result;

pub fn run(
    cfg: &Config,
    name: Option<String>,
    base: Option<String>,
    dir: &str,
    in_place: bool,
) -> Result<()> {
    // Resurrection guard: if we're already a worktree pane, do nothing.
    if let Ok(wt) = std::env::var("SUPERZEJ_WORKTREE") {
        if std::path::Path::new(&wt).is_dir() {
            msg::warn("already inside a superzej worktree; ignoring new-pane");
            return Ok(());
        }
    }

    let cwd = std::env::current_dir()?;
    let root = repo::main_worktree(&cwd).unwrap_or_else(|| {
        msg::die("not inside a git repository — open a workspace first (superzej new-workspace)")
    });

    let base = base.unwrap_or_else(|| worktree::resolve_base(&root, cfg));
    let branch = worktree::branch_name(&root, name.as_deref(), cfg);
    let path = worktree::worktree_path(&root, &branch, cfg);

    msg::info(&format!("creating worktree {branch} off {base}"));
    worktree::add(&root, &branch, &base, &path, cfg);

    let tab = std::env::var("SUPERZEJ_TAB").unwrap_or_else(|_| repo::repo_name(&root));
    let path_s = path.to_string_lossy().into_owned();
    let db = Db::open()?;
    db.put_worktree(&tab, &root.to_string_lossy(), &path_s, &branch)?;

    if in_place {
        std::env::set_current_dir(&path)?;
        // Become the picker in this very pane (no leftover exited pane).
        return commands::pick_agent::run(cfg, Some(path_s), Some(branch), None);
    }

    if zellij::in_zellij() {
        zellij::new_pane(
            &path,
            &branch,
            dir,
            &[
                "superzej",
                "pick-agent",
                "--worktree",
                &path_s,
                "--branch",
                &branch,
            ],
        );
    } else {
        msg::info(&format!("(not in zellij) worktree ready at {path_s}"));
    }
    Ok(())
}
