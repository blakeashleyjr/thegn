//! The **zone** seam: per-profile grouping of workspaces (the DB half — a
//! zone's *existence* and *membership*). A zone is a named soft firewall inside
//! one profile: a credential sub-vault plus egress/budget ceilings over its
//! member workspaces (the policy half lives in config, `[zone.<name>]`, and the
//! resolution/enforcement in [`crate::zone`]).
//!
//! Membership is daemon/DB-tracked, never inferred from a spoofable filesystem
//! path. [`crate::db::Db`] is the embedded-SQLite implementation
//! (`db_zones.rs`); the DB is already per-profile (profiles reroot
//! `XDG_STATE_HOME`), so zone rows are profile-scoped for free.

use anyhow::Result;

/// A zone row plus its live member count.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneRow {
    pub zone_id: i64,
    pub name: String,
    pub created_at: i64,
    pub member_count: i64,
}

/// The outcome of a delete attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ZoneDeleteOutcome {
    Deleted,
    /// Refused because the zone still has members (pass `force` to unassign +
    /// delete). Carries the member count.
    RefusedNonEmpty(i64),
}

/// Persisted zone existence + membership. Object-safe (`&self` + concrete args).
pub trait ZoneStore {
    /// Create a zone, returning its id. Fails if the name is already taken.
    fn create_zone(&self, name: &str, now: i64) -> Result<i64>;

    /// Rename a zone.
    fn rename_zone(&self, zone_id: i64, new_name: &str) -> Result<()>;

    /// Delete a zone. Refuses when members exist unless `force` (which first
    /// unassigns every member).
    fn delete_zone(&self, zone_id: i64, force: bool) -> Result<ZoneDeleteOutcome>;

    /// All zones with member counts, ordered by name.
    fn list_zones(&self) -> Result<Vec<ZoneRow>>;

    /// Assign (or, with `None`, unassign) a workspace's zone.
    fn assign_workspace_zone(&self, repo_path: &str, zone: Option<i64>) -> Result<()>;

    /// The zone a workspace belongs to (by repo path), or `None` if unzoned.
    fn zone_of_workspace(&self, repo_path: &str) -> Result<Option<ZoneRow>>;

    /// The zone a *worktree* belongs to: worktree → its `repo_path` → the
    /// workspace's zone. Falls back to treating the arg as a repo path (home-tab
    /// panes). `None` if unzoned or unknown.
    fn zone_of_worktree(&self, worktree: &str) -> Result<Option<ZoneRow>>;
}
