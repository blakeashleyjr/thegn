//! Host-side terminal-activity state machine for the sidebar's live dots.
//!
//! Activity is measured by scanning `/proc` for processes whose cwd sits under
//! a managed worktree and summing their CPU time. A worktree whose CPU is
//! advancing is `active` (filled white dot — working); one that was active and
//! has gone idle is `waiting` (filled **red** dot — "stuck, look at me", an
//! *unread* alert); focusing its tab marks it `read` (hollow red dot — seen but
//! still stuck) via [`ack`]. A red worktree is **sticky**: it only leaves red
//! when work *genuinely resumes* — sustained CPU over `RESUME_GRACE_SECS`, not
//! a one-window blip from a spinner redraw or a stray watcher.
//!
//!   none ─── cpu delta ≥ threshold ─────────▶ active
//!   active ─ idle ≥ QUIET_GRACE_SECS ───────▶ waiting   (filled red, unread)
//!   waiting ─ ack(tab) ─────────────────────▶ read      (hollow red, seen)
//!   waiting/read ─ busy ≥ RESUME_GRACE_SECS ▶ active     (work resumed)
//!
//! State persists in `~/.superzej/activity.json` (ephemeral, self-healing; kept
//! out of the SQLite DB so frequent polling never contends on the WAL). This
//! used to be the `superzej activity` CLI command; the native host now owns the
//! FSM in-process. Never errors on scan problems — a partial/empty scan just
//! holds the current state (a stuck worktree stays red).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// CPU per wall-second that counts as "working": 3 jiffies/s = 30ms/s ≈ 3% of
/// one core. Catches builds / model streaming / tool runs; ignores an idle
/// shell prompt. (CLK_TCK hardcoded to the Linux default of 100.)
const ACTIVE_JIFFIES_PER_SEC: f64 = 3.0;
/// An `active` worktree must stay below the threshold this long before it turns
/// `waiting` — damps flapping from scheduling gaps between close polls.
const QUIET_GRACE_SECS: f64 = 5.0;
/// A red (`waiting`/`read`) worktree must stay *continuously* busy this long
/// before it flips back to `active`. Without this, a single spinner redraw or
/// stray watcher blip over the CPU threshold would clear the "stuck" dot a
/// fraction of a second after it appeared — the over-resetting this FSM fixes.
const RESUME_GRACE_SECS: f64 = 3.0;
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
    state: String, // "none" | "active" | "waiting" | "read"
    #[serde(default)]
    quiet_since: Option<f64>,
    #[serde(default)]
    last_active_at: Option<f64>,
    /// When the current uninterrupted busy streak began (CPU ≥ threshold every
    /// poll since). `None` while idle. Gates `waiting`/`read` → `active` so a
    /// momentary blip can't clear a stuck dot.
    #[serde(default)]
    busy_since: Option<f64>,
}

/// Path to the activity snapshot.
fn state_path() -> PathBuf {
    crate::util::superzej_dir().join("activity.json")
}

/// Read the latest activity states as `tab_name -> state` (`"active"`,
/// `"waiting"`, `"read"`, `"none"`). Empty on any read/parse failure.
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
    poll_and_save_with(managed, &BTreeMap::new());
}

/// [`poll_and_save`] with injected per-worktree jiffies (keyed by worktree path)
/// that **override** the local `/proc` scan for those paths. Used for remote/
/// provider worktrees whose real processes run in the env, not on this host —
/// the host gathers their jiffies over the resident bridge (`proc.list`) and
/// passes them in. Local worktrees (absent from `extra`) are scanned as usual.
pub fn poll_and_save_with(managed: &[ManagedWorktree], extra: &BTreeMap<String, u64>) {
    poll_and_save_at_with(&state_path(), managed, extra, unix_now());
}

/// [`poll_and_save`] against an explicit path/clock (testable).
pub fn poll_and_save_at(path: &Path, managed: &[ManagedWorktree], now: f64) {
    poll_and_save_at_with(path, managed, &BTreeMap::new(), now);
}

/// [`poll_and_save_with`] against an explicit path/clock (testable).
pub fn poll_and_save_at_with(
    path: &Path,
    managed: &[ManagedWorktree],
    extra: &BTreeMap<String, u64>,
    now: f64,
) {
    let mut snap = load(path);
    if now - snap.polled_at < MIN_SCAN_INTERVAL_SECS {
        return;
    }
    poll(&mut snap, managed, extra, now);
    save(path, &snap);
}

