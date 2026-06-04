//! `superzej new-workspace [path|url]` — open a repo as a tab. Directory-agnostic:
//! prompts to pick a repo (from repo_roots) or clone a URL when no target given.

use crate::config::Config;
use crate::db::Db;
use crate::{msg, picker, repo, util, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

fn is_url(s: &str) -> bool {
    s.contains("://") || (s.starts_with("git@") && s.contains(':'))
}

pub fn run(cfg: &Config, target: Option<String>, name: Option<String>) -> Result<()> {
    let target = match target {
        Some(t) => t,
        None => match picker::pick_repo(cfg) {
            Some(t) => t,
            None => {
                msg::info("no repo selected");
                return Ok(());
            }
        },
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
    db.put_tab(&name, &root_s)?;

    if zellij::in_zellij() {
        if !zellij::new_tab(&name, &root, Some("workspace-tab")) {
            zellij::new_tab(&name, &root, None);
        }
    } else {
        msg::info(&format!(
            "(not in zellij) workspace '{name}' registered at {root_s}"
        ));
    }
    Ok(())
}
