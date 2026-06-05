//! `superzej new-workspace [path|url]` — open a repo as a workspace: its **home
//! tab** in the single superzej session. Directory-agnostic: prompts to pick a
//! repo (from repo_roots) or clone a URL when no target given.
//!
//! Inside our session we open-or-focus the repo's `{slug}/home` tab (a tab
//! switch — never a session teleport). From a plain terminal we cold-start the
//! one session, rooted at the repo (its first tab becomes that repo's home).

use crate::commands::attach;
use crate::config::Config;
use crate::db::Db;
use crate::{msg, picker, repo, util, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

fn is_url(s: &str) -> bool {
    s.contains("://") || (s.starts_with("git@") && s.contains(':'))
}

pub fn run(
    cfg: &Config,
    target: Option<String>,
    name: Option<String>,
    from_home: bool,
) -> Result<()> {
    let target = match target {
        Some(t) => t,
        None => {
            let picked = if from_home {
                picker::pick_dir_home(cfg)
            } else {
                picker::pick_repo(cfg)
            };
            match picked {
                Some(t) => t,
                None => {
                    msg::info("no repo selected");
                    return Ok(());
                }
            }
        }
    };

    let root: PathBuf = if is_url(&target) {
        let repo_name = util::basename(&target).trim_end_matches(".git").to_string();
        let dest = Path::new(&cfg.workspaces_dir).join(&repo_name);
        if dest.join(".git").is_dir() {
            msg::info(&format!("already cloned at {}", dest.display()));
        } else {
            std::fs::create_dir_all(&cfg.workspaces_dir)?;
            msg::info(&format!("cloning {target} -> {}", dest.display()));
            let ok = Command::new("git")
                .arg("clone")
                .arg(&target)
                .arg(&dest)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok {
                msg::die("clone failed");
            }
        }
        dest
    } else {
        repo::main_worktree(Path::new(&target))
            .unwrap_or_else(|| msg::die(&format!("'{target}' is not inside a git repository")))
    };

    let name = name.unwrap_or_else(|| repo::repo_name(&root));
    let root_s = root.to_string_lossy().into_owned();

    let db = Db::open()?;
    db.touch_repo(&root_s, &name)?; // record in history (launcher recents)
    db.put_workspace(&root_s, &name)?; // register the repo (shows in the sidebar)

    let slug = repo::repo_slug(&root);
    let home = repo::home_tab(&slug);

    if zellij::in_superzej_session() {
        // Already in our world: open-or-focus the repo's home TAB in the one
        // session. A tab switch — the sidebar/panel stay put, no teleport. The
        // home tab's `superzej status` pane registers the repo + names the tab.
        if zellij::tab_names().iter().any(|t| t == &home) {
            zellij::go_to_tab_name(&home);
        } else {
            zellij::new_tab(&home, &root, Some("home-tab"));
        }
        // Nudge the sidebar to re-pull (the repo may be newly registered).
        let url = crate::commands::panels::plugin_url("sidebar.wasm");
        zellij::pipe_plugin(&url, "superzej_refresh", "");
        Ok(())
    } else if std::env::var_os("SUPERZEJ_NO_EXEC").is_some() {
        // Scripting / tests: register without launching/affecting any session.
        msg::info(&format!("workspace '{name}' registered ({root_s})"));
        Ok(())
    } else {
        // From a plain (or foreign/leaked) terminal: cold-start the one superzej
        // session, rooted at this repo (its first tab becomes the repo's home).
        attach::cold_start(&zellij::ui_session(), &root);
    }
}
