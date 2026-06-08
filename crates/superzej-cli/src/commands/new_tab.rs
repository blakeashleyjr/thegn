//! `superzej new-tab` — open a SECOND full-chrome tab on the current worktree
//! (Alt+t / tab-mode `n` pipe `superzej_new_tab` to the tabbar plugin, which
//! runs this — no spawned command pane, no floating flash). The tab is named
//! `{base} ·N` (`{slug}/{branch} ·2`, `·3`, …) so the tabbar lists it next to
//! its worktree; the center pane is a plain shell (worktree-tab-extra layout).
//! No worktree is created and no DB row is written — closing the tab is just
//! closing a tab. The diff/PR panel resolves `·N` tabs by stripping the suffix
//! (see resolve.rs).
//!
//! Concurrency: the pipe broadcast makes EVERY per-tab tabbar instance run
//! this (zellij starves background-tab plugins of Tab/Pane updates, so no
//! instance-side "am I the focused tab" guard can be trusted). The focused tab
//! is therefore resolved HERE via `dump-layout` (always fresh, straight from
//! the server) and a stamp+lockfile collapses the burst to one tab.
//!
//! `--session` (the tabbar pipe path): plugin-spawned commands can't rely on
//! env/cwd, so the session comes as an arg and the directory is resolved from
//! the DB / dump-layout. Without it (the Cmd+K palette, which runs in a
//! floating pane WITH env/cwd) the worktree is derived from the cwd if the
//! focused tab can't be resolved.

use crate::db;
use crate::{msg, repo, util, zellij};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Concurrent pipe-burst invocations arrive within this window; later
/// deliberate re-presses take longer than it.
const BURST_MS: u64 = 800;

pub fn run(session: Option<String>) -> Result<()> {
    if let Some(s) = &session {
        // Plugin-spawned: target the right session for `zellij action` (and
        // satisfy in_zellij()).
        std::env::set_var("ZELLIJ_SESSION_NAME", s);
    }
    if !zellij::in_zellij() {
        msg::die("new-tab only works inside the superzej session");
    }
    if !claim_burst() {
        return Ok(()); // a sibling pipe invocation already created the tab
    }

    let (base, dir) = match zellij::focused_tab_name() {
        Some(t) => {
            let base = strip_page_suffix(&t).to_string();
            let session_name = session
                .as_deref()
                .map(str::to_string)
                .unwrap_or_else(db::session);
            match crate::commands::resolve::resolve_tab_dir(&session_name, &base).map(PathBuf::from)
            {
                Some(dir) => (base, dir),
                None => cwd_base()?, // ad-hoc tab with no DB row
            }
        }
        None => cwd_base()?,
    };

    let name = next_free_name(&base, &zellij::tab_names());
    msg::info(&format!("opening tab {name}"));
    if !zellij::new_tab(&name, &dir, Some("worktree-tab-extra")) {
        zellij::new_tab(&name, &dir, None);
    }
    Ok(())
}

/// One winner per pipe burst: a fresh stamp rejects siblings, the O_EXCL
/// lockfile serializes the stamp check-and-write itself (a crashed holder's
/// lock counts as stale after 5s).
fn claim_burst() -> bool {
    let dir = util::superzej_dir();
    let _ = std::fs::create_dir_all(&dir);
    let age = |p: &Path| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .unwrap_or(Duration::MAX)
    };
    let stamp = dir.join(".newtab_stamp");
    if age(&stamp) < Duration::from_millis(BURST_MS) {
        return false;
    }
    let lock = dir.join(".newtab_lock");
    let acquire = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .is_ok()
    };
    let got = acquire()
        || (age(&lock) > Duration::from_secs(5) && {
            let _ = std::fs::remove_file(&lock);
            acquire()
        });
    if !got {
        return false;
    }
    let _ = std::fs::write(&stamp, b"");
    let _ = std::fs::remove_file(&lock);
    true
}

/// Base tab name + worktree dir from the cwd (the palette fallback).
fn cwd_base() -> Result<(String, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let Some(top) = repo::toplevel(&cwd) else {
        msg::die("not inside a git repository — open a workspace first");
    };
    let Some(main) = repo::main_worktree(&cwd) else {
        msg::die("could not resolve the repo's main worktree");
    };
    let slug = repo::repo_slug(&main);
    let base = if top == main {
        repo::home_tab(&slug)
    } else {
        let branch = util::git_out(&top, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .unwrap_or_else(|| "detached".into());
        repo::branch_tab(&slug, &branch)
    };
    Ok((base, top))
}

/// Lowest free `"{base} ·N"` (N ≥ 2) among the existing tab names.
fn next_free_name(base: &str, tabs: &[String]) -> String {
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{base} \u{b7}{n}");
        if !tabs.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Strip a `" ·N"` page suffix off a tab name (the panel resolves extra tabs
/// to the same worktree as their base tab).
pub fn strip_page_suffix(tab: &str) -> &str {
    let Some((base, suffix)) = tab.rsplit_once(" \u{b7}") else {
        return tab;
    };
    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
        base
    } else {
        tab
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_first_free_page_number() {
        let tabs = vec!["r/b".to_string(), "r/b ·2".to_string()];
        assert_eq!(next_free_name("r/b", &tabs), "r/b ·3");
        assert_eq!(next_free_name("r/x", &tabs), "r/x ·2");
    }

    #[test]
    fn strips_only_real_page_suffixes() {
        assert_eq!(strip_page_suffix("r/b ·2"), "r/b");
        assert_eq!(strip_page_suffix("r/b ·12"), "r/b");
        assert_eq!(strip_page_suffix("r/b"), "r/b");
        assert_eq!(strip_page_suffix("r/b ·x"), "r/b ·x");
        assert_eq!(strip_page_suffix("r/b ·"), "r/b ·");
    }
}
