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
//! whose rows may still be queued — only a SIGKILL can drop a layout write.
//!
//! The thread blocks on `recv()` (no polling, no timer, no `TerminalWaker`
//! pulse), so it adds zero idle wakes — the 0%-idle contract is untouched.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use thegn_core::db::Db;

/// A unit of best-effort work run against the writer's connection.
type Job = Box<dyn FnOnce(&Db) + Send>;

enum Msg {
    Run(Job),
    RunLatest {
        key: String,
        generation: u64,
        job: Job,
    },
    /// A barrier: the writer acks once every earlier `Run` has completed.
    Flush(SyncSender<()>),
}

struct Writer {
    tx: Sender<Msg>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

fn writer() -> &'static Writer {
    static W: OnceLock<Writer> = OnceLock::new();
    W.get_or_init(|| {
        let generations = Arc::new(Mutex::new(HashMap::new()));
        Writer {
            tx: spawn_with_generations(Db::open, Arc::clone(&generations)),
            generations,
        }
    })
}

/// Spawn the writer thread with a connection opener and return its send handle.
/// Split from [`writer`] so tests can drive [`run`] with an isolated DB.
fn spawn_with_generations(
    open: impl Fn() -> anyhow::Result<Db> + Send + 'static,
    generations: Arc<Mutex<HashMap<String, u64>>>,
) -> Sender<Msg> {
    let (tx, rx) = channel::<Msg>();
    std::thread::Builder::new()
        .name("thegn-db-writer".into())
        .spawn(move || run(rx, open, generations))
        .expect("spawn db-writer thread");
    tx
}

/// Drain jobs in FIFO order against a lazily-opened connection. A failed open is
/// logged once and its job dropped (best-effort semantics); the next job retries
/// the open, so a transient failure self-heals. Returns when the channel closes.
fn run(
    rx: Receiver<Msg>,
    open: impl Fn() -> anyhow::Result<Db>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
) {
    let mut db: Option<Db> = None;
    let mut warned = false;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Run(job) => {
                run_job(&mut db, &mut warned, &open, job);
            }
            Msg::RunLatest {
                key,
                generation,
                job,
            } => {
                let is_latest = generations
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&key)
                    .copied()
                    == Some(generation);
                if is_latest {
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
/// silently ignored — the DB is a cache.
pub(crate) fn persist(job: impl FnOnce(&Db) + Send + 'static) {
    let _ = writer().tx.send(Msg::Run(Box::new(job))); // best-effort: send: the consumer may be gone; a closed channel is the consumer going away
}

/// Queue a replaceable write. A queued write is skipped if a newer value for
/// the same key arrived before the writer began it.
pub(crate) fn persist_latest(key: String, job: impl FnOnce(&Db) + Send + 'static) {
    let writer = writer();
    enqueue_latest(&writer.tx, &writer.generations, key, Box::new(job));
}

fn enqueue_latest(
    tx: &Sender<Msg>,
    generations: &Mutex<HashMap<String, u64>>,
    key: String,
    job: Job,
) {
    // Keys must come from a small fixed set so this generation map stays bounded.
    let mut generations = generations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let generation = generations.get(&key).copied().unwrap_or(0).wrapping_add(1);
    generations.insert(key.clone(), generation);
    let _ = tx.send(Msg::RunLatest {
        key,
        generation,
        job,
    }); // best-effort: send: the consumer may be gone; the DB is a cache
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

    type TempWriter = (
        Sender<Msg>,
        std::path::PathBuf,
        Arc<Mutex<HashMap<String, u64>>>,
    );

    /// A writer over a throwaway on-disk DB, plus a flush helper bound to its
    /// own channel (not the process-global one). The dir is unique per CALL
    /// (pid + counter), not just per process — the module's tests run as
    /// parallel threads of one process, and a shared dir would let one test's
    /// `remove_dir_all` delete another's live DB mid-test.
    fn temp_writer() -> TempWriter {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tg-dbtask-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let p = path.clone();
        let generations = Arc::new(Mutex::new(HashMap::new()));
        let tx = spawn_with_generations(move || Db::open_at(&p), Arc::clone(&generations));
        (tx, path, generations)
    }

    fn barrier(tx: &Sender<Msg>) {
        let (ack_tx, ack_rx) = sync_channel(0);
        tx.send(Msg::Flush(ack_tx)).unwrap();
        ack_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn writes_run_in_fifo_order_last_wins() {
        let (tx, path, _) = temp_writer();
        for v in ["first", "second", "third"] {
            let v = v.to_string();
            tx.send(Msg::Run(Box::new(move |db| {
                let _ = db.set_ui_state("t", "k", &v); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            })))
            .unwrap();
        }
        barrier(&tx);
        // Reopen independently: the last queued write wins (FIFO).
        let db = Db::open_at(&path).unwrap();
        assert_eq!(db.get_ui_state("t", "k").unwrap().as_deref(), Some("third"));
    }

    #[test]
    fn flush_barrier_waits_for_queued_work() {
        let (tx, path, _) = temp_writer();
        tx.send(Msg::Run(Box::new(|db| {
            let _ = db.set_ui_state("t", "flushed", "yes"); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        })))
        .unwrap();
        barrier(&tx); // must not return before the write above ran
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            db.get_ui_state("t", "flushed").unwrap().as_deref(),
            Some("yes")
        );
    }

    #[test]
    fn persist_latest_coalesces_queued_values_by_key() {
        let (tx, path, generations) = temp_writer();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        tx.send(Msg::Run(Box::new(move |_db| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })))
        .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();

        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for value in ["first", "second", "third"] {
            let value = value.to_string();
            let ran = Arc::clone(&ran);
            enqueue_latest(
                &tx,
                &generations,
                "replaceable".to_string(),
                Box::new(move |db| {
                    ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    db.set_ui_state("t", "latest", &value).unwrap();
                }),
            );
        }
        release_tx.send(()).unwrap();
        barrier(&tx);

        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
        let db = Db::open_at(&path).unwrap();
        assert_eq!(
            db.get_ui_state("t", "latest").unwrap().as_deref(),
            Some("third")
        );
    }

    #[test]
    fn lock_contention_does_not_block_enqueue_and_recovers_after_commit() {
        let (tx, path, _) = temp_writer();
        tx.send(Msg::Run(Box::new(|db| {
            db.set_ui_state("t", "ready", "yes").unwrap();
        })))
        .unwrap();
        barrier(&tx);
        let lock = rusqlite::Connection::open(&path).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        tx.send(Msg::Run(Box::new(|db| {
            db.set_ui_state("t", "locked", "written").unwrap();
        })))
        .unwrap();
        assert!(started.elapsed() < Duration::from_millis(50));

        let (ack_tx, ack_rx) = sync_channel(0);
        tx.send(Msg::Flush(ack_tx)).unwrap();
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
        let tx = spawn_with_generations(
            || anyhow::bail!("no db for you"),
            Arc::new(Mutex::new(HashMap::new())),
        );
        tx.send(Msg::Run(Box::new(|_db| {
            unreachable!("open never succeeds")
        })))
        .unwrap();
        let (ack_tx, ack_rx) = sync_channel(0);
        tx.send(Msg::Flush(ack_tx)).unwrap();
        assert!(ack_rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }
}
