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
//! acceptable here. The *critical* session-layout persist stays synchronous on
//! its own paths — it is not routed through this thread. On a clean quit the
//! loop calls [`flush`] so queued writes land before exit.
//!
//! The thread blocks on `recv()` (no polling, no timer, no `TerminalWaker`
//! pulse), so it adds zero idle wakes — the 0%-idle contract is untouched.

use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::time::Duration;

use thegn_core::db::Db;

/// A unit of best-effort work run against the writer's connection.
type Job = Box<dyn FnOnce(&Db) + Send>;

enum Msg {
    Run(Job),
    /// A barrier: the writer acks once every earlier `Run` has completed.
    Flush(SyncSender<()>),
}

struct Writer {
    tx: Sender<Msg>,
}

fn writer() -> &'static Writer {
    static W: OnceLock<Writer> = OnceLock::new();
    W.get_or_init(|| Writer {
        tx: spawn(Db::open),
    })
}

/// Spawn the writer thread with a connection opener and return its send handle.
/// Split from [`writer`] so tests can drive [`run`] with an isolated DB.
fn spawn(open: impl Fn() -> anyhow::Result<Db> + Send + 'static) -> Sender<Msg> {
    let (tx, rx) = channel::<Msg>();
    std::thread::Builder::new()
        .name("thegn-db-writer".into())
        .spawn(move || run(rx, open))
        .expect("spawn db-writer thread");
    tx
}

/// Drain jobs in FIFO order against a lazily-opened connection. A failed open is
/// logged once and its job dropped (best-effort semantics); the next job retries
/// the open, so a transient failure self-heals. Returns when the channel closes.
fn run(rx: Receiver<Msg>, open: impl Fn() -> anyhow::Result<Db>) {
    let mut db: Option<Db> = None;
    let mut warned = false;
    while let Ok(msg) = rx.recv() {
        match msg {
            Msg::Run(job) => {
                if db.is_none() {
                    match open() {
                        Ok(d) => {
                            db = Some(d);
                            warned = false;
                        }
                        Err(e) => {
                            if !warned {
                                tracing::warn!("db-writer: open failed, dropping write: {e}");
                                warned = true;
                            }
                            continue;
                        }
                    }
                }
                if let Some(d) = &db {
                    job(d);
                }
            }
            // FIFO: acking here means every prior Run has already run.
            Msg::Flush(ack) => {
                let _ = ack.send(());
            }
        }
    }
}

/// Queue a best-effort DB write to run on the writer thread, in send order.
/// Fire-and-forget: returns immediately and never blocks the caller, so it is
/// safe to call from the event loop. Failure to enqueue (writer gone) is
/// silently ignored — the DB is a cache.
pub(crate) fn persist(job: impl FnOnce(&Db) + Send + 'static) {
    let _ = writer().tx.send(Msg::Run(Box::new(job)));
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

    /// A writer over a throwaway on-disk DB, plus a flush helper bound to its
    /// own channel (not the process-global one). The dir is unique per CALL
    /// (pid + counter), not just per process — the module's tests run as
    /// parallel threads of one process, and a shared dir would let one test's
    /// `remove_dir_all` delete another's live DB mid-test.
    fn temp_writer() -> (Sender<Msg>, std::path::PathBuf) {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tg-dbtask-{}-{seq}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let p = path.clone();
        let tx = spawn(move || Db::open_at(&p));
        (tx, path)
    }

    fn barrier(tx: &Sender<Msg>) {
        let (ack_tx, ack_rx) = sync_channel(0);
        tx.send(Msg::Flush(ack_tx)).unwrap();
        ack_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn writes_run_in_fifo_order_last_wins() {
        let (tx, path) = temp_writer();
        for v in ["first", "second", "third"] {
            let v = v.to_string();
            tx.send(Msg::Run(Box::new(move |db| {
                let _ = db.set_ui_state("t", "k", &v);
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
        let (tx, path) = temp_writer();
        tx.send(Msg::Run(Box::new(|db| {
            let _ = db.set_ui_state("t", "flushed", "yes");
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
    fn open_failure_is_survived_without_panic() {
        // An opener that always fails: jobs are dropped, the thread stays alive,
        // and the flush barrier still acks.
        let tx = spawn(|| anyhow::bail!("no db for you"));
        tx.send(Msg::Run(Box::new(|_db| {
            unreachable!("open never succeeds")
        })))
        .unwrap();
        let (ack_tx, ack_rx) = sync_channel(0);
        tx.send(Msg::Flush(ack_tx)).unwrap();
        assert!(ack_rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }
}
