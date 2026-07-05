//! The **intent** seam: the CLI→compositor mailbox (`superzej open`).
//!
//! A short-lived CLI process enqueues an intent row; the live compositor's
//! model refresh claims-and-deletes pending rows on its next tick (~1s), the
//! same DB-as-mailbox pattern notifications use. No IPC by design.

use anyhow::Result;

/// One pending intent row.
#[derive(Debug, Clone)]
pub struct IntentRow {
    pub id: i64,
    pub kind: String,
    /// Kind-specific JSON payload (e.g. [`crate::models::FocusIntent`]).
    pub payload: String,
    pub created_at: i64,
}

/// Object-safe (`&self` + concrete args), so `&dyn IntentStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait IntentStore {
    /// Enqueue an intent. Returns the new row id.
    fn put_intent(&self, kind: &str, payload: &str) -> Result<i64>;

    /// Atomically claim-and-delete every pending intent of `kind`, FIFO by
    /// insertion order. Consumers apply what they need (typically the last
    /// row wins for focus-style intents) and drop the rest.
    fn take_intents(&self, kind: &str) -> Result<Vec<IntentRow>>;
}
