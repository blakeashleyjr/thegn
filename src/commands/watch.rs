//! `superzej watch --session <s>` — the per-session background daemon that keeps
//! the diff/PR panel live. It follows the focused worktree (written to a focus
//! file by `panel-snapshot`) and:
//!   - filesystem-watches that worktree (inotify via `notify`), debounces, and on
//!     any change recomputes the diff and pushes `superzej_diff` to the panel —
//!     so edits, commits and checkouts refresh the panel instantly; and
//!   - on an interval re-fetches the PR state and pushes `superzej_pr` (with the
//!     same rate-limit back-off the old `pr watch` used).
//!
//! Both pushes also write the SQLite caches (`diff_cache` / `pr_cache`) so the
//! next `panel-snapshot` paints instantly. One daemon per session: a pid
//! lockfile makes the auto-spawn from `attach` idempotent.

use crate::commands::{diff, panels};
use crate::config::Config;
use crate::db::Db;
use crate::github::{self, PanelState};
use crate::remote::GitLoc;
use crate::{util, zellij};
use anyhow::Result;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::Duration;

/// How often the loop wakes to re-check focus and the PR interval.
const POLL: Duration = Duration::from_millis(500);
/// Quiet window after the last fs event before we recompute the diff.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// The file `panel-snapshot` writes the focused worktree path to, and this
/// daemon reads to know what to watch. Lives in the private socket dir, so a
/// sandboxed test instance gets its own.
pub fn focus_path(session: &str) -> PathBuf {
    zellij::socket_dir().join(format!("{session}.focus"))
}

/// Record the currently-focused worktree for the watch daemon (called by
/// `panel-snapshot` on every tab focus change).
pub fn write_focus(session: &str, worktree: &str) {
    let p = focus_path(session);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, worktree);
}

fn read_focus(session: &str) -> Option<String> {
    std::fs::read_to_string(focus_path(session))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn lock_path(session: &str) -> PathBuf {
    zellij::socket_dir().join(format!("{session}.watch.pid"))
}

/// One daemon per session: refuse to start if a live daemon already holds the
/// lock (a dead holder — no `/proc/<pid>` — is treated as stale and replaced).
fn acquire_lock(session: &str) -> bool {
    let p = lock_path(session);
    if let Ok(s) = std::fs::read_to_string(&p) {
        if let Ok(pid) = s.trim().parse::<i32>() {
            if Path::new(&format!("/proc/{pid}")).exists() {
                return false;
            }
        }
    }
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    std::fs::write(&p, std::process::id().to_string()).is_ok()
}

pub fn run(cfg: &Config, session: Option<String>, pr_interval: Option<u64>) -> Result<()> {
    let session = session.unwrap_or_else(zellij::ui_session);
    if !acquire_lock(&session) {
        return Ok(()); // a live daemon already owns this session
    }
    let url = panels::plugin_url("panel.wasm");
    let base = pr_interval.unwrap_or(cfg.watch.pr_interval_secs).max(1) as i64;

    let (tx, rx) = channel::<()>();
    let mut watcher: Option<RecommendedWatcher> = None;
    let mut current: Option<String> = None;
    let mut last_pr: i64 = 0;
    let mut pr_delay = base;

    loop {
        // Re-target the fs watcher when focus changes.
        let focus = read_focus(&session);
        if focus != current {
            current = focus.clone();
            watcher = None; // drop the old watch
            if let Some(wt) = current.as_deref() {
                if Path::new(wt).is_dir() {
                    watcher = make_watcher(tx.clone(), wt);
                    push_diff(&url, wt); // immediate paint for the new focus
                    last_pr = 0; // force a PR refresh for the new worktree
                }
            }
        }

        // Interval PR refresh for the focused worktree, with rate-limit back-off.
        if let Some(wt) = current.as_deref() {
            if util::now() - last_pr >= pr_delay {
                let rate_limited = push_pr(&url, wt);
                last_pr = util::now();
                pr_delay = if rate_limited {
                    (pr_delay.saturating_mul(2)).min(base.saturating_mul(8))
                } else {
                    base
                };
            }
        }

        // `watcher` is a kept-alive RAII guard (dropping it stops the watch);
        // borrow it so it isn't flagged as write-only.
        let _ = &watcher;

        // Wait for fs events (debounced) or wake on POLL to re-check focus.
        match rx.recv_timeout(POLL) {
            Ok(()) => {
                // Coalesce a burst: keep draining until quiet for DEBOUNCE.
                while let Ok(()) = rx.recv_timeout(DEBOUNCE) {}
                if let Some(wt) = current.as_deref() {
                    push_diff(&url, wt);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// A recursive watcher on `wt` that forwards relevant change events to `tx`,
/// filtering out high-churn paths (object stores, build output).
fn make_watcher(tx: Sender<()>, wt: &str) -> Option<RecommendedWatcher> {
    let mut w = recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            if is_relevant(&ev) {
                let _ = tx.send(());
            }
        }
    })
    .ok()?;
    w.watch(Path::new(wt), RecursiveMode::Recursive).ok()?;
    Some(w)
}

/// Ignore events that don't change the diff: git's object/pack store, reflogs,
/// and common build-output dirs. Keep `.git/HEAD`, `.git/index`, `.git/refs`
/// (commits, checkouts, branch switches) and all worktree files.
fn is_relevant(ev: &Event) -> bool {
    ev.paths.iter().any(|p| {
        let s = p.to_string_lossy();
        !(s.contains("/.git/objects/")
            || s.contains("/.git/lfs/")
            || s.contains("/.git/logs/")
            || s.contains("/.git/fsmonitor")
            || s.contains("/target/")
            || s.contains("/node_modules/"))
    })
}

/// Recompute the diff for `wt`, cache it, and push it to the panel.
fn push_diff(url: &str, wt: &str) {
    let tsv = diff::files_for(Path::new(wt));
    if let Ok(db) = Db::open() {
        let _ = db.put_diff_cache(wt, &tsv);
    }
    let payload = serde_json::json!({ "worktree": wt, "files": tsv }).to_string();
    zellij::pipe_plugin(url, "superzej_diff", &payload);
}

/// Re-fetch the PR for `wt`, cache it, push it; returns whether we were rate
/// limited (so the caller can back off).
fn push_pr(url: &str, wt: &str) -> bool {
    let panel = github::pr_status(&GitLoc::for_worktree(Path::new(wt)));
    let rate_limited = matches!(panel.state, PanelState::RateLimited);
    let json = serde_json::to_string(&panel).unwrap_or_default();
    if let Ok(db) = Db::open() {
        let _ = db.put_pr_cache(wt, &panel.branch, &json);
    }
    zellij::pipe_plugin(url, "superzej_pr", &json);
    rate_limited
}
