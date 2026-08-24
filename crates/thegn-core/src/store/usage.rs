//! The **usage-history** seam: a rolling record of each AI account's rate-limit
//! windows, one row per window per poll.
//!
//! This is what turns a percentage into a trend — the sparkline in the Usage
//! panel section and the "you'll hit the cap at …" forecast both read it. A pure
//! cache: the provider is the source of truth, so it is always safe to drop and
//! let the next poll repopulate.

use anyhow::Result;

/// One recorded observation of a window.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageSample {
    /// The account's stable identity (`usage::AccountUsage::key`) — NOT a
    /// credential path, so history survives a home being moved or re-adopted.
    pub account_key: String,
    pub window: String,
    pub used_percent: f32,
    pub resets_at: Option<i64>,
    /// Epoch seconds.
    pub sampled_at: i64,
}

/// Object-safe (`&self` + concrete args), so `&dyn UsageStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait UsageStore {
    /// Record a batch of observations (one poll's worth), in one transaction.
    fn put_usage_samples(&self, samples: &[UsageSample]) -> Result<()>;

    /// One window's history at or after `since`, oldest first.
    fn usage_history(
        &self,
        account_key: &str,
        window: &str,
        since: i64,
    ) -> Result<Vec<UsageSample>>;

    /// Drop samples older than `before`. Returns the number removed.
    fn prune_usage_samples(&self, before: i64) -> Result<usize>;
}
