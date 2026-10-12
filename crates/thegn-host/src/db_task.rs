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
//! **Scope: best-effort cache writes only.** Git is the source of truth and the
//! DB is a cache (see CLAUDE.md), so a write dropped on a hard exit is
//! acceptable here. The session-layout persist is also routed through this
//! thread (`run::persist_session_layout` captures rows on the loop and queues
//! the transaction): main.rs flushes the queue on every clean exit, and the
//! cold workspace-switch path calls [`flush`] before resurrecting a session
//! whose rows may still be queued. The bounded-queue overflow policy can also
//! drop a newly submitted layout write; critical-write failure surfacing is a
//! separate follow-up.
//!
//! The thread blocks on `recv()` (no polling, no timer, no `TerminalWaker`
//! pulse), so it adds zero idle wakes — the 0%-idle contract is untouched.
//!
//! The FIFO is bounded. Overflow drops new best-effort writes and preserves
//! the order of writes already queued; [`dropped_writes`] exposes the count
//! for a later UI surfacing slice. Replaceable writes retain one pending job
//! per key, regardless of how often a caller updates that key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use thegn_core::db::Db;

/// A unit of best-effort work run against the writer's connection.
type Job = Box<dyn FnOnce(&Db) + Send>;

/// The production cap bounds memory while allowing ordinary bursts to drain.
#[cfg(not(test))]
const MAX_QUEUED_JOBS: usize = 4096;
#[cfg(test)]
const MAX_QUEUED_JOBS: usize = 8;

enum Msg {
    Run(Job),
    RunLatest {
        key: String,
    },
    /// A barrier: the writer acks once every earlier `Run` has completed.
    Flush(SyncSender<()>),
}

struct Writer {
    tx: Sender<Msg>,
    state: Arc<QueueState>,
}

#[derive(Default)]
struct QueueState {
    latest: Mutex<HashMap<String, Job>>,
    depth: AtomicUsize,
    dropped: AtomicU64,
    overflow_warned: AtomicBool,
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

/// Drain jobs in FIFO order against a lazily-opened connection. A failed open is
/// logged once and its job dropped (best-effort semantics); the next job retries
/// the open, so a transient failure self-heals. Returns when the channel closes.
fn run(rx: Receiver<Msg>, open: impl Fn() -> anyhow::Result<Db>, state: Arc<QueueState>) {
    let mut db: Option<Db> = None;
    let mut warned = false;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Run(job) => {
                dequeue(&state);
                run_job(&mut db, &mut warned, &open, job);
            }
            Msg::RunLatest { key } => {
                let job = {
                    let mut latest = state
                        .latest
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let job = latest.remove(&key);
                    dequeue(&state);
                    job
                };
                if let Some(job) = job {
                    run_job(&mut db, &mut warned, &open, job);
                }
            }
            // FIFO: acking here means every prior Run has already run.
            Msg::Flush(ack) => {
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

fn note_drop(state: &QueueState) {
    state.dropped.fetch_add(1, Ordering::Relaxed);
    if !state.overflow_warned.swap(true, Ordering::AcqRel) {
        tracing::warn!(
            dropped = state.dropped.load(Ordering::Relaxed),
            "db-writer queue full; dropping new best-effort write"
        );
    }
}

fn enqueue(writer: &Writer, job: impl FnOnce(&Db) + Send + 'static) -> bool {
    if !reserve(&writer.state) {
        note_drop(&writer.state);
        return false;
    }
    if let Err(error) = writer.tx.send(Msg::Run(Box::new(job))) {
        dequeue(&writer.state);
        tracing::debug!(%error, "db-writer stopped before write could be queued");
        return false;
    }
    true
}

fn run_job(
    db: &mut Option<Db>,
    warned: &mut bool,
    open: &impl Fn() -> anyhow::Result<Db>,
    job: Job,
) {
    if db.is_none() {
        match open() {
            Ok(d) => {
                *db = Some(d);
                *warned = false;
            }
            Err(e) => {
                if !*warned {
                    tracing::warn!("db-writer: open failed, dropping write: {e}");
                    *warned = true;
                }
                return;
            }
        }
    }
    if let Some(d) = db {
        job(d);
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
        tracing::debug!(%error, "db-writer stopped before replaceable write could be queued");
        return false;
    }
    true
}

/// Number of best-effort writes rejected because the bounded queue was full.
#[allow(dead_code)] // Exposed for the follow-up UI surfacing slice.
pub(crate) fn dropped_writes() -> u64 {
    writer().state.dropped.load(Ordering::Relaxed)
}

/// Block until every previously-queued write has run, or `timeout` elapses
/// (returns whether it drained in time). Called on the clean quit path so
/// best-effort persists aren't lost at process exit.
pub(crate) fn flush(timeout: Duration) -> bool {
    let (ack_tx, ack_rx) = sync_channel(0);
    if writer().tx.send(Msg::Flush(ack_tx)).is_err() {
        return false;
    }
    ack_rx.recv_timeout(timeout).is_ok()
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
        // An opener that always fails: jobs are dropped, the thread stays alive,
        // and the flush barrier still acks.
        let writer = spawn_writer(|| anyhow::bail!("no db for you"));
        assert!(enqueue(&writer, |_db| {
            unreachable!("open never succeeds")
        }));
        let (ack_tx, ack_rx) = sync_channel(0);
        writer.tx.send(Msg::Flush(ack_tx)).unwrap();
        assert!(ack_rx.recv_timeout(Duration::from_secs(5)).is_ok());
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

        note_drop(&state);
        dequeue(&state);
        assert!(state.overflow_warned.load(Ordering::Acquire));
        note_drop(&state);
        assert_eq!(state.dropped.load(Ordering::Relaxed), 2);
        assert!(state.overflow_warned.load(Ordering::Acquire));

        while state.depth.load(Ordering::Relaxed) > MAX_QUEUED_JOBS / 2 {
            dequeue(&state);
        }
        assert!(!state.overflow_warned.load(Ordering::Acquire));
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
