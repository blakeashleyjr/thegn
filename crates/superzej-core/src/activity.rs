//! Host-side terminal-activity state machine for the sidebar's live dots.
//!
//! Activity is measured by scanning `/proc` for processes whose cwd sits under
//! a managed worktree and summing their CPU time. A worktree whose CPU is
//! advancing is `active` (pulsing dot); one that was active and has gone quiet
//! is `quiet` (steady dot — "done, look at me"); focusing its tab acks it back
//! to neutral via [`ack`].
//!
//!   none ── cpu delta ≥ threshold ──▶ active
//!   active ── quiet ≥ grace ────────▶ quiet
//!   quiet ── new activity ──────────▶ active
//!   quiet ── ack(tab) ──────────────▶ acked   (renders like none; re-arms)
//!
//! State persists in `~/.superzej/activity.json` (ephemeral, self-healing; kept
//! out of the SQLite DB so frequent polling never contends on the WAL). This
//! used to be the `superzej activity` CLI command; the native host now owns the
//! FSM in-process. Never errors on scan problems — a partial/empty scan just
//! trends quiet.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// CPU per wall-second that counts as "working": 3 jiffies/s = 30ms/s ≈ 3% of
/// one core. Catches builds / model streaming / tool runs; ignores an idle
/// shell prompt. (CLK_TCK hardcoded to the Linux default of 100.)
const ACTIVE_JIFFIES_PER_SEC: f64 = 3.0;
/// An `active` worktree must stay below the threshold this long before it turns
/// `quiet` — damps flapping from scheduling gaps between close polls.
const QUIET_GRACE_SECS: f64 = 5.0;
/// Polls closer together than this reuse the previous scan.
const MIN_SCAN_INTERVAL_SECS: f64 = 1.0;

/// A managed worktree the scanner should track: `(path, tab_name)`.
#[derive(Debug, Clone)]
pub struct ManagedWorktree {
    pub worktree: String,
    pub tab: String,
}

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
    #[serde(default)]
    cpu_jiffies: u64,
    state: String, // "none" | "active" | "quiet" | "acked"
    #[serde(default)]
    quiet_since: Option<f64>,
    #[serde(default)]
    last_active_at: Option<f64>,
}

/// Path to the activity snapshot.
fn state_path() -> PathBuf {
    crate::util::superzej_dir().join("activity.json")
}

/// Read the latest activity states as `tab_name -> state` (`"active"`,
/// `"quiet"`, `"none"`, `"acked"`). Empty on any read/parse failure.
pub fn read_states() -> BTreeMap<String, String> {
    read_states_at(&state_path())
}

/// [`read_states`] against an explicit snapshot path (testable, no global env).
pub fn read_states_at(path: &Path) -> BTreeMap<String, String> {
    load(path)
        .worktrees
        .into_values()
        .map(|e| (e.tab, e.state))
        .collect()
}

/// Advance the FSM one step over `managed` and persist. Cheap to call on a
/// timer; skips the `/proc` walk if the last scan was under a second ago.
pub fn poll_and_save(managed: &[ManagedWorktree]) {
    poll_and_save_at(&state_path(), managed, unix_now());
}

/// [`poll_and_save`] against an explicit path/clock (testable).
pub fn poll_and_save_at(path: &Path, managed: &[ManagedWorktree], now: f64) {
    let mut snap = load(path);
    if now - snap.polled_at < MIN_SCAN_INTERVAL_SECS {
        return;
    }
    poll(&mut snap, managed, now);
    save(path, &snap);
}

/// Ack a worktree's tab: a `quiet` "look at me" dot clears once the user is on
/// the tab. No-op if the tab isn't quiet.
pub fn ack(tab: &str) {
    ack_at(&state_path(), tab);
}

/// [`ack`] against an explicit path (testable).
pub fn ack_at(path: &Path, tab: &str) {
    let mut snap = load(path);
    let mut changed = false;
    for e in snap.worktrees.values_mut() {
        if e.tab == tab && e.state == "quiet" {
            e.state = "acked".into();
            e.quiet_since = None;
            changed = true;
        }
    }
    if changed {
        save(path, &snap);
    }
}

