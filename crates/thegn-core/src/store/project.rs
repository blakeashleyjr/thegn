//! The **project** seam: per-profile grouping of workspaces ABOVE the workspace
//! layer (the DB half — a project's *existence*, *membership*, and header
//! *order*). A project is a named group of repos worked on together (a
//! microservice feature spanning `api`, `web`, `shared-lib`).
//!
//! This is the zones *shape* with workflow semantics instead of policy:
//! membership carries ZERO policy — assigning a project never re-scopes
//! credentials, egress, budget, or sandbox (that is [`crate::zone`]'s exclusive
//! job). A workspace may belong to one zone AND one project simultaneously, and
//! a project may span zones.
//!
//! Membership is DB-tracked, never inferred from a spoofable filesystem path.
//! [`crate::db::Db`] is the embedded-SQLite implementation (`db_projects.rs`);
//! the DB is already per-profile (profiles reroot `XDG_STATE_HOME`), so project
//! rows are profile-scoped for free. No cross-repo feature-link rows are stored:
//! git stays the sole source of truth per repo (feature sets are derived).

use anyhow::Result;

/// A project row plus its live member count.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRow {
    pub project_id: i64,
    pub name: String,
    pub created_at: i64,
    /// Manual sidebar-header ordering (exact-order persistence via
    /// [`ProjectStore::set_project_order`]). Lower sorts first.
    pub position: i64,
    pub member_count: i64,
}

/// The outcome of a delete attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectDeleteOutcome {
    Deleted,
    /// Refused because the project still has members (pass `force` to unassign +
    /// delete). Carries the member count.
    RefusedNonEmpty(i64),
}

/// Persisted project existence + membership + header order. Object-safe
/// (`&self` + concrete args), mirroring [`crate::store::ZoneStore`].
pub trait ProjectStore {
    /// Create a project, returning its id. Fails if the name is already taken.
    fn create_project(&self, name: &str, now: i64) -> Result<i64>;

    /// Rename a project.
    fn rename_project(&self, project_id: i64, new_name: &str) -> Result<()>;

    /// Delete a project. Refuses when members exist unless `force` (which first
    /// unassigns every member).
    fn delete_project(&self, project_id: i64, force: bool) -> Result<ProjectDeleteOutcome>;

    /// All projects with member counts, ordered by `position` then name.
    fn list_projects(&self) -> Result<Vec<ProjectRow>>;

    /// Assign (or, with `None`, unassign) a workspace's project (by repo path).
    fn assign_workspace_project(&self, repo_path: &str, project: Option<i64>) -> Result<()>;

    /// The project a workspace belongs to (by repo path), or `None` if
    /// unprojected.
    fn project_of_workspace(&self, repo_path: &str) -> Result<Option<ProjectRow>>;

    /// A project's member workspaces as `(repo_path, name)`, in the sidebar's
    /// workspace order (deterministic — drives the per-member batched-create
    /// order). Empty when the project has no members.
    fn project_members(&self, project_id: i64) -> Result<Vec<(String, String)>>;

    /// Persist the full sidebar order of project headers: writes
    /// `position = index` for each id in `order` under one transaction, so the
    /// stored order is exactly the sequence the sidebar is showing (the same
    /// exact-order style as `set_workspace_order`).
    fn set_project_order(&self, order: &[i64]) -> Result<()>;
}
