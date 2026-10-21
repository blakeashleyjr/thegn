//! Background DB-writer: a dedicated thread owning one long-lived [`Db`]
//! connection that drains fire-and-forget write jobs off an mpsc channel.
//!
//! The compositor's hard invariant is that the event loop never blocks on I/O
//! (git, DB, subprocess). Yet many loop-side *best-effort cache* persists —
//! yank registers, `ui_state` toggles, pin state — used to call
//! `thegn_core::db::Db::open()` inline, and each open re-runs the WAL pragmas
//! plus the `user_version` migration check (with a 5s `busy_timeout` ceiling if
//! the write lock is contended). Routing those writes through this thread keeps
//! the loop non-blocking: [`persist`] queues a closure and returns immediately.
//!
//! Git is the source of truth and the DB is a cache (see CLAUDE.md). Best-effort
//! writes may be dropped at the cap; required layout/removal writes use a
//! critical class accepted beyond the cap and retained across failed DB opens.
//! A write remains queued across a failed DB open and is retried when another
//! run or flush message arrives. The session-layout persist is routed through this thread
//! (`run::persist_session_layout` captures rows on the loop and queues the
//! transaction): main.rs performs a bounded shutdown flush, and the cold
//! workspace-switch path calls [`flush`] before resurrecting a session whose
//! rows may still be queued. Queue overflow and critical loss are counted and
//! reported both in-session and during shutdown.
//!
//! The thread blocks on `recv()` (no polling, no timer, no `TerminalWaker`
//! pulse), so it adds zero idle wakes — the 0%-idle contract is untouched.
//!
//! The FIFO is bounded. Overflow drops new best-effort writes and preserves
//! the order of writes already queued; [`shutdown_flush`] exposes the total
//! drop count for shutdown reporting and in-session notices.
//! Replaceable writes retain one pending job per key, regardless of how often
//! a caller updates that key.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use termwiz::terminal::TerminalWaker;
use thegn_core::db::Db;

/// A unit of best-effort work run against the writer's connection.
type Job = Box<dyn FnOnce(&Db) + Send>;

/// The production cap bounds memory while allowing ordinary bursts to drain.
#[cfg(not(test))]
const MAX_QUEUED_JOBS: usize = 4096;
#[cfg(test)]
const MAX_QUEUED_JOBS: usize = 8;

enum Msg {
    Run(QueuedJob),
    RunCritical(QueuedJob),
    RunLatest {
        key: String,
    },
    /// A barrier: the writer acks once every earlier job has run or been retained.
    Flush(SyncSender<()>),
}

struct QueuedJob {
    job: Job,
    critical: bool,
}

struct Writer {
    tx: Sender<Msg>,
    state: Arc<QueueState>,
}

#[derive(Default)]
struct QueueState {
    latest: Mutex<HashMap<String, Job>>,
    depth: AtomicUsize,
    in_flight: AtomicUsize,
    dropped: AtomicU64,
    critical_lost: AtomicU64,
    overflow_warned: AtomicBool,
}

struct InFlightGuard<'a> {
    state: &'a QueueState,
}

impl<'a> InFlightGuard<'a> {
    fn new(state: &'a QueueState) -> Self {
        state.in_flight.fetch_add(1, Ordering::AcqRel);
        Self { state }
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        let previous = self.state.in_flight.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(
            previous > 0,
            "finished a job without an in-flight reservation"
        );
    }
}

fn writer() -> &'static Writer {
    static W: OnceLock<Writer> = OnceLock::new();
    W.get_or_init(|| spawn_writer(Db::open))
}

/// Spawn the writer thread with a connection opener and return its send handle.
/// Split from [`writer`] so tests can drive [`run`] with an isolated DB.
fn spawn_writer(open: impl Fn() -> anyhow::Result<Db> + Send + 'static) -> Writer {
    let (tx, rx) = channel::<Msg>();
    let state = Arc::new(QueueState::default());
    let worker_state = Arc::clone(&state);
    std::thread::Builder::new()
        .name("thegn-db-writer".into())
        .spawn(move || run(rx, open, worker_state))
        .expect("spawn db-writer thread");
    Writer { tx, state }
}

