//! Integration coverage for traversal errors during bounded repository discovery.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use thegn_core::config::Config;
use thegn_core::repo::{DiscoveryBudget, DiscoveryReport, discover_repos_bounded};

/// Restores the mode of a locked directory on drop so tempdir cleanup succeeds even when a test panics.
struct Locked(PathBuf);

impl Drop for Locked {
    fn drop(&mut self) {
        // Best-effort cleanup: tempdir removal reports a real failure if this does not work.
        if let Err(error) =
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755))
        {
            tracing::debug!(path = %self.0.display(), %error, "could not unlock discovery test fixture");
        }
    }
}

fn fixture() -> Option<(tempfile::TempDir, PathBuf, Locked)> {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    std::fs::create_dir_all(root.join("repo/.git")).unwrap();
    std::fs::create_dir_all(root.join("other/.git")).unwrap();
    let locked = root.join("locked");
    std::fs::create_dir_all(locked.join("inner")).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let locked_guard = Locked(locked.clone());
    if !thegn_core::testenv::require_or_skip(
        "a non-root user that cannot read a mode 000 directory",
        std::fs::read_dir(&locked).is_err(),
    ) {
        return None;
    }
    Some((temp, root, locked_guard))
}

fn config(root: &Path) -> Config {
    Config {
        repo_roots: vec![root.to_string_lossy().into_owned()],
        ..Config::default()
    }
}

fn walk(cfg: &Config, budget: DiscoveryBudget) -> (DiscoveryReport, Vec<String>) {
    let mut found = Vec::new();
    let report = discover_repos_bounded(cfg, budget, &AtomicBool::new(false), |batch| {
        found.extend(batch);
        true
    });
    (report, found)
}

#[test]
fn discovery_traversal_error_is_counted_and_does_not_stop_the_walk() {
    let Some((_temp, root, _locked)) = fixture() else {
        return;
    };
    let (report, mut found) = walk(&config(&root), DiscoveryBudget::default());

    assert_eq!(report.traversal_errors, 1);
    assert!(!report.truncated);
    assert!(!report.cancelled);
    assert_eq!(report.repositories, 2);
    found.sort();
    let mut expected = vec![
        root.join("repo").to_string_lossy().into_owned(),
        root.join("other").to_string_lossy().into_owned(),
    ];
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn discovery_traversal_error_budget_truncates_the_walk() {
    let Some((_temp, root, _locked)) = fixture() else {
        return;
    };
    let (report, found) = walk(
        &config(&root),
        DiscoveryBudget {
            max_errors: 1,
            ..DiscoveryBudget::default()
        },
    );

    assert_eq!(report.traversal_errors, 1);
    assert!(report.truncated);
    let allowed = [
        root.join("repo").to_string_lossy().into_owned(),
        root.join("other").to_string_lossy().into_owned(),
    ];
    assert!(found.iter().all(|path| allowed.contains(path)));
}
