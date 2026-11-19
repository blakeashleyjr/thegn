//! Background per-worktree measurement: the `du` size scan and the tokei LOC
//! count.
//!
//! Both are the same shape — pick targets, ask the pure planner
//! ([`thegn_core::scan_sched`]) what to do this round, do it off the loop on the
//! background lane, pulse the waker so the next hydration paints from cache — so
//! the shared parts live here and the two runners stay small. Lifted out of
//! `hydrate.rs`, which owned the size scan and (worse) ran the LOC walk inline
//! on the interactive hydration lane.
//!
//! Three rules hold for everything in this module:
//!
//! 1. **Never on the loop.** A cold `du` is seconds; a tokei walk on a large
//!    tree is longer. Each runs on its own named `Background` lane, outside the
//!    shared background permits.
//! 2. **Never wake a sleeping sandbox.** Remote/provider worktrees are skipped
//!    outright — measuring them on a timer would fight `[lifecycle]`
//!    hibernation, and their host path is a stub whose size would be a lie.
//! 3. **One round at a time.** A round outliving its pump used to mean
//!    overlapping full scans double-measuring everything and queueing redundant
//!    work in the dedicated measurement lane.

pub(crate) mod disk;
pub(crate) mod loc;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread::JoinHandle;

use thegn_core::db::Db;
use thegn_core::scan_sched::ScanTarget;
use thegn_core::store::WorkspaceStore;

/// Tracing target for both scanners — `THEGN_LOG=thegn::measure=debug` shows
/// each round's plan, and every reason a round declined to run.
pub(crate) const LOG: &str = "thegn::measure";

type Job = Box<dyn FnOnce() + Send>;

struct LaneHandle {
    tx: mpsc::SyncSender<Job>,
    _thread: JoinHandle<()>,
}

/// One long-lived, bounded measurement worker. Keeping the handle makes its
/// ownership explicit; the worker parks in `recv` while its lane is idle.
pub(crate) struct Lane {
    name: &'static str,
    slot: OnceLock<Option<LaneHandle>>,
}

impl Lane {
    pub(crate) const fn new(name: &'static str) -> Self {
        Self {
            name,
            slot: OnceLock::new(),
        }
    }

    pub(crate) fn submit(&'static self, job: Job) -> crate::sched::Admission {
        let Some(handle) = self.slot.get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel::<Job>(1);
            let name = self.name;
            let thread = match thegn_core::thread_name::try_spawn_named(name, move || {
                crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                while let Ok(job) = rx.recv() {
                    if catch_unwind(AssertUnwindSafe(job)).is_err() {
                        tracing::warn!(target: LOG, lane = name, "measurement job panicked");
                    }
                }
            }) {
                Ok(thread) => thread,
                Err(error) => {
                    tracing::debug!(target: LOG, lane = name, %error, "could not start measurement lane");
                    return None;
                }
            };
            Some(LaneHandle { tx, _thread: thread })
        }) else {
            return crate::sched::Admission::Unavailable;
        };

        match handle.tx.try_send(job) {
            Ok(()) => crate::sched::Admission::Admitted,
            Err(mpsc::TrySendError::Full(_)) => crate::sched::Admission::Saturated,
            Err(mpsc::TrySendError::Disconnected(_)) => crate::sched::Admission::Unavailable,
        }
    }
}