/// One scan + state-machine step over every managed worktree.
fn poll(snap: &mut Snapshot, managed: &[ManagedWorktree], now: f64) {
    // Longest-prefix targets so a worktree nested under its repo root
    // (worktree_mode = "in_repo") wins over the home tab.
    let mut targets: Vec<(PathBuf, String)> = managed
        .iter()
        .map(|w| (PathBuf::from(&w.worktree), w.worktree.clone()))
        .collect();
    targets.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));

    let jiffies = scan_proc(&targets);

    let wall = (now - snap.polled_at).max(0.0);
    let first_poll = snap.polled_at == 0.0;
    let threshold = ACTIVE_JIFFIES_PER_SEC * wall;

    let mut next: BTreeMap<String, Entry> = BTreeMap::new();
    for w in managed {
        let cur = jiffies.get(&w.worktree).copied().unwrap_or(0);
        let prev = snap.worktrees.get(&w.worktree);
        let mut e = prev.cloned().unwrap_or(Entry {
            tab: w.tab.clone(),
            cpu_jiffies: cur,
            state: "none".into(),
            quiet_since: None,
            last_active_at: None,
        });
        e.tab = w.tab.clone(); // tab renames follow the caller

        // A first sighting (or first-ever poll) records a baseline; deltas only
        // mean something from the second reading on.
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
    snap.worktrees = next; // worktrees gone from the caller are pruned here
}

/// Sum utime+stime jiffies per managed worktree for every process whose cwd is
/// under it. Unreadable PIDs (races, permissions) are skipped silently.
#[cfg(target_os = "linux")]
fn scan_proc(targets: &[(PathBuf, String)]) -> BTreeMap<String, u64> {
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
        let Some((_, wt)) = targets.iter().find(|(p, _)| cwd.starts_with(p)) else {
            continue;
        };
        if let Some(j) = stat_jiffies(Path::new("/proc").join(pid).join("stat")) {
            *sums.entry(wt.clone()).or_insert(0) += j;
        }
    }
    sums
}

#[cfg(not(target_os = "linux"))]
fn scan_proc(_targets: &[(PathBuf, String)]) -> BTreeMap<String, u64> {
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

fn load(path: &Path) -> Snapshot {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Atomic-ish write (tmp + rename) so concurrent readers never see a torn file.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sz-act-{tag}-{}.json", std::process::id()))
    }

    #[test]
    fn missing_file_is_empty() {
        let path = tmp("missing");
        let _ = std::fs::remove_file(&path);
        assert!(read_states_at(&path).is_empty());
    }

    #[test]
    fn parses_tab_states_from_disk() {
        let path = tmp("parse");
        let json = r#"{"worktrees":{"/wt/a":{"tab":"app/home","state":"active","cpu_jiffies":0},
                        "/wt/b":{"tab":"app/feat","state":"quiet","cpu_jiffies":0}}}"#;
        std::fs::write(&path, json).unwrap();
        let m = read_states_at(&path);
        assert_eq!(m.get("app/home").map(String::as_str), Some("active"));
        assert_eq!(m.get("app/feat").map(String::as_str), Some("quiet"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn garbled_file_is_empty() {
        let path = tmp("bad");
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(read_states_at(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_states_uses_default_path_without_panicking() {
        let _ = read_states();
    }

    #[test]
    fn poll_records_baseline_then_quiet_then_ack() {
        let path = tmp("fsm");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt".into(),
            tab: "app/home".into(),
        }];
        // First poll: baseline, state "none".
        poll_and_save_at(&path, &managed, 1000.0);
        assert_eq!(
            read_states_at(&path).get("app/home").map(String::as_str),
            Some("none")
        );
        // No CPU advance (path doesn't exist) → stays none/quiet, never panics.
        poll_and_save_at(&path, &managed, 1100.0);
        let st = read_states_at(&path);
        assert!(st.contains_key("app/home"));

        // Manually mark quiet, then ack clears it to acked.
        let mut snap = load(&path);
        if let Some(e) = snap.worktrees.values_mut().next() {
            e.state = "quiet".into();
            e.quiet_since = Some(1100.0);
        }
        save(&path, &snap);
        ack_at(&path, "app/home");
        assert_eq!(
            read_states_at(&path).get("app/home").map(String::as_str),
            Some("acked")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn poll_skips_when_called_too_soon() {
        let path = tmp("skip");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/x".into(),
            tab: "t".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);
        // < MIN_SCAN_INTERVAL_SECS later: no rescan, snapshot unchanged.
        poll_and_save_at(&path, &managed, 1000.5);
        assert_eq!(read_states_at(&path).len(), 1);
        let _ = std::fs::remove_file(&path);
    }
}
