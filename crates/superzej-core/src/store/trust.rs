//! The **repo-trust** seam: trust-on-first-use approvals for a repo-root
//! `.superzej.*` overlay's *gated* sandbox requests (extra mounts, init/prepare
//! scripts, image, ports, gpu, nix-daemon). The config-resolution clamp
//! ([`crate::config_resolve`]) never applies these without a matching approval.
//!
//! Each row records one decision keyed by `(repo_root, request_json)` — the
//! canonical request JSON is the security match key, so a later edit to the
//! requested set produces a different key and re-prompts. `request_id` is a
//! short display handle only.
//!
//! [`crate::db::Db`] is the embedded-SQLite implementation (`db_trust.rs`); a
//! server backend would persist approvals per user against Postgres.

use anyhow::Result;

/// A persisted trust decision for one gated repo request.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoTrustRow {
    /// Short display handle (a hash of `request_json`) for the CLI/UI.
    pub request_id: String,
    /// Canonical request JSON — the match key.
    pub request_json: String,
    /// `"approved"` or `"denied"`.
    pub decision: String,
    /// Unix seconds when the decision was recorded.
    pub decided_at: i64,
}

/// Persisted trust-on-first-use approvals. Object-safe (`&self` + concrete
/// args), so `&dyn RepoTrustStore` works for backend-agnostic consumers.
pub trait RepoTrustStore {
    /// Record (or replace) a decision for one gated request.
    fn repo_trust_decide(
        &self,
        repo_root: &str,
        request_id: &str,
        request_json: &str,
        decision: &str,
        now: i64,
    ) -> Result<()>;

    /// Forget a decision (by canonical request JSON) so it re-prompts.
    fn repo_trust_revoke(&self, repo_root: &str, request_json: &str) -> Result<()>;

    /// All recorded decisions for a repo, newest first.
    fn repo_trust_list(&self, repo_root: &str) -> Result<Vec<RepoTrustRow>>;

    /// The canonical request JSONs *approved* for this repo — feeds
    /// [`crate::config_resolve::Approvals::from_canonical`].
    fn repo_trust_approved(&self, repo_root: &str) -> Result<Vec<String>>;
}
