//! `superzej launch` (bare `sj`) — the entry point, run from anywhere.
//!
//! Flow:
//!   1. If the cwd is a git repo: already a superzej workspace -> just open it;
//!      otherwise offer to add it.
//!   2. Otherwise fall back to the recents/discovered menu; if there are none,
//!      run the first-time wizard.
//!
//! Opening a workspace switches sessions only when we're already inside a
//! superzej-managed session; otherwise it cold-starts a fresh, isolated session
//! (stripping inherited zellij env) so it never hijacks a foreign session.

use crate::commands;
use crate::config::Config;
use crate::db::Db;
use crate::{msg, picker, repo, util, zellij};
use anyhow::Result;

const ADD_NEW: &str = "+ add a new repo…";
const SHELL_HERE: &str = "· shell here";

pub fn run(cfg: &Config) -> Result<()> {
    let db = Db::open()?;

    std::thread::spawn(|| {
        if let Ok(db) = Db::open() {
            if let Ok(wts) = db.worktrees() {
                let paths: Vec<String> = wts.into_iter().map(|w| w.worktree).collect();
                let _ = superzej_core::sandbox::run_gc(&paths);
            }
        }
    });

    // 1. The cwd is a git repo?
    let cwd = std::env::current_dir().unwrap_or_else(|_| util::home());
    if let Some(root) = repo::main_worktree(&cwd) {
        let root_s = root.to_string_lossy().into_owned();
        if db.is_known_repo(&root_s)? {
            // Already added — just open/resume it.
            return commands::new_workspace::run(cfg, Some(root_s), None, false);
        }
        let name = repo::repo_name(&root);
        if commands::confirm(&format!(
            "'{name}' isn't a superzej workspace yet — add it?"
        )) {
            return commands::new_workspace::run(cfg, Some(root_s), None, false);
        }
        // Declined: fall through to the menu.
    }

    // 2. Not a git repo (or declined): existing repos, else the wizard.
    let recents = db.recent_repos(20)?;
    if recents.is_empty() && repo::discover_repos(cfg).is_empty() {
        return wizard(cfg);
    }
    menu(cfg)
}

/// The recents picker. One-shot inside a superzej session (a switch happens);
/// loops otherwise (opening a repo cold-starts and never returns) until quit.
fn menu(cfg: &Config) -> Result<()> {
    loop {
        let mut options = Db::open()?.recent_repos(20)?;
        options.push(ADD_NEW.to_string());
        options.push(SHELL_HERE.to_string());

        match picker::pick("superzej — open a repo", &options, cfg.picker.as_str()) {
            None => util::exec_shell(),
            Some(c) if c == SHELL_HERE => util::exec_shell(),
            Some(c) if c == ADD_NEW => {
                if let Some(t) = picker::pick_repo(cfg) {
                    commands::new_workspace::run(cfg, Some(t), None, false)?;
                }
            }
            Some(c) => commands::new_workspace::run(cfg, Some(c), None, false)?,
        }

        if zellij::in_superzej_session() {
            return Ok(());
        }
    }
}

/// First-time setup: no repos known yet, so help add the first one.
fn wizard(cfg: &Config) -> Result<()> {
    msg::info("welcome to superzej — let's add your first repo.");
    let options = vec![
        "Clone a repo from a URL".to_string(),
        "Add a local repo by path".to_string(),
        "Quit".to_string(),
    ];
    match picker::pick("superzej setup", &options, cfg.picker.as_str()).as_deref() {
        Some("Clone a repo from a URL") => {
            if let Some(url) = picker::prompt("Git URL (git@github.com:org/repo.git)") {
                return commands::new_workspace::run(cfg, Some(url), None, false);
            }
        }
        Some("Add a local repo by path") => {
            if let Some(path) = picker::prompt("Path to a local git repo") {
                return commands::new_workspace::run(
                    cfg,
                    Some(util::expand_tilde(&path)),
                    None,
                    false,
                );
            }
        }
        _ => {}
    }
    Ok(())
}
