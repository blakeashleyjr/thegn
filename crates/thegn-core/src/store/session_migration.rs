//! Store seam for the two independent SQLite stores used by session migration.

use crate::session_migration::{
    MigrationBundle, MigrationCleanupResult, MigrationImportResult, MigrationPlan, MigrationTarget,
};
use anyhow::Result;

/// Synchronous, backend-agnostic persistence seam for session migration.
///
/// Each mutating method is one store-local transaction. The trait intentionally
/// has no daemon/config/profile-environment operations: the host owns those
/// boundaries and supplies the two already-open stores.
pub trait SessionMigrationStore {
    /// Read the exact source worktree presentation and its allowlisted rows.
    fn migration_snapshot(
        &self,
        source_profile: &str,
        target_profile: &str,
        active_session: &str,
        worktree_path: &str,
    ) -> Result<MigrationBundle>;

    /// Read target rows needed for pure conflict/resume planning.
    fn migration_target_snapshot(
        &self,
        active_session: &str,
        worktree_path: &str,
    ) -> Result<MigrationTarget>;

    /// Import a validated plan in one target transaction.
    fn import_migration(&self, plan: &MigrationPlan) -> Result<MigrationImportResult>;

    /// Read back the allowlisted target rows after import.
    fn confirm_migration(&self, plan: &MigrationPlan) -> Result<bool>;

    /// Delete only the exact source rows represented by the confirmed bundle.
    fn cleanup_migration(&self, bundle: &MigrationBundle) -> Result<MigrationCleanupResult>;
}