/// Drain jobs in FIFO order against a lazily-opened connection. Failed opens
/// retain their jobs and retry only when another message arrives. Returns when
/// the channel closes.
fn run(rx: Receiver<Msg>, open: impl Fn() -> anyhow::Result<Db>, state: Arc<QueueState>) {
    let mut db: Option<Db> = None;
    let mut warned = false;
    let mut retained = VecDeque::new();
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Run(job) => {
                retain_job(&mut retained, job, &state);
                drain_retained(&mut db, &mut warned, &open, &mut retained, &state);
            }
            Msg::RunCritical(job) => {
                retain_job(&mut retained, job, &state);
                drain_retained(&mut db, &mut warned, &open, &mut retained, &state);
            }
            Msg::RunLatest { key } => {
                let job = state
                    .latest
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&key);
                if let Some(job) = job {
                    retain_job(
                        &mut retained,
                        QueuedJob {
                            job,
                            critical: false,
                        },
                        &state,
                    );
                    drain_retained(&mut db, &mut warned, &open, &mut retained, &state);
                } else {
                    dequeue(&state);
                }
            }
            // FIFO: acking means every prior job has run or remains retained.
            Msg::Flush(ack) => {
                drain_retained(&mut db, &mut warned, &open, &mut retained, &state);
                let _ = ack.send(()); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
            }
        }
    }
}

fn reserve(state: &QueueState) -> bool {
    let mut depth = state.depth.load(Ordering::Relaxed);
    loop {
        if depth >= MAX_QUEUED_JOBS {
            return false;
        }
        match state.depth.compare_exchange_weak(
            depth,
            depth + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => depth = actual,
        }
    }
}

fn dequeue(state: &QueueState) {
    let previous = state.depth.fetch_sub(1, Ordering::AcqRel);
    debug_assert!(previous > 0, "dequeued a job without a queue reservation");
    if previous.saturating_sub(1) <= MAX_QUEUED_JOBS / 2 {
        state.overflow_warned.store(false, Ordering::Release);
    }
}

fn retain_job(retained: &mut VecDeque<QueuedJob>, job: QueuedJob, state: &QueueState) {
    if job.critical {
        if retained.len() >= MAX_QUEUED_JOBS
            && let Some(oldest_best_effort) = retained.iter().position(|queued| !queued.critical)
        {
            retained.remove(oldest_best_effort);
            dequeue(state);
            note_drop(state);
        }
        retained.push_back(job);
    } else {
        retained.push_back(job);
        if retained.len() >= MAX_QUEUED_JOBS
            && let Some(oldest_best_effort) = retained.iter().position(|queued| !queued.critical)
        {
            retained.remove(oldest_best_effort);
            dequeue(state);
            note_drop(state);
        }
    }
}

fn note_drop(state: &QueueState) -> bool {
    state.dropped.fetch_add(1, Ordering::Relaxed);
    let first = !state.overflow_warned.swap(true, Ordering::AcqRel);
    if first {
        tracing::warn!(
            dropped = state.dropped.load(Ordering::Relaxed),
            "db-writer dropped a best-effort write"
        );
    }
    if first {
        notify_drop_target("Database writes are being dropped while the database is busy");
    }
    first
}

struct DropNoticeTarget {
    tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: TerminalWaker,
}

fn drop_notice_target() -> &'static Mutex<Option<DropNoticeTarget>> {
    static TARGET: OnceLock<Mutex<Option<DropNoticeTarget>>> = OnceLock::new();
    TARGET.get_or_init(|| Mutex::new(None))
}

pub(crate) fn install_drop_notice(
    tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: TerminalWaker,
) {
    if let Ok(mut target) = drop_notice_target().lock() {
        *target = Some(DropNoticeTarget { tx, waker });
    }
}

