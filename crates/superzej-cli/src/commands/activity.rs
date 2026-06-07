//! `superzej activity` (internal) — terminal-activity state per worktree, for
//! the sidebar's live dots.
//!
//! zellij's plugin API exposes no pane-content/activity signal, so activity is
//! measured host-side: every call scans `/proc` for processes whose cwd sits
//! under a managed worktree and sums their CPU time. A worktree whose CPU is
//! advancing is `active` (pulsing dot); one that was active and has gone quiet
//! is `quiet` (steady dot — "done, look at me"); focusing its tab acks it back
//! to neutral via `--ack`.
//!
//! The full state machine lives HERE (not in the plugin) because the sidebar
//! runs one instance per tab — plugin-local latches would diverge. State is
//! persisted in `~/.superzej/activity.json` (ephemeral, self-healing; kept out
//! of the sqlite DB so N sidebar instances polling every few seconds don't
//! contend on the WAL).
//!
//!   none ── cpu delta ≥ threshold ──▶ active
//!   active ── quiet ≥ grace ────────▶ quiet
//!   quiet ── new activity ──────────▶ active
//!   quiet ── --ack <tab> ───────────▶ acked   (renders like none; re-arms)
//!
//! stdout: one TSV row per managed worktree: `{tab}\t{state}\t{quiet_secs}`.
//! Never errors on scan problems — a partial or empty scan just trends quiet.

use crate::db::Db;
use crate::util;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// CPU per wall-second that counts as "working": 3 jiffies/s = 30ms/s ≈ 3% of
/// one core. Catches builds / model streaming / tool runs; ignores an idle
/// shell prompt. (CLK_TCK is hardcoded to the Linux default of 100 — wrong
/// only in exotic kernels, and then only as threshold sensitivity.)
const ACTIVE_JIFFIES_PER_SEC: f64 = 3.0;
/// An `active` worktree must stay below the threshold this long before it
/// turns `quiet` — damps flapping from scheduling gaps between close polls.
const QUIET_GRACE_SECS: f64 = 5.0;
/// Calls closer together than this reuse the previous scan (several sidebar
/// instances poll independently; only the first pays for the /proc walk).
const MIN_SCAN_INTERVAL_SECS: f64 = 1.0;

#[derive(Default, Serialize, Deserialize)]
struct Snapshot {
    #[serde(default)]
    version: u32,
    /// Unix seconds of the last *scan* (not ack).
    #[serde(default)]
    polled_at: f64,
    /// Keyed by worktree path.
    #[serde(default)]
    worktrees: BTreeMap<String, Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    tab: String,
    cpu_jiffies: u64,
    state: String, // "none" | "active" | "quiet" | "acked"
    #[serde(default)]
    quiet_since: Option<f64>,
    #[serde(default)]
    last_active_at: Option<f64>,
}

pub fn run(ack: Option<String>) -> Result<()> {
    let path = state_path();
    let mut snap = load(&path);

    if let Some(tab) = ack {
        for e in snap.worktrees.values_mut() {
            if e.tab == tab && e.state == "quiet" {
                e.state = "acked".into();
                e.quiet_since = None;
            }
        }
        save(&path, &snap);
        return Ok(());
    }

    let now = unix_now();
    if now - snap.polled_at >= MIN_SCAN_INTERVAL_SECS {
        poll(&mut snap, now);
        save(&path, &snap);
    }

    for e in snap.worktrees.values() {
        let quiet_secs = match (e.state.as_str(), e.quiet_since) {
            ("quiet", Some(t)) => (now - t).max(0.0) as u64,
            _ => 0,
        };
        crate::outln!("{}\t{}\t{}", e.tab, e.state, quiet_secs);
    }
    Ok(())
}

