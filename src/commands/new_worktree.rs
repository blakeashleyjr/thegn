//! `superzej new-worktree` — create a git worktree for a repo and open it as a
//! new zellij *tab* (named `{repo_slug}/{branch}`) whose first pane prompts for
//! what to run (the agent picker). `--in-place` runs that picker in the current
//! pane (the worktree-tab layout). `--repo <path>` targets a specific repo (the
//! sidebar's "+ worktree"); otherwise the current tab's repo is used. All tabs
//! live in the one session, so this is always a plain `new-tab` + tab switch.

use crate::config::Config;
use crate::db::Db;
use crate::{commands, msg, repo, util, worktree, zellij};
use anyhow::Result;
use std::path::Path;

pub fn run(
    cfg: &Config,
    name: Option<String>,
    base: Option<String>,
    in_place: bool,
    repo_arg: Option<String>,
) -> Result<()> {
    // Resolve the target repo root: an explicit `--repo` (sidebar "+ worktree"),
    // else the repo of the current tab's cwd.
    let root = if let Some(r) = repo_arg.as_deref() {
        repo::main_worktree(Path::new(r))
            .unwrap_or_else(|| msg::die(&format!("'{r}' is not inside a git repository")))
    } else {
        // Resurrection guard (cwd path only): if we're already inside a worktree,
        // do nothing — prevents the worktree-tab pane recursing on resurrection.
        if let Ok(wt) = std::env::var("SUPERZEJ_WORKTREE") {
            if Path::new(&wt).is_dir() {
                msg::warn("already inside a superzej worktree; ignoring new-worktree");
                return Ok(());
            }
        }
        let cwd = std::env::current_dir()?;
        repo::main_worktree(&cwd).unwrap_or_else(|| {
            msg::die(
                "not inside a git repository — open a workspace first (superzej new-workspace)",
            )
        })
    };

    let base = base.unwrap_or_else(|| worktree::resolve_base(&root, cfg));

    // A base with no commits (fresh repo on an unborn branch) can't be branched
    // from. Bail cleanly into a shell instead of dumping a raw git error.
    if util::git_out(&root, &["rev-parse", "--verify", "--quiet", &base]).is_none() {
        msg::warn(&format!(
            "'{base}' has no commits yet — make an initial commit in this repo, then press Alt-w."
        ));
        return fallback(&root, in_place);
    }

    let slug = repo::repo_slug(&root);
    let branch = worktree::branch_name(&root, name.as_deref(), cfg);
    let path = worktree::worktree_path(&root, &branch, cfg);
    let tab = repo::branch_tab(&slug, &branch);

    msg::info(&format!("creating worktree {branch} off {base}"));
    if !worktree::add(&root, &branch, &base, &path, cfg) {
        msg::warn("could not create the worktree (see the git error above).");
        return fallback(&root, in_place);
    }

    let path_s = path.to_string_lossy().into_owned();
    let db = Db::open()?;
    db.put_worktree(&tab, &root.to_string_lossy(), &path_s, &branch)?;

    if in_place {
        std::env::set_current_dir(&path)?;
        // This pane *is* the worktree tab's first pane: name the tab, then become
        // the picker (no leftover exited pane).
        if zellij::in_zellij() {
            zellij::rename_tab(&tab);
        }
        return commands::pick_agent::run(cfg, Some(path_s), Some(branch), None);
    }

    if zellij::in_zellij() {
        // Open a new tab in the worktree (a tab switch in the one session); its
        // layout pane runs pick-agent.
        if !zellij::new_tab(&tab, &path, Some("worktree-tab")) {
            zellij::new_tab(&tab, &path, None);
        }
    } else {
        msg::info(&format!("(not in zellij) worktree ready at {path_s}"));
    }
    Ok(())
}

/// When a worktree can't be created, keep an in-place pane usable by dropping to
/// a shell in the repo root (so it isn't a dead, exited box).
fn fallback(root: &std::path::Path, in_place: bool) -> Result<()> {
    if in_place {
        std::env::set_current_dir(root)?;
        util::exec_shell();
    }
    Ok(())
}