/// Mark a worktree's tab as read: a `waiting` "look at me" dot (filled red)
/// turns `read` (hollow red) once the user focuses the tab. The dot is *not*
/// cleared — it stays hollow until work genuinely resumes. No-op unless the tab
/// is `waiting`.
pub fn ack(tab: &str) {
    ack_at(&state_path(), tab);
}

/// [`ack`] against an explicit path (testable).
pub fn ack_at(path: &Path, tab: &str) {
    let mut snap = load(path);
    let mut changed = false;
    for e in snap.worktrees.values_mut() {
        if e.tab == tab && e.state == "waiting" {
            e.state = "read".into();
            changed = true;
        }
    }
    if changed {
        save(path, &snap);
    }
}

/// The settled state a stale running/active dot collapses to at resurrection
/// (no live work; no dot).
pub const SETTLED_STATE: &str = "none";

/// Restore-time stale-state guard (pure). A `"running"`/`"active"` state whose
/// last live signal is older than `grace_ms` collapses to [`SETTLED_STATE`], so a
/// session killed mid-run never resurrects a phantom forever-running dot. Fresh
/// running states and already-settled states (`"waiting"`/`"read"`/`"none"`) pass
/// through unchanged. This is the age-based generalization of the live
/// `RESUME_GRACE_SECS` sticky logic, applied **once** at resurrection; the live
/// `poll` FSM is untouched. Boundary: an age of exactly `grace_ms` is treated
/// as stale (`>=`), matching the `RESUME_GRACE_SECS` convention above.
pub fn coerce_stale(state: &str, age_ms: u64, grace_ms: u64) -> String {
    let running = matches!(state, "active" | "running");
    if running && age_ms >= grace_ms {
        SETTLED_STATE.to_string()
    } else {
        state.to_string()
    }
}

/// Apply [`coerce_stale`] to every persisted entry once at resurrection, so a
/// crash mid-run doesn't resurrect a phantom running/stuck dot. Each entry's age
/// is `now - last_active_at` (falling back to the snapshot's `polled_at` when the
/// entry never recorded an active timestamp). A coerced entry also clears its
/// streak bookkeeping so the next `poll` starts clean; the live FSM then
/// re-derives the true state from fresh CPU deltas. Best-effort: a missing or
/// garbled snapshot is a no-op, and nothing is written unless a state changed.
pub fn coerce_stale_states_at(path: &Path, grace_ms: u64, now: f64) {
    let mut snap = load(path);
    if snap.worktrees.is_empty() {
        return;
    }
    let mut changed = false;
    for e in snap.worktrees.values_mut() {
        let ref_secs = e.last_active_at.unwrap_or(snap.polled_at);
        let age_ms = ((now - ref_secs).max(0.0) * 1000.0) as u64;
        let coerced = coerce_stale(&e.state, age_ms, grace_ms);
        if coerced != e.state {
            e.state = coerced;
            e.quiet_since = None;
            e.busy_since = None;
            changed = true;
        }
    }
    if changed {
        save(path, &snap);
    }
}

/// [`coerce_stale_states_at`] against the default snapshot path + wall clock.
pub fn coerce_stale_states(grace_ms: u64) {
    coerce_stale_states_at(&state_path(), grace_ms, unix_now());
}