fn notify_drop_target(message: &str) {
    if let Ok(target) = drop_notice_target().lock()
        && let Some(target) = target.as_ref()
    {
        if let Err(error) = target.tx.send(crate::hydrate::RefreshKind::Toast {
            message: message.to_owned(),
            priority: thegn_core::notification::Priority::Alert,
        }) {
            tracing::debug!(%error, "failed to send database write-loss toast");
        }
        if let Err(error) = target.waker.wake() {
            tracing::debug!(%error, "failed to wake after database write-loss toast");
        }
    }
}

/// Raise an Alert toast from a DB-writer job; no-op when no UI target is installed.
pub(crate) fn alert(message: &str) {
    notify_drop_target(message);
}

fn enqueue(writer: &Writer, job: impl FnOnce(&Db) + Send + 'static) -> bool {
    if !reserve(&writer.state) {
        note_drop(&writer.state);
        return false;
    }
    if let Err(error) = writer.tx.send(Msg::Run(QueuedJob {
        job: Box::new(job),
        critical: false,
    })) {
        dequeue(&writer.state);
        note_drop(&writer.state);
        tracing::debug!(%error, "db-writer stopped before write could be queued");
        return false;
    }
    true
}

/// Queue required resurrection/removal state. Critical jobs bypass the
/// best-effort cap and are never evicted while a DB open is unavailable.
pub(crate) fn persist_critical(job: impl FnOnce(&Db) + Send + 'static) {
    persist_critical_for(writer(), job);
}

#[cfg(test)]
pub(crate) fn persist_critical_to_closed_writer_for_test(
    job: impl FnOnce(&Db) + Send + 'static,
) -> bool {
    let (tx, rx) = channel();
    drop(rx);
    let writer = Writer {
        tx,
        state: Arc::new(QueueState::default()),
    };
    persist_critical_for(&writer, job)
}

fn persist_critical_for(writer: &Writer, job: impl FnOnce(&Db) + Send + 'static) -> bool {
    writer.state.depth.fetch_add(1, Ordering::AcqRel);
    if let Err(error) = writer.tx.send(Msg::RunCritical(QueuedJob {
        job: Box::new(job),
        critical: true,
    })) {
        writer.state.depth.fetch_sub(1, Ordering::AcqRel);
        writer.state.critical_lost.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(%error, "db-writer stopped before critical write could be queued");
        notify_drop_target("A required database write was lost");
        return false;
    }
    true
}

fn drain_retained(
    db: &mut Option<Db>,
    warned: &mut bool,
    open: &impl Fn() -> anyhow::Result<Db>,
    retained: &mut VecDeque<QueuedJob>,
    state: &QueueState,
) {
    if db.is_none() && !retained.is_empty() {
        match open() {
            Ok(d) => {
                *db = Some(d);
                *warned = false;
            }
            Err(e) => {
                if !*warned {
                    tracing::warn!("db-writer: open failed, retaining writes for retry: {e}");
                    *warned = true;
                }
                return;
            }
        }
    }
    if let Some(d) = db {
        while let Some(job) = retained.pop_front() {
            let _in_flight = InFlightGuard::new(state);
            dequeue(state);
            (job.job)(d);
        }
    }
}

/// Queue a best-effort DB write to run on the writer thread, in send order.
/// Fire-and-forget: returns immediately and never blocks the caller, so it is
/// safe to call from the event loop. Failure to enqueue (writer gone) is
/// logged at debug level if the writer has stopped; a full queue increments
/// the drop counter and emits one warning per overflow episode.
pub(crate) fn persist(job: impl FnOnce(&Db) + Send + 'static) {
    enqueue(writer(), job);
}

/// Queue a replaceable write. A queued write is skipped if a newer value for
/// the same key arrived before the writer began it. Replacements keep the FIFO
/// position of the key's first pending enqueue, so the newest value can run
/// ahead of `persist` jobs queued after that first `persist_latest` call.
pub(crate) fn persist_latest(key: String, job: impl FnOnce(&Db) + Send + 'static) {
    enqueue_latest(writer(), key, job);
}

