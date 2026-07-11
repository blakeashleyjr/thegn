//! Small shared aliases/helpers used across the proxy crate.

use std::sync::{Arc, Mutex};

use thegn_core::db::Db;

/// The proxy daemon owns a single SQLite connection, guarded by a mutex. DB ops
/// are quick and synchronous; the lock is never held across an `.await`.
/// `rusqlite::Connection` is `Send` but not `Sync`, so `Mutex<Db>` makes the
/// handle shareable across tokio tasks.
pub type SharedDb = Arc<Mutex<Db>>;

/// Current wall-clock time in epoch milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Current wall-clock time in epoch seconds (for OpenAI `created` fields).
pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}