/// One scan + state-machine step over every managed worktree. `extra` supplies
/// pre-fetched jiffies (e.g. from a remote env's bridge) that override the local
/// `/proc` scan for those worktree paths.
fn poll(snap: &mut Snapshot, managed: &[ManagedWorktree], extra: &BTreeMap<String, u64>, now: f64) {
    // Longest-prefix targets so a worktree nested under its repo root
    // (worktree_mode = "in_repo") wins over the home tab.
    let mut targets: Vec<(PathBuf, String)> = managed
        .iter()
        .map(|w| (PathBuf::from(&w.worktree), w.worktree.clone()))
        .collect();
    targets.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));

    let mut jiffies = scan_proc(&targets);
    // Remote/provider worktrees: the bridge's in-env scan is authoritative (it
    // is inserted even when 0, so a stray host process under a bind path can't
    // masquerade as in-env activity).
    for (k, v) in extra {
        jiffies.insert(k.clone(), *v);
    }

    let wall = (now - snap.polled_at).max(0.0);
    let first_poll = snap.polled_at == 0.0;
    let threshold = ACTIVE_JIFFIES_PER_SEC * wall;

    // Start from the prior snapshot so worktrees absent from `managed` this
    // cycle (a transient DB-read gap, a not-yet-persisted tab) carry their state
    // forward unchanged instead of being reset to `none`.
    let mut next = std::mem::take(&mut snap.worktrees);
    for w in managed {
        let cur = jiffies.get(&w.worktree).copied().unwrap_or(0);
        let prev_known = next.contains_key(&w.worktree);
        let mut e = next.remove(&w.worktree).unwrap_or(Entry {
            tab: w.tab.clone(),
            cpu_jiffies: cur,
            state: "none".into(),
            quiet_since: None,
            last_active_at: None,
            busy_since: None,
        });
        e.tab = w.tab.clone(); // tab renames follow the caller

        // A first sighting (or first-ever poll) records a baseline; deltas only
        // mean something from the second reading on.
        if prev_known && !first_poll {
            let delta = cur.saturating_sub(e.cpu_jiffies) as f64;
            let busy = delta >= threshold && wall > 0.0;

            // Track the uninterrupted busy streak.
            if busy {
                e.busy_since.get_or_insert(now);
            } else {
                e.busy_since = None;
            }

            match e.state.as_str() {
                // Red is sticky: only sustained, genuine work resumes it. A
                // momentary blip (busy for a single window) is ignored.
                "waiting" | "read" => {
                    if busy && now - e.busy_since.unwrap_or(now) >= RESUME_GRACE_SECS {
                        e.state = "active".into();
                        e.quiet_since = None;
                        e.last_active_at = Some(now);
                    }
                }
                "active" => {
                    if busy {
                        e.last_active_at = Some(now);
                    } else if now - e.last_active_at.unwrap_or(0.0) >= QUIET_GRACE_SECS {
                        e.state = "waiting".into();
                        e.quiet_since = Some(now);
                    }
                }
                // none / legacy / unknown: any work wakes it.
                _ => {
                    if busy {
                        e.state = "active".into();
                        e.quiet_since = None;
                        e.last_active_at = Some(now);
                    }
                }
            }
        }
        e.cpu_jiffies = cur;
        next.insert(w.worktree.clone(), e);
    }

    snap.version = 1;
    snap.polled_at = now;
    snap.worktrees = next;
}

