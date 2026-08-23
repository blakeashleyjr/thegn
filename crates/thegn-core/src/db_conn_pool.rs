//! A process-wide pool of SQLite connections, keyed by resolved database path.
//!
//! `Db::open()` is called from ~311 sites in the host, ~40 of them on the event
//! loop itself, and each call used to mean a real `sqlite3_open`: a new file
//! handle, a new page cache, a WAL-index mmap, three pragmas and a
//! `user_version` query. Measured warm on Windows
//! (`cargo bench -p thegn-core --bench core_hot -- db/open_at_warm`):
//! **~2.6 ms per open**, dominated by the file open — a machine with a
//! security agent scanning every `CreateFile` pays that on every one.
//!
//! Pooling keeps the *connection* alive across opens. It deliberately does NOT
//! cache the schema check: [`Db::open`] still runs the cheap `user_version`
//! query on checkout, so a migration written by another process is still
//! noticed exactly as before. Only the expensive part — opening the file — is
//! skipped.
//!
//! ## Why keyed on the resolved path
//!
//! `db_path()` re-reads `XDG_STATE_HOME` / `%LOCALAPPDATA%` on **every** call,
//! and the test suite repoints those constantly (the `state-db` spec requires
//! test isolation of state). A single global connection would hand a test the
//! previous test's database. Keying on the resolved path means a test that
//! repoints its state dir simply lands in a different bucket, and
//! `Db::open_memory` / `Db::open_at` are not pooled at all.
//!
//! ## Why `Connection` and not `Db`
//!
//! `rusqlite::Connection` is `Send` but `!Sync`, so it cannot be shared — only
//! moved. The pool moves connections in and out under a `Mutex`, which is what
//! makes the pool itself `Sync`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::Connection;

/// Idle connections retained per database path.
///
/// Sized against the host's real concurrency: `sched::BG_PERMITS` bounds
/// background work at 8, plus the event loop, the refresh ticker, the DB writer
/// thread and headroom. Beyond this a returned connection is simply closed —
/// the pool never blocks a caller and never grows without bound.
const MAX_IDLE_PER_PATH: usize = 12;

fn pool() -> &'static Mutex<HashMap<PathBuf, Vec<Connection>>> {
    static POOL: OnceLock<Mutex<HashMap<PathBuf, Vec<Connection>>>> = OnceLock::new();
    POOL.get_or_init(Default::default)
}

/// Take an idle connection for `path`, if one is parked.
pub(crate) fn take(path: &Path) -> Option<Connection> {
    let mut p = pool().lock().unwrap_or_else(|e| e.into_inner());
    p.get_mut(path).and_then(Vec::pop)
}

/// Park `conn` for reuse. Dropped instead when the bucket is already full.
pub(crate) fn put(path: &Path, conn: Connection) {
    let mut p = pool().lock().unwrap_or_else(|e| e.into_inner());
    let slot = p.entry(path.to_path_buf()).or_default();
    if slot.len() < MAX_IDLE_PER_PATH {
        slot.push(conn);
    }
    // else: drop, closing it.
}

/// Close every parked connection.
///
/// Tests that delete a state directory need this: an open handle keeps the file
/// alive on Windows, and a stale pooled connection would otherwise serve a
/// database the test believes it removed.
pub fn clear() {
    let mut p = pool().lock().unwrap_or_else(|e| e.into_inner());
    p.clear();
}

/// Number of parked connections for `path` — for tests and diagnostics.
pub fn idle_count(path: &Path) -> usize {
    let p = pool().lock().unwrap_or_else(|e| e.into_inner());
    p.get(path).map_or(0, Vec::len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tg-pool-{}-{tag}.db", std::process::id()))
    }

    /// A returned connection is handed back out, not reopened.
    #[test]
    fn round_trips_a_connection() {
        let path = tmp("roundtrip");
        clear();
        assert_eq!(idle_count(&path), 0, "starts empty");
        assert!(take(&path).is_none(), "nothing parked yet");

        put(&path, Connection::open_in_memory().unwrap());
        assert_eq!(idle_count(&path), 1);

        let got = take(&path);
        assert!(got.is_some(), "the parked connection comes back");
        assert_eq!(idle_count(&path), 0, "checkout removes it from the pool");
        clear();
    }

    /// Buckets are per-path, so a test that repoints its state dir can never be
    /// served another path's database.
    #[test]
    fn buckets_do_not_leak_across_paths() {
        let a = tmp("iso-a");
        let b = tmp("iso-b");
        clear();
        put(&a, Connection::open_in_memory().unwrap());
        assert_eq!(idle_count(&a), 1);
        assert_eq!(idle_count(&b), 0, "a different path sees nothing");
        assert!(take(&b).is_none());
        assert!(take(&a).is_some());
        clear();
    }

    /// The pool is bounded: past the cap, returned connections are closed
    /// rather than accumulating.
    #[test]
    fn retains_at_most_the_cap() {
        let path = tmp("cap");
        clear();
        for _ in 0..(MAX_IDLE_PER_PATH + 5) {
            put(&path, Connection::open_in_memory().unwrap());
        }
        assert_eq!(
            idle_count(&path),
            MAX_IDLE_PER_PATH,
            "bucket must not grow past the cap"
        );
        clear();
    }

    #[test]
    fn clear_closes_everything() {
        let path = tmp("clear");
        clear();
        put(&path, Connection::open_in_memory().unwrap());
        put(&path, Connection::open_in_memory().unwrap());
        assert_eq!(idle_count(&path), 2);
        clear();
        assert_eq!(idle_count(&path), 0);
    }
}
