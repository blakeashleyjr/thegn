//! The **account** seam: per-provider coding-agent accounts (config dir,
//! last-used stamp, active pointer).

use anyhow::Result;

/// Object-safe (`&self` + concrete args), so `&dyn AccountStore` works for
/// backend-agnostic consumers. [`crate::db::Db`] is the embedded-SQLite impl.
pub trait AccountStore {
    /// The credential-home dir for a managed account, `None` if unknown.
    fn account_dir(&self, provider: &str, name: &str) -> Result<Option<String>>;

    /// Every managed account for a provider, as `(name, dir, managed)`.
    fn list_accounts(&self, provider: &str) -> Result<Vec<(String, String, bool)>>;

    /// Register (or update) an account's credential-home dir.
    fn put_account(
        &self,
        provider: &str,
        name: &str,
        dir: &str,
        managed: bool,
        now_ms: i64,
    ) -> Result<()>;

    /// Mark an account as just used (for picker ordering).
    fn touch_account(&self, provider: &str, name: &str, now_ms: i64) -> Result<()>;

    /// Forget a managed account (does not delete its on-disk credential dir).
    fn del_account(&self, provider: &str, name: &str) -> Result<()>;
}