fn enqueue_latest(writer: &Writer, key: String, job: impl FnOnce(&Db) + Send + 'static) -> bool {
    // Keys must come from a small fixed set so the pending-slot map stays bounded.
    let mut latest = writer
        .state
        .latest
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(slot) = latest.get_mut(&key) {
        *slot = Box::new(job);
        return true;
    }
    if !reserve(&writer.state) {
        note_drop(&writer.state);
        return false;
    }
    latest.insert(key.clone(), Box::new(job));
    if let Err(error) = writer.tx.send(Msg::RunLatest { key: key.clone() }) {
        latest.remove(&key);
        dequeue(&writer.state);
        note_drop(&writer.state);
        tracing::debug!(%error, "db-writer stopped before replaceable write could be queued");
        return false;
    }
    true
}

/// Wait until every earlier channel message has been handled, or `timeout`
/// elapses. A retained job whose DB open still fails remains pending even when
/// this barrier acks. Used on workspace switches before resurrecting rows.
pub(crate) fn flush(timeout: Duration) -> bool {
    let (ack_tx, ack_rx) = sync_channel(0);
    if writer().tx.send(Msg::Flush(ack_tx)).is_err() {
        return false;
    }
    ack_rx.recv_timeout(timeout).is_ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShutdownFlush {
    pub(crate) drained: bool,
    pub(crate) pending: usize,
    pub(crate) dropped: u64,
    pub(crate) critical_lost: u64,
}

/// Bounded shutdown barrier with enough detail to report writes still queued or lost.
pub(crate) fn shutdown_flush(timeout: Duration) -> ShutdownFlush {
    shutdown_flush_for(writer(), timeout)
}

fn shutdown_flush_for(writer: &Writer, timeout: Duration) -> ShutdownFlush {
    let (ack_tx, ack_rx) = sync_channel(0);
    let acked = writer.tx.send(Msg::Flush(ack_tx)).is_ok() && ack_rx.recv_timeout(timeout).is_ok();
    let pending =
        writer.state.depth.load(Ordering::Acquire) + writer.state.in_flight.load(Ordering::Acquire);
    ShutdownFlush {
        drained: acked && pending == 0,
        pending,
        dropped: writer.state.dropped.load(Ordering::Relaxed),
        critical_lost: writer.state.critical_lost.load(Ordering::Relaxed),
    }
}

pub(crate) fn shutdown_notice(flush: &ShutdownFlush) -> Option<String> {
    if flush.drained && flush.dropped == 0 && flush.critical_lost == 0 {
        return None;
    }
    if flush.drained && flush.critical_lost == 0 {
        return Some(format!(
            "thegn: {} best-effort database write(s) were dropped this session",
            flush.dropped
        ));
    }
    let mut details = Vec::new();
    details.push(format!("{} write(s) still pending", flush.pending));
    if flush.dropped > 0 {
        details.push(format!("{} write(s) dropped", flush.dropped));
    }
    if flush.critical_lost > 0 {
        details.push(format!("{} required write(s) lost", flush.critical_lost));
    }
    Some(format!(
        "thegn: database shutdown flush incomplete ({})",
        details.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::WorkspaceStore;

    type TempWriter = (Writer, std::path::PathBuf, tempfile::TempDir);

    /// A writer over a private throwaway on-disk DB.
    fn temp_writer() -> TempWriter {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let p = path.clone();
        (spawn_writer(move || Db::open_at(&p)), path, dir)
    }

    fn barrier(writer: &Writer) {
        let (ack_tx, ack_rx) = sync_channel(0);
        writer.tx.send(Msg::Flush(ack_tx)).unwrap();
        ack_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn writes_run_in_fifo_order_last_wins() {
        let (writer, path, _dir) = temp_writer();
        for v in ["first", "second", "third"] {
            let v = v.to_string();
            assert!(enqueue(&writer, move |db| {
                let _ = db.set_ui_state("t", "k", &v); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            }));
        }
        barrier(&writer);
        // Reopen independently: the last queued write wins (FIFO).
        let db = Db::open_at(&path).unwrap();
        assert_eq!(db.get_ui_state("t", "k").unwrap().as_deref(), Some("third"));
    }

    #[test]
    fn flush_barrier_waits_for_queued_work() {
        let (writer, path, _dir) = temp_writer();
        assert!(enqueue(&writer, |db| {
            let _ = db.set_ui_state("t", "flushed", "yes"); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }));
        barrier(&writer); // must not return before the write above ran
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            db.get_ui_state("t", "flushed").unwrap().as_deref(),
            Some("yes")
        );
    }

    #[test]
    fn persist_latest_coalesces_queued_values_by_key() {
        let (writer, path, _dir) = temp_writer();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        assert!(enqueue(&writer, move |_db| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for value in ["first", "second", "third"] {
            let value = value.to_string();
            let ran = Arc::clone(&ran);
            assert!(enqueue_latest(
                &writer,
                "replaceable".to_string(),
                move |db| {
                    ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    db.set_ui_state("t", "latest", &value).unwrap();
                }
            ));
        }
        release_tx.send(()).unwrap();
        barrier(&writer);

        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            db.get_ui_state("t", "latest").unwrap().as_deref(),
            Some("third")
        );
    }

    #[test]
    fn lock_contention_does_not_block_enqueue_and_recovers_after_commit() {
        let (writer, path, _dir) = temp_writer();
        assert!(enqueue(&writer, |db| {
            db.set_ui_state("t", "ready", "yes").unwrap();
        }));
        barrier(&writer);
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        assert!(enqueue(&writer, |db| {
            db.set_ui_state("t", "locked", "written").unwrap();
        }));
        assert!(started.elapsed() < Duration::from_millis(50));

        let (ack_tx, ack_rx) = sync_channel(0);
        writer.tx.send(Msg::Flush(ack_tx)).unwrap();
        assert!(ack_rx.recv_timeout(Duration::from_millis(100)).is_err());

        lock.execute_batch("COMMIT").unwrap();
        ack_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            db.get_ui_state("t", "locked").unwrap().as_deref(),
            Some("written")
        );
    }

    #[test]
    fn open_failure_is_survived_without_panic() {
        // An opener that always fails: jobs remain retained, the thread stays
        // alive, and the flush barrier still acks.
        let writer = spawn_writer(|| anyhow::bail!("no db for you"));
        assert!(enqueue(&writer, |_db| {
            unreachable!("open never succeeds")
        }));
        let (ack_tx, ack_rx) = sync_channel(0);
        writer.tx.send(Msg::Flush(ack_tx)).unwrap();
        assert!(ack_rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert_eq!(writer.state.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn open_failure_retains_writes_until_open_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recovery.db");
        let opener_path = path.clone();
        let opens = Arc::new(AtomicUsize::new(0));
        let opener_count = Arc::clone(&opens);
        let writer = spawn_writer(move || {
            if opener_count.fetch_add(1, Ordering::SeqCst) == 0 {
                anyhow::bail!("transient open failure")
            }
            Db::open_at(&opener_path)
        });
        let order = Arc::new(Mutex::new(Vec::new()));
        for value in ["a", "b"] {
            let order = Arc::clone(&order);
            let value = value.to_string();
            assert!(enqueue(&writer, move |db| {
                order.lock().unwrap().push(value.clone());
                db.set_ui_state("t", "k", &value).unwrap();
            }));
        }
        barrier(&writer);

        assert_eq!(
            *order.lock().unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        let db = Db::open_at(&path).unwrap();
        assert_eq!(db.get_ui_state("t", "k").unwrap().as_deref(), Some("b"));
        assert_eq!(writer.state.dropped.load(Ordering::Relaxed), 0);
        assert!(opens.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn persistently_failing_open_counts_overflow_and_retained_evictions() {
        let writer = spawn_writer(|| anyhow::bail!("database stays unavailable"));
        let mut accepted = 0usize;
        let mut overflows = 0usize;
        for _ in 0..(MAX_QUEUED_JOBS + 3) {
            if enqueue(&writer, |_db| {}) {
                accepted += 1;
            } else {
                overflows += 1;
            }
        }
        barrier(&writer);

        let retained = MAX_QUEUED_JOBS - 1;
        let evictions = accepted - retained;
        assert_eq!(
            writer.state.dropped.load(Ordering::Relaxed),
            (overflows + evictions) as u64
        );
        assert_eq!(writer.state.depth.load(Ordering::Relaxed), retained);
        let (ack_tx, ack_rx) = sync_channel(0);
        writer.tx.send(Msg::Flush(ack_tx)).unwrap();
        assert!(ack_rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn send_to_stopped_writer_counts_as_a_drop() {
        let (tx, rx) = channel();
        drop(rx);
        let writer = Writer {
            tx,
            state: Arc::new(QueueState::default()),
        };

        assert!(!enqueue(&writer, |_db| {}));
        assert_eq!(writer.state.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(writer.state.depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn critical_send_to_stopped_writer_has_separate_loss_accounting() {
        let (tx, rx) = channel();
        drop(rx);
        let writer = Writer {
            tx,
            state: Arc::new(QueueState::default()),
        };

        assert!(!persist_critical_for(&writer, |_db| {}));
        assert_eq!(writer.state.dropped.load(Ordering::Relaxed), 0);
        assert_eq!(writer.state.critical_lost.load(Ordering::Relaxed), 1);
        assert_eq!(writer.state.depth.load(Ordering::Relaxed), 0);
        let notice = shutdown_notice(&ShutdownFlush {
            drained: false,
            pending: 0,
            dropped: 0,
            critical_lost: 1,
        })
        .unwrap();
        assert!(notice.contains("1 required write(s) lost"));
    }

    #[test]
    fn shutdown_notice_reports_pending_and_dropped_writes() {
        assert_eq!(
            shutdown_notice(&ShutdownFlush {
                drained: true,
                pending: 0,
                dropped: 0,
                critical_lost: 0,
            }),
            None
        );
        let timed_out = shutdown_notice(&ShutdownFlush {
            drained: false,
            pending: 3,
            dropped: 0,
            critical_lost: 0,
        })
        .unwrap();
        assert!(timed_out.contains("3 write(s) still pending"));
        let dropped = shutdown_notice(&ShutdownFlush {
            drained: true,
            pending: 0,
            dropped: 2,
            critical_lost: 0,
        })
        .unwrap();
        assert_eq!(
            dropped,
            "thegn: 2 best-effort database write(s) were dropped this session"
        );
    }

    #[test]
    fn shutdown_flush_reports_a_blocked_job_as_pending() {
        let (writer, _path, _dir) = temp_writer();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        assert!(enqueue(&writer, move |_db| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let result = shutdown_flush_for(&writer, Duration::from_millis(10));
        assert!(!result.drained);
        assert!(result.pending >= 1);

        release_tx.send(()).unwrap();
        barrier(&writer);
    }

    #[test]
    fn in_flight_guard_releases_count_during_unwind() {
        let state = QueueState::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _in_flight = InFlightGuard::new(&state);
            panic!("job panic");
        }));

        assert!(result.is_err());
        assert_eq!(state.in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn bounded_queue_drops_overflow_without_blocking() {
        let (writer, _path, _dir) = temp_writer();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        assert!(enqueue(&writer, move |_db| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let ran = Arc::new(AtomicUsize::new(0));
        let mut accepted = 0;
        let mut dropped = 0;
        for _ in 0..(MAX_QUEUED_JOBS + 5) {
            let ran = Arc::clone(&ran);
            let started = std::time::Instant::now();
            if enqueue(&writer, move |_db| {
                ran.fetch_add(1, Ordering::SeqCst);
            }) {
                accepted += 1;
            } else {
                dropped += 1;
            }
            assert!(started.elapsed() < Duration::from_millis(50));
        }
        assert_eq!(accepted, MAX_QUEUED_JOBS);
        assert_eq!(dropped, 5);
        assert_eq!(writer.state.dropped.load(Ordering::Relaxed), 5);

        release_tx.send(()).unwrap();
        barrier(&writer);
        assert_eq!(ran.load(Ordering::SeqCst), MAX_QUEUED_JOBS);
        assert_eq!(writer.state.depth.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn overflow_warning_rearms_after_queue_drains_below_half() {
        let state = QueueState::default();
        for _ in 0..MAX_QUEUED_JOBS {
            assert!(reserve(&state));
        }

        assert!(note_drop(&state));
        dequeue(&state);
        assert!(state.overflow_warned.load(Ordering::Acquire));
        assert!(!note_drop(&state));
        assert_eq!(state.dropped.load(Ordering::Relaxed), 2);
        assert!(state.overflow_warned.load(Ordering::Acquire));

        while state.depth.load(Ordering::Relaxed) > MAX_QUEUED_JOBS / 2 {
            dequeue(&state);
        }
        assert!(!state.overflow_warned.load(Ordering::Acquire));
        assert!(note_drop(&state));
    }

    #[test]
    fn critical_write_evicts_best_effort_and_runs_after_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("critical-recovery.db");
        let opener_path = path.clone();
        let recover = Arc::new(AtomicBool::new(false));
        let should_recover = Arc::clone(&recover);
        let writer = spawn_writer(move || {
            if !should_recover.load(Ordering::SeqCst) {
                anyhow::bail!("database remains unavailable until recovery")
            }
            Db::open_at(&opener_path)
        });
        let order = Arc::new(Mutex::new(Vec::new()));
        let initial_critical_order = Arc::clone(&order);
        assert!(persist_critical_for(&writer, move |_db| {
            initial_critical_order.lock().unwrap().push(usize::MIN);
        }));
        for value in 0..(MAX_QUEUED_JOBS - 1) {
            let order = Arc::clone(&order);
            assert!(enqueue(&writer, move |_db| {
                order.lock().unwrap().push(value);
            }));
        }
        barrier(&writer);

        let critical_ran = Arc::new(AtomicBool::new(false));
        let critical_ran_job = Arc::clone(&critical_ran);
        let critical_order = Arc::clone(&order);
        persist_critical_for(&writer, move |db| {
            db.set_ui_state("t", "required", "saved").unwrap();
            critical_ran_job.store(true, Ordering::SeqCst);
            critical_order.lock().unwrap().push(usize::MAX);
        });
        recover.store(true, Ordering::SeqCst);
        barrier(&writer);

        assert!(critical_ran.load(Ordering::SeqCst));
        assert_eq!(writer.state.dropped.load(Ordering::Relaxed), 1);
        assert_eq!(
            *order.lock().unwrap(),
            std::iter::once(usize::MIN)
                .chain(1..(MAX_QUEUED_JOBS - 1))
                .chain(std::iter::once(usize::MAX))
                .collect::<Vec<_>>()
        );
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            db.get_ui_state("t", "required").unwrap().as_deref(),
            Some("saved")
        );
    }

    #[test]
    fn critical_writes_are_accepted_past_the_queue_cap() {
        let writer = spawn_writer(|| anyhow::bail!("database stays unavailable"));
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..(MAX_QUEUED_JOBS + 3) {
            let ran = Arc::clone(&ran);
            assert!(persist_critical_for(&writer, move |_db| {
                ran.fetch_add(1, Ordering::SeqCst);
            }));
        }
        barrier(&writer);
        assert_eq!(
            writer.state.depth.load(Ordering::Relaxed),
            MAX_QUEUED_JOBS + 3
        );
        assert_eq!(writer.state.dropped.load(Ordering::Relaxed), 0);
        assert_eq!(writer.state.critical_lost.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn latest_slot_holds_one_job_per_key_under_contention() {
        let (writer, path, _dir) = temp_writer();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        assert!(enqueue(&writer, move |_db| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        }));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let ran = Arc::new(AtomicUsize::new(0));
        for value in 0..10_000 {
            let ran = Arc::clone(&ran);
            assert!(enqueue_latest(
                &writer,
                "replaceable".to_string(),
                move |db| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    db.set_ui_state("t", "latest-many", &value.to_string())
                        .unwrap();
                }
            ));
        }
        assert_eq!(writer.state.latest.lock().unwrap().len(), 1);
        assert!(writer.state.depth.load(Ordering::Relaxed) <= 1);

        release_tx.send(()).unwrap();
        barrier(&writer);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            db.get_ui_state("t", "latest-many").unwrap().as_deref(),
            Some("9999")
        );
    }
}