/// Sum utime+stime jiffies for every process whose cwd is under each path —
/// the reusable core of the activity scan. Also served over the resident bridge
/// (`proc.list`) so a remote env's *own* processes drive the activity dots.
/// Longest-prefix wins (a nested worktree over its repo root). Empty off Linux.
pub fn cpu_jiffies_by_path(paths: &[String]) -> BTreeMap<String, u64> {
    let mut targets: Vec<(PathBuf, String)> = paths
        .iter()
        .map(|p| (PathBuf::from(p), p.clone()))
        .collect();
    targets.sort_by_key(|(p, _)| std::cmp::Reverse(p.as_os_str().len()));
    scan_proc(&targets)
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
        let json = r#"{"worktrees":{"/wt/a":{"tab":"app/home","state":"waiting","cpu_jiffies":0},
                        "/wt/b":{"tab":"app/feat","state":"read","cpu_jiffies":0}}}"#;
        std::fs::write(&path, json).unwrap();
        let m = read_states_at(&path);
        assert_eq!(m.get("app/home").map(String::as_str), Some("waiting"));
        assert_eq!(m.get("app/feat").map(String::as_str), Some("read"));
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
    fn poll_records_baseline_then_waiting_then_read() {
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
        // No CPU advance (path doesn't exist) → stays none, never panics.
        poll_and_save_at(&path, &managed, 1100.0);
        let st = read_states_at(&path);
        assert!(st.contains_key("app/home"));

        // Manually mark waiting, then ack turns it to read (hollow, not cleared).
        let mut snap = load(&path);
        if let Some(e) = snap.worktrees.values_mut().next() {
            e.state = "waiting".into();
            e.quiet_since = Some(1100.0);
        }
        save(&path, &snap);
        ack_at(&path, "app/home");
        assert_eq!(
            read_states_at(&path).get("app/home").map(String::as_str),
            Some("read")
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

    /// Seed an `active` entry with a known low jiffies baseline so the next
    /// poll (no real CPU advance under the bogus path) sees `delta < threshold`
    /// and `now - last_active_at >= QUIET_GRACE_SECS`, flipping it to `waiting`.
    #[test]
    fn active_goes_waiting_after_grace() {
        let path = tmp("waiting");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt-waiting".into(),
            tab: "app/q".into(),
        }];
        // Baseline poll establishes prev + polled_at.
        poll_and_save_at(&path, &managed, 1000.0);

        // Hand-edit the entry into the `active` state with an old activity
        // timestamp, then poll again past the grace window with no CPU advance.
        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut("/nonexistent/wt-waiting").unwrap();
            e.state = "active".into();
            e.cpu_jiffies = 0;
            e.last_active_at = Some(1000.0);
            e.quiet_since = None;
        }
        save(&path, &snap);

        // wall = 1010 - 1000 = 10s > 0; delta = 0 < threshold; grace elapsed.
        poll_and_save_at(&path, &managed, 1010.0);
        let st = read_states_at(&path);
        assert_eq!(st.get("app/q").map(String::as_str), Some("waiting"));

        // The quiet_since stamp was recorded.
        let snap = load(&path);
        assert_eq!(
            snap.worktrees["/nonexistent/wt-waiting"].quiet_since,
            Some(1010.0)
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A `waiting`/`read` dot is sticky: an idle poll (no CPU advance) never
    /// clears it, and a worktree absent from `managed` carries its state
    /// forward unchanged instead of resetting to `none`.
    #[test]
    fn waiting_is_sticky_and_survives_absence() {
        let path = tmp("sticky");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt-sticky".into(),
            tab: "app/s".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);

        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut("/nonexistent/wt-sticky").unwrap();
            e.state = "waiting".into();
            e.cpu_jiffies = 0;
            e.quiet_since = Some(1000.0);
        }
        save(&path, &snap);

        // Idle poll: no CPU advance under the bogus path → stays waiting.
        poll_and_save_at(&path, &managed, 1100.0);
        assert_eq!(
            read_states_at(&path).get("app/s").map(String::as_str),
            Some("waiting")
        );

        // The worktree drops out of `managed` for a cycle → carried forward.
        poll_and_save_at(&path, &[], 1200.0);
        assert_eq!(
            read_states_at(&path).get("app/s").map(String::as_str),
            Some("waiting")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A real CPU burner under a managed worktree drives the sticky-resume
    /// edge: a `read` dot stays red through a single busy window and only flips
    /// to `active` once the busy streak has lasted `RESUME_GRACE_SECS`.
    #[cfg(target_os = "linux")]
    #[test]
    fn read_resumes_active_only_after_sustained_busy() {
        use std::process::Command;
        let wt = std::env::temp_dir().join(format!("sz-act-resume-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let path = tmp("resume");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: wt.to_string_lossy().into_owned(),
            tab: "app/r".into(),
        }];

        let mut child = Command::new("sh")
            .arg("-c")
            .arg("while :; do :; done")
            .current_dir(&wt)
            .spawn()
            .expect("spawn cpu burner");

        // Baseline records the burner's current jiffies, then seed `read`.
        poll_and_save_at(&path, &managed, 1000.0);
        let mut snap = load(&path);
        {
            let e = snap
                .worktrees
                .get_mut(&wt.to_string_lossy().into_owned())
                .unwrap();
            e.state = "read".into();
            e.busy_since = None;
        }
        save(&path, &snap);

        // First busy window (wall=1s): busy=true but the streak is 0s old, so a
        // sticky red dot must NOT clear yet.
        std::thread::sleep(std::time::Duration::from_millis(250));
        poll_and_save_at(&path, &managed, 1001.0);
        assert_eq!(
            read_states_at(&path).get("app/r").map(String::as_str),
            Some("read"),
            "a single busy window must not clear the sticky dot"
        );

        // Keep burning until the streak exceeds RESUME_GRACE_SECS (3s): resume.
        // wall = 1005 - 1001 = 4s ⇒ threshold = 12 jiffies; burn well past it.
        std::thread::sleep(std::time::Duration::from_millis(500));
        poll_and_save_at(&path, &managed, 1005.0);
        assert_eq!(
            read_states_at(&path).get("app/r").map(String::as_str),
            Some("active"),
            "sustained busy must resume the worktree to active"
        );

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// An `active` entry still inside the grace window neither flaps to quiet
    /// nor (without CPU advance) re-arms active — it just holds `active`.
    #[test]
    fn active_holds_within_grace() {
        let path = tmp("hold");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/wt-hold".into(),
            tab: "app/h".into(),
        }];
        poll_and_save_at(&path, &managed, 1000.0);

        let mut snap = load(&path);
        {
            let e = snap.worktrees.get_mut("/nonexistent/wt-hold").unwrap();
            e.state = "active".into();
            e.cpu_jiffies = 0;
            e.last_active_at = Some(1000.5);
        }
        save(&path, &snap);

        // 1s wall, only 0.5s since last activity → within QUIET_GRACE_SECS.
        poll_and_save_at(&path, &managed, 1001.0);
        let st = read_states_at(&path);
        assert_eq!(st.get("app/h").map(String::as_str), Some("active"));
        let _ = std::fs::remove_file(&path);
    }

    /// Drive the `active` transition (lines 157-159) and the `/proc` scan
    /// (`scan_proc` + `stat_jiffies`) with a real CPU-burning child process
    /// whose cwd lives under a managed worktree directory.
    #[cfg(target_os = "linux")]
    #[test]
    fn real_cpu_burn_marks_active() {
        use std::process::Command;
        let wt = std::env::temp_dir().join(format!("sz-act-burn-{}", std::process::id()));
        std::fs::create_dir_all(&wt).unwrap();
        let path = tmp("burn");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: wt.to_string_lossy().into_owned(),
            tab: "app/burn".into(),
        }];

        // A shell that spins, burning CPU, with cwd inside the worktree so
        // scan_proc attributes its jiffies to this worktree.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("while :; do :; done")
            .current_dir(&wt)
            .spawn()
            .expect("spawn cpu burner");

        // Baseline poll records the burner's current jiffies.
        poll_and_save_at(&path, &managed, 1000.0);

        // Let it accumulate CPU, then poll again far enough apart that the
        // scan actually runs (>= MIN_SCAN_INTERVAL_SECS) and the delta clears
        // the active threshold.
        std::thread::sleep(std::time::Duration::from_millis(400));
        poll_and_save_at(&path, &managed, 1001.0);

        let _ = child.kill();
        let _ = child.wait();

        let st = read_states_at(&path);
        // The burner ran for ~400ms wall against a 1s "wall" the FSM was told,
        // so threshold = 3 jiffies and the delta (tens of jiffies) clears it.
        assert_eq!(st.get("app/burn").map(String::as_str), Some("active"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&wt);
    }

    /// `stat_jiffies` parses utime+stime from a synthetic /proc/PID/stat line,
    /// including a `comm` field that itself contains spaces and parens.
    #[cfg(target_os = "linux")]
    #[test]
    fn stat_jiffies_parses_fields() {
        let p = std::env::temp_dir().join(format!("sz-stat-{}.txt", std::process::id()));
        // pid (comm) state ppid pgrp ... fields 14/15 (utime/stime) are the
        // 11th/12th whitespace tokens after the last ')'. Build a line where
        // comm = "(weird cmd)" to exercise the rfind(')') logic.
        // After ')': state(3) ppid(4) pgrp(5) session(6) tty(7) tpgid(8)
        // flags(9) minflt(10) cminflt(11) majflt(12) cmajflt(13) utime(14)
        // stime(15) ...
        let line = "42 ((weird cmd)) R 1 1 1 0 -1 0 0 0 0 0 7 11 0 0";
        std::fs::write(&p, line).unwrap();
        assert_eq!(stat_jiffies(p.clone()), Some(18));
        let _ = std::fs::remove_file(&p);
    }

    /// A malformed stat line (no ')') yields None.
    #[cfg(target_os = "linux")]
    #[test]
    fn stat_jiffies_handles_garbage() {
        let p = std::env::temp_dir().join(format!("sz-stat-bad-{}.txt", std::process::id()));
        std::fs::write(&p, "no parens here").unwrap();
        assert_eq!(stat_jiffies(p.clone()), None);
        let _ = std::fs::remove_file(&p);
        // A missing file also yields None.
        assert_eq!(stat_jiffies(PathBuf::from("/no/such/stat")), None);
    }

    /// Injected jiffies (the remote-bridge `proc.list` path) drive the FSM
    /// independent of the local `/proc` scan: a bogus path with no local
    /// processes goes `active` purely from the `extra` override advancing.
    #[test]
    fn injected_jiffies_drive_active() {
        let path = tmp("inject");
        let _ = std::fs::remove_file(&path);
        let managed = vec![ManagedWorktree {
            worktree: "/nonexistent/remote-wt".into(),
            tab: "app/remote".into(),
        }];
        // Baseline poll establishes prev + polled_at with injected jiffies = 0.
        let mut extra = BTreeMap::new();
        extra.insert("/nonexistent/remote-wt".to_string(), 0u64);
        poll_and_save_at_with(&path, &managed, &extra, 1000.0);
        assert_eq!(
            read_states_at(&path).get("app/remote").map(String::as_str),
            Some("none")
        );
        // A second poll 1s later with a large jiffies advance (delta well over
        // the 3-jiffies/s threshold) flips it to active — no /proc involvement.
        extra.insert("/nonexistent/remote-wt".to_string(), 500u64);
        poll_and_save_at_with(&path, &managed, &extra, 1001.0);
        assert_eq!(
            read_states_at(&path).get("app/remote").map(String::as_str),
            Some("active")
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The public, default-path wrappers must run without panicking, covering
    /// `state_path`/`unix_now` plumbing.
    #[test]
    fn default_path_wrappers_dont_panic() {
        let _ = read_states();
        // poll_and_save against the real default path with no managed worktrees:
        // a no-op step that just persists an empty snapshot.
        poll_and_save(&[]);
        // ack of a tab that isn't quiet anywhere: a harmless no-op.
        ack("definitely-not-a-real-tab");
        // unix_now returns a positive, monotonic-ish wall clock.
        assert!(unix_now() > 0.0);
        // coerce_stale_states against the default path: a no-op unless a stale
        // dot exists — never panics.
        coerce_stale_states(600_000);
    }

    // ── restore-time stale-state guard ────────────────────────────────────────

    #[test]
    fn coerce_stale_downgrades_only_stale_running() {
        // Fresh running stays running.
        assert_eq!(coerce_stale("active", 100, 1000), "active");
        assert_eq!(coerce_stale("running", 100, 1000), "running");
        // Stale running downgrades to the settled state.
        assert_eq!(coerce_stale("active", 5000, 1000), SETTLED_STATE);
        assert_eq!(coerce_stale("running", 5000, 1000), SETTLED_STATE);
    }

    #[test]
    fn coerce_stale_passes_non_running_through() {
        // Non-running states are never coerced, however old.
        for st in ["waiting", "read", "none", "quiet", "weird"] {
            assert_eq!(coerce_stale(st, 10_000_000, 1000), st);
        }
    }

    #[test]
    fn coerce_stale_boundary_is_inclusive() {
        // Exactly at the grace threshold counts as stale (>=).
        assert_eq!(coerce_stale("active", 1000, 1000), SETTLED_STATE);
        // One ms under is still fresh.
        assert_eq!(coerce_stale("active", 999, 1000), "active");
    }

    #[test]
    fn coerce_stale_states_downgrades_phantom_but_keeps_fresh() {
        let path = tmp("coerce");
        let _ = std::fs::remove_file(&path);
        // Two worktrees left "active" by a killed session, plus a genuinely-stuck
        // "waiting" dot. polled_at 1000; one entry was last active long ago, the
        // other just before the (simulated) restart.
        let json = r#"{"polled_at":1000.0,"worktrees":{
            "/wt/phantom":{"tab":"app/phantom","state":"active","cpu_jiffies":0,"last_active_at":1000.0},
            "/wt/fresh":{"tab":"app/fresh","state":"active","cpu_jiffies":0,"last_active_at":1990.0},
            "/wt/stuck":{"tab":"app/stuck","state":"waiting","cpu_jiffies":0}
        }}"#;
        std::fs::write(&path, json).unwrap();

        // Restart at now=2000 with a 600s grace: the phantom (1000s old) collapses;
        // the fresh one (10s old) survives; the stuck red dot is never touched.
        coerce_stale_states_at(&path, 600_000, 2000.0);
        let st = read_states_at(&path);
        assert_eq!(st.get("app/phantom").map(String::as_str), Some("none"));
        assert_eq!(st.get("app/fresh").map(String::as_str), Some("active"));
        assert_eq!(st.get("app/stuck").map(String::as_str), Some("waiting"));
    }

    #[test]
    fn coerce_stale_states_no_snapshot_is_noop() {
        let path = tmp("coerce-missing");
        let _ = std::fs::remove_file(&path);
        // Missing file: no write, no panic.
        coerce_stale_states_at(&path, 600_000, 2000.0);
        assert!(!path.exists());
    }
}