/// One scan + state-machine step over every managed worktree.
fn poll(snap: &mut Snapshot, now: f64) {
    // Managed worktrees (incl. home tabs) from the DB; without it, keep the
    // previous snapshot rather than erroring (the sidebar tolerates staleness).
    let managed = match Db::open().and_then(|db| db.worktrees()) {
        Ok(rows) => rows,
        Err(_) => return,
    };

    // Longest-prefix targets so a worktree nested under its repo root
    // (worktree_mode = "in_repo") wins over the home tab.
    let mut targets: Vec<(PathBuf, String, String)> = managed
        .iter()
        .map(|w| {
            (
                PathBuf::from(&w.worktree),
                w.worktree.clone(),
                w.tab_name.clone(),
            )
        })
        .collect();
    targets.sort_by_key(|(p, _, _)| std::cmp::Reverse(p.as_os_str().len()));

    let jiffies = scan_proc(&targets);

    let wall = (now - snap.polled_at).max(0.0);
    let first_poll = snap.polled_at == 0.0;
    let threshold = ACTIVE_JIFFIES_PER_SEC * wall;

    let mut next: BTreeMap<String, Entry> = BTreeMap::new();
    for w in &managed {
        let cur = jiffies.get(&w.worktree).copied().unwrap_or(0);
        let prev = snap.worktrees.get(&w.worktree);
        let mut e = prev.cloned().unwrap_or(Entry {
            tab: w.tab_name.clone(),
            cpu_jiffies: cur,
            state: "none".into(),
            quiet_since: None,
            last_active_at: None,
        });
        e.tab = w.tab_name.clone(); // tab renames follow the DB

        // A first sighting (or first-ever poll) records a baseline; deltas
        // only mean something from the second reading on.
        if prev.is_some() && !first_poll {
            let delta = cur.saturating_sub(e.cpu_jiffies) as f64;
            if delta >= threshold && wall > 0.0 {
                e.state = "active".into();
                e.quiet_since = None;
                e.last_active_at = Some(now);
            } else if e.state == "active"
                && now - e.last_active_at.unwrap_or(0.0) >= QUIET_GRACE_SECS
            {
                e.state = "quiet".into();
                e.quiet_since = Some(now);
            }
        }
        e.cpu_jiffies = cur;
        next.insert(w.worktree.clone(), e);
    }

    snap.version = 1;
    snap.polled_at = now;
    snap.worktrees = next; // worktrees gone from the DB are pruned here
}

/// Sum utime+stime jiffies per managed worktree for every process whose cwd
/// is under it. Unreadable PIDs (races, permissions) are skipped silently.
#[cfg(target_os = "linux")]
fn scan_proc(targets: &[(PathBuf, String, String)]) -> BTreeMap<String, u64> {
    let mut sums: BTreeMap<String, u64> = BTreeMap::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return sums;
    };
    for ent in proc_dir.flatten() {
        let name = ent.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) else {
            continue;
        };
        let Some((_, wt, _)) = targets.iter().find(|(p, _, _)| cwd.starts_with(p)) else {
            continue;
        };
        if let Some(j) = stat_jiffies(Path::new("/proc").join(pid).join("stat")) {
            *sums.entry(wt.clone()).or_insert(0) += j;
        }
    }
    sums
}

/// TODO: a `ps`-based fallback for macOS; until then every worktree is `none`.
#[cfg(not(target_os = "linux"))]
fn scan_proc(_targets: &[(PathBuf, String, String)]) -> BTreeMap<String, u64> {
    BTreeMap::new()
}

/// utime+stime from /proc/PID/stat. comm (field 2) may contain spaces and
/// parens, so parse from the LAST ')' — after it, fields resume at 3 (state),
/// so utime/stime (fields 14/15) are tokens 11/12.
#[cfg(target_os = "linux")]
fn stat_jiffies(path: PathBuf) -> Option<u64> {
    let s = std::fs::read_to_string(path).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    let mut it = rest.split_whitespace().skip(11);
    let utime: u64 = it.next()?.parse().ok()?;
    let stime: u64 = it.next()?.parse().ok()?;
    Some(utime + stime)
}

fn state_path() -> PathBuf {
    util::superzej_dir().join("activity.json")
}

fn load(path: &Path) -> Snapshot {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic-ish write (tmp + rename) so concurrent sidebar instances never read
/// a torn file; a lost race just means one redundant scan next poll.
fn save(path: &Path, snap: &Snapshot) {
    let Ok(json) = serde_json::to_string(snap) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
