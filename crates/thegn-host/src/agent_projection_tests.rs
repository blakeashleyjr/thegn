use super::{ProjectionLease, ProjectionRollback, projection_registry, rollback_action};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use thegn_core::config::DataMode;
use thegn_core::placement::Placement;
use thegn_core::projection::ProjectionSpec;

fn projection_spec() -> ProjectionSpec {
    ProjectionSpec {
        mode: DataMode::Sshfs,
        placement: Placement::Local,
        remote_dir: "/remote/worktree".to_string(),
        mountpoint: "/local/mountpoint".to_string(),
    }
}

fn worktree_path() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("agent-projection-lease-")
        .tempdir()
        .unwrap()
}

#[test]
fn uncommitted_lease_rolls_back_exactly_once_on_drop() {
    let worktree_dir = worktree_path();
    let worktree = worktree_dir.path().to_string_lossy().into_owned();
    let rollback_count = Arc::new(AtomicUsize::new(0));
    let mount_count = Arc::new(AtomicUsize::new(0));
    let rollback_counter = Arc::clone(&rollback_count);
    let mount_counter = Arc::clone(&mount_count);
    let mut warnings = Vec::new();

    let lease = ProjectionLease::acquire_with(
        &worktree,
        projection_spec(),
        move |_| {
            mount_counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        Box::new(move |_| {
            rollback_counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        "test",
        &mut warnings,
    );
    assert!(lease.is_some());
    drop(lease);

    assert_eq!(mount_count.load(Ordering::SeqCst), 1);
    assert_eq!(rollback_count.load(Ordering::SeqCst), 1);
    assert!(
        !projection_registry()
            .lock()
            .unwrap()
            .contains_key(&worktree)
    );
}

#[test]
fn committed_lease_registers_and_never_rolls_back() {
    let worktree_dir = worktree_path();
    let worktree = worktree_dir.path().to_string_lossy().into_owned();
    let rollback_count = Arc::new(AtomicUsize::new(0));
    let rollback_counter = Arc::clone(&rollback_count);
    let mut warnings = Vec::new();

    let lease = ProjectionLease::acquire_with(
        &worktree,
        projection_spec(),
        |_| Ok(()),
        Box::new(move |_| {
            rollback_counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        "test",
        &mut warnings,
    )
    .unwrap();
    lease.commit();

    assert_eq!(rollback_count.load(Ordering::SeqCst), 0);
    let registered = projection_registry().lock().unwrap().remove(&worktree);
    assert!(registered.is_some());
}

#[test]
fn failed_mount_yields_no_lease_and_warns() {
    let worktree_dir = worktree_path();
    let worktree = worktree_dir.path().to_string_lossy().into_owned();
    let rollback_count = Arc::new(AtomicUsize::new(0));
    let rollback_counter = Arc::clone(&rollback_count);
    let mut warnings = Vec::new();

    let lease = ProjectionLease::acquire_with(
        &worktree,
        projection_spec(),
        |_| Err(anyhow::anyhow!("mount rejected")),
        Box::new(move |_| {
            rollback_counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        "fixture",
        &mut warnings,
    );

    assert!(lease.is_none());
    assert_eq!(rollback_count.load(Ordering::SeqCst), 0);
    assert!(warnings.iter().any(|warning| {
        warning.contains("projection (fixture) failed:") && warning.contains("mount rejected")
    }));
}

#[test]
fn sync_rollback_never_pushes() {
    assert_eq!(rollback_action(DataMode::Sync), ProjectionRollback::Nothing);
    assert_eq!(
        rollback_action(DataMode::Sshfs),
        ProjectionRollback::Unmount
    );
    assert_eq!(
        rollback_action(DataMode::InEnv),
        ProjectionRollback::Nothing
    );
    assert_eq!(
        rollback_action(DataMode::LocalExec),
        ProjectionRollback::Nothing
    );
}