/// RAII round guard. Process-global rather than a loop-side `bool` (contrast
/// `run.rs`'s `prq_inflight`) because a scan is fire-and-forget: there is no
/// completion message back to the loop that could clear a loop-side flag, and a
/// Drop-released atomic stays correct even if the round panics.
pub(crate) struct Round(&'static AtomicBool);

impl Drop for Round {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Claim the single round slot for a scanner. `None` ⇒ a round is already in
/// flight, so skip — the next pump retries, and everything this round would
/// have measured is still stale (and now older, so it sorts earlier).
pub(crate) fn begin(flag: &'static AtomicBool, what: &'static str) -> Option<Round> {
    match flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => Some(Round(flag)),
        Err(_) => {
            tracing::debug!(target: LOG, scan = what, "round already in flight — skipping");
            None
        }
    }
}

/// Process-wide "nothing due before" gate for one lane. A pump earlier than
/// `next_due` returns before any DB open or lane job.
///
/// Armed after each round with [`thegn_core::scan_sched::next_due_at`]: the
/// moment the earliest row reaches its TTL. Between sweeps a lane therefore has
/// no work in hand; the ticker slot still fires, but costs two atomic loads.
/// A config reload changes the fingerprint and re-opens the gate; a kick
/// ([`kick`]) re-opens it too.
pub(crate) struct DueGate {
    next_due: AtomicI64,
    shape_fp: AtomicU64,
    kicked: AtomicBool,
}

impl DueGate {
    pub(crate) const fn new() -> Self {
        Self {
            // Open: the first pump of the process always runs.
            next_due: AtomicI64::new(0),
            shape_fp: AtomicU64::new(0),
            kicked: AtomicBool::new(false),
        }
    }

    /// `true` if a round should be submitted for this trigger now. Watch and
    /// Kick always pass; a Pump passes once the armed time has come or when
    /// the config fingerprint changed since the gate was armed.
    pub(crate) fn admit(
        &self,
        now: i64,
        fp: u64,
        trigger: thegn_core::scan_sched::RoundTrigger,
    ) -> bool {
        use thegn_core::scan_sched::RoundTrigger;
        match trigger {
            RoundTrigger::Watch | RoundTrigger::Kick => true,
            RoundTrigger::Pump => {
                self.shape_fp.load(Ordering::Acquire) != fp
                    || now >= self.next_due.load(Ordering::Acquire)
            }
        }
    }

    /// Record when the next pump has work. `None` (nothing tracked) closes the
    /// gate until a kick or a config change.
    pub(crate) fn arm(&self, next_due: Option<i64>, fp: u64) {
        self.next_due
            .store(next_due.unwrap_or(i64::MAX), Ordering::Release);
        self.shape_fp.store(fp, Ordering::Release);
    }

    /// Re-open the gate and mark the next round as a kick.
    pub(crate) fn kick(&self) {
        self.next_due.store(0, Ordering::Release);
        self.kicked.store(true, Ordering::Release);
    }

    /// Consume a pending kick: `true` once per [`DueGate::kick`].
    pub(crate) fn take_kick(&self) -> bool {
        self.kicked.swap(false, Ordering::AcqRel)
    }
}

pub(crate) static LOC_GATE: DueGate = DueGate::new();
pub(crate) static DISK_GATE: DueGate = DueGate::new();

/// Config fingerprint (ttl, budget, enabled, ...): a reload re-opens the gate.
/// FNV-1a over the parts; stable within a process, which is all a gate needs.
pub(crate) fn shape_fingerprint(parts: &[u64]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

pub(crate) static DISK_LANE: Lane = Lane::new("thegn-msr-disk");
pub(crate) static LOC_LANE: Lane = Lane::new("thegn-msr-loc");

/// Kick both scans now: startup, a worktree created, a workspace added, a
/// worktree switched to.
///
/// Cheap to over-call — a kick round plans only the active row and never-measured
/// rows, so a redundant kick costs one DB read and plans nothing. Both gates are
/// re-opened first, so the pump after a kick sweeps as well. The point is that a
/// *cold* path (the freshly created worktree, the workspace you just switched
/// to) is measured within a second or two instead of waiting out a full pump
/// interval.
pub(crate) fn kick(tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>) {
    LOC_GATE.kick();
    DISK_GATE.kick();
    let _ = tx.send(crate::hydrate::RefreshKind::Disk); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
    let _ = tx.send(crate::hydrate::RefreshKind::Loc { watch: false }); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
}

/// Every path a background scan should cover: the worktree registry, plus each
/// workspace's MAIN checkout.
///
/// The workspace roots are the fix for a whole class of "it never shows a size":
/// a workspace's home group is session-only — the workspace create/switch flow
/// writes `workspaces`/`repos` rows but never a
/// `worktrees` row — so a scan that walked `db.worktrees()` alone left the
/// sidebar workspace row (which does carry `worktree_path = repo_path`) and the
/// bottom-bar `disk` chip on a home tab permanently blank.
///
/// Remote/provider worktrees are excluded: see the module docs, rule 2.
fn candidate_paths(db: &Db) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(rows) = db.worktrees() {
        for row in rows {
            // Resolve from the row's own `location` column rather than
            // `GitLoc::for_worktree`, which would re-open the DB per row.
            let loc = thegn_core::remote::GitLoc::from_db(&row.worktree, Some(&row.location));
            if loc.is_remote() {
                continue;
            }
            out.push(row.worktree);
        }
    }
    if let Ok(rows) = db.workspaces() {
        for w in rows {
            if !w.repo_path.is_empty() {
                out.push(w.repo_path);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Log once per process per lane when [`thegn_core::scan_sched::lane_shape`]
/// had to stretch the effective TTL (the registry is larger than one window's
/// examine ceiling). INFO, not WARN: the lane still idles; it only refreshes
/// background rows less often.
pub(crate) fn log_stretched_once(lane: &'static str, rows: usize, ttl: u64) {
    use std::sync::Mutex;
    static LOGGED: OnceLock<Mutex<std::collections::BTreeSet<&'static str>>> = OnceLock::new();
    let seen = LOGGED.get_or_init(|| Mutex::new(std::collections::BTreeSet::new()));
    let Ok(mut seen) = seen.lock() else {
        return; // best-effort: diagnostics only; a poisoned latch must not break a scan
    };
    if !seen.insert(lane) {
        return;
    }
    tracing::info!(
        target: LOG,
        scan = lane,
        rows,
        effective_ttl_secs = ttl,
        "registry exceeds one window's examine ceiling; stretching the background refresh"
    );
}

/// [`candidate_paths`] joined with a cache's fetch stamps and the on-screen
/// worktree, ready for [`thegn_core::scan_sched::plan`].
fn targets(
    db: &Db,
    stamps: &std::collections::HashMap<String, i64>,
    active: Option<&str>,
) -> Vec<ScanTarget> {
    candidate_paths(db)
        .into_iter()
        .map(|path| ScanTarget {
            measured_at: stamps.get(&path).copied(),
            active: active == Some(path.as_str()),
            path,
        })
        .collect()
}

/// The live set for the orphan sweeps. Deliberately the FULL registry, remote
/// worktrees included: a remote worktree isn't measured, but it is still live,
/// and reaping a size some earlier version cached for it is the orphan sweep's
/// job only once the worktree itself is gone.
fn live_paths(db: &Db) -> Vec<String> {
    let mut out: Vec<String> = db
        .worktrees()
        .map(|rows| rows.into_iter().map(|r| r.worktree).collect())
        .unwrap_or_default();
    if let Ok(rows) = db.workspaces() {
        out.extend(
            rows.into_iter()
                .map(|w| w.repo_path)
                .filter(|p| !p.is_empty()),
        );
    }
    out
}

/// The active worktree path as a plain `String`, for the `active` flag.
pub(crate) fn active_key(active: Option<&std::path::Path>) -> Option<String> {
    active.map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use thegn_core::store::WorktreeAuxStore;

    static TEST_FLAG: AtomicBool = AtomicBool::new(false);

    #[test]
    fn begin_admits_one_round_and_releases_on_drop() {
        let first = begin(&TEST_FLAG, "test").expect("first round is admitted");
        assert!(
            begin(&TEST_FLAG, "test").is_none(),
            "a second concurrent round must be refused"
        );
        drop(first);
        assert!(
            begin(&TEST_FLAG, "test").is_some(),
            "the slot frees when the round ends"
        );
    }

    /// The whole point of covering `workspaces.repo_path`: a workspace's home
    /// group never gets a `worktrees` row, so walking the registry alone left
    /// every workspace row and home tab permanently sizeless.
    #[test]
    fn candidates_cover_workspace_roots_as_well_as_worktrees() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.put_worktree("repo/feat", "/repo", "/wt/feat", "feat", None, None)
            .unwrap();

        let paths = candidate_paths(&db);
        assert!(paths.contains(&"/repo".to_string()), "{paths:?}");
        assert!(paths.contains(&"/wt/feat".to_string()), "{paths:?}");
    }

    /// A provider/remote worktree's host path is a stub — `du`ing it produces a
    /// confident wrong number, and scanning it on a timer would fight
    /// `[lifecycle]` hibernation. It must never become a target.
    #[test]
    fn candidates_exclude_remote_worktrees() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("repo/local", "/repo", "/wt/local", "local", None, None)
            .unwrap();
        db.put_worktree(
            "repo/far",
            "/repo",
            "/wt/far",
            "far",
            Some(r#"{"host":"build-box","path":"/srv/wt","port":22,"forward_agent":false}"#),
            None,
        )
        .unwrap();

        let paths = candidate_paths(&db);
        assert!(paths.contains(&"/wt/local".to_string()), "{paths:?}");
        assert!(!paths.contains(&"/wt/far".to_string()), "{paths:?}");

        // …but it is still LIVE, so the orphan sweep must not reap whatever an
        // earlier version cached for it.
        assert!(live_paths(&db).contains(&"/wt/far".to_string()));
    }

    #[test]
    fn candidates_dedupe_a_root_that_is_also_a_registered_worktree() {
        let db = Db::open_memory().unwrap();
        db.put_workspace("/repo", "repo", "repo").unwrap();
        db.put_worktree("repo/home", "/repo", "/repo", "main", None, None)
            .unwrap();
        let paths = candidate_paths(&db);
        assert_eq!(
            paths.iter().filter(|p| *p == "/repo").count(),
            1,
            "{paths:?}"
        );
    }

    #[test]
    fn targets_carry_the_cache_stamp_and_flag_the_active_worktree() {
        let db = Db::open_memory().unwrap();
        db.put_worktree("repo/a", "/repo", "/wt/a", "a", None, None)
            .unwrap();
        db.put_worktree("repo/b", "/repo", "/wt/b", "b", None, None)
            .unwrap();
        db.put_worktree_disk("/wt/a", 10, 2).unwrap();
        let stamps = db.all_worktree_disk_stamps().unwrap();

        let targets = targets(&db, &stamps, Some("/wt/b"));
        let a = targets.iter().find(|t| t.path == "/wt/a").unwrap();
        let b = targets.iter().find(|t| t.path == "/wt/b").unwrap();
        assert!(a.measured_at.is_some() && !a.active);
        assert!(
            b.measured_at.is_none() && b.active,
            "the on-screen, never-measured worktree is the one that must go first"
        );
        // And the planner agrees.
        let due = thegn_core::scan_sched::plan(&targets, thegn_core::util::now(), 100, 0);
        assert_eq!(due.first().map(String::as_str), Some("/wt/b"));
    }

    #[test]
    fn due_gate_holds_a_pump_until_its_next_due() {
        use thegn_core::scan_sched::RoundTrigger;
        let gate = DueGate::new();
        let fp = shape_fingerprint(&[900, 2, 1]);
        assert!(
            gate.admit(100, fp, RoundTrigger::Pump),
            "a fresh gate is open"
        );
        gate.arm(Some(1_000), fp);
        assert!(!gate.admit(999, fp, RoundTrigger::Pump));
        assert!(gate.admit(1_000, fp, RoundTrigger::Pump));
        assert!(gate.admit(5_000, fp, RoundTrigger::Pump));
        // Watch and Kick are never held.
        assert!(gate.admit(0, fp, RoundTrigger::Watch));
        assert!(gate.admit(0, fp, RoundTrigger::Kick));
        // A config reload re-opens it.
        let reloaded = shape_fingerprint(&[600, 2, 1]);
        assert_ne!(fp, reloaded);
        assert!(gate.admit(0, reloaded, RoundTrigger::Pump));
        // Nothing tracked: closed until a kick.
        gate.arm(None, fp);
        assert!(!gate.admit(i64::MAX - 1, fp, RoundTrigger::Pump));
        assert!(!gate.take_kick());
        gate.kick();
        assert!(gate.admit(0, fp, RoundTrigger::Pump), "kick re-opens");
        assert!(gate.take_kick());
        assert!(!gate.take_kick(), "a kick is consumed once");
    }

    #[test]
    fn shape_fingerprint_separates_parts() {
        assert_eq!(shape_fingerprint(&[1, 2]), shape_fingerprint(&[1, 2]));
        assert_ne!(shape_fingerprint(&[1, 2]), shape_fingerprint(&[2, 1]));
        assert_ne!(shape_fingerprint(&[]), shape_fingerprint(&[0]));
    }

    #[test]
    fn active_key_stringifies_the_path_or_yields_none() {
        assert_eq!(
            active_key(Some(std::path::Path::new("/wt/a"))).as_deref(),
            Some("/wt/a")
        );
        assert!(active_key(None).is_none());
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn lane_runs_jobs_on_one_named_thread() {
        static LANE: Lane = Lane::new("thegn-msr-one");
        let (tx, rx) = mpsc::channel();
        let mut seen = Vec::new();
        // One job at a time: the queue holds a single pending job, so a second
        // submit before the lane takes the first is (correctly) `Saturated`.
        for _ in 0..2 {
            let tx = tx.clone();
            assert_eq!(
                LANE.submit(Box::new(move || {
                    tx.send((
                        std::thread::current().id(),
                        std::thread::current().name().map(str::to_owned),
                    ))
                    .unwrap();
                })),
                crate::sched::Admission::Admitted
            );
            seen.push(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        let (first, second) = (seen[0].clone(), seen[1].clone());
        assert_eq!(first, second);
        assert_eq!(first.1.as_deref(), Some("thegn-msr-one"));
    }

    #[test]
    fn lane_full_queue_refuses_without_blocking() {
        static LANE: Lane = Lane::new("thegn-msr-full");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        assert_eq!(
            LANE.submit(Box::new(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })),
            crate::sched::Admission::Admitted
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            LANE.submit(Box::new(move || finished_tx.send(()).unwrap())),
            crate::sched::Admission::Admitted
        );
        assert_eq!(
            LANE.submit(Box::new(|| {})),
            crate::sched::Admission::Saturated
        );
        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn lane_survives_a_panicking_job() {
        static LANE: Lane = Lane::new("thegn-msr-panic");
        assert_eq!(
            LANE.submit(Box::new(|| panic!("expected lane test panic"))),
            crate::sched::Admission::Admitted
        );
        let (tx, rx) = mpsc::channel();
        // The panicking job may still sit in the single-slot queue: retry until
        // the lane takes the follow-up, which proves it survived the panic.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let tx = tx.clone();
            match LANE.submit(Box::new(move || tx.send(()).unwrap())) {
                crate::sched::Admission::Admitted => break,
                other => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "lane never re-admitted: {other:?}"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}
