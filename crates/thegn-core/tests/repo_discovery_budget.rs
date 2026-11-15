//! Budget, cancellation and pruning matrix for bounded repository discovery (THE-367).
//!
//! The zero-elapsed budget is a stand-in for a slow mount: a real mount-boundary
//! fixture needs privileges, so it is not exercised here.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use thegn_core::config::Config;
use thegn_core::repo::{DiscoveryBudget, DiscoveryReport, discover_repos_bounded};

fn config(root: &Path) -> Config {
    config_roots(&[root.to_path_buf()])
}

fn config_roots(roots: &[PathBuf]) -> Config {
    Config {
        repo_roots: roots
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect(),
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

fn make_repo(path: &Path) {
    std::fs::create_dir_all(path.join(".git")).unwrap();
}

fn three_repos() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    for name in ["a", "b", "c"] {
        make_repo(&root.join(name));
    }
    (temp, root)
}

#[test]
fn git_internals_are_never_visited() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    let repo = root.join("repo");
    let objects = repo.join(".git/objects/xx");
    std::fs::create_dir_all(&objects).unwrap();
    for i in 0..3000 {
        std::fs::write(objects.join(format!("{i:038x}")), b"").unwrap();
    }
    std::fs::create_dir_all(root.join("sibling")).unwrap();
    std::fs::write(root.join("sibling/file.txt"), b"").unwrap();

    let (report, found) = walk(&config(&root), DiscoveryBudget::default());

    assert_eq!(found, vec![repo.to_string_lossy().into_owned()]);
    assert_eq!(report.repositories, 1);
    // root, repo, repo/.git, sibling, sibling/file.txt = 5; allow a margin.
    assert!(
        report.visited_entries < 20,
        "visited {} entries; .git internals must be skipped",
        report.visited_entries
    );
    assert!(!report.truncated && !report.cancelled);
}

#[test]
fn max_entries_budget_truncates_and_reports_what_was_emitted() {
    let (_temp, root) = three_repos();
    let (report, found) = walk(
        &config(&root),
        DiscoveryBudget {
            max_entries: 1,
            ..DiscoveryBudget::default()
        },
    );
    assert!(report.truncated);
    assert!(!report.cancelled);
    assert_eq!(report.visited_entries, 1);
    assert_eq!(report.repositories, found.len());
}

#[test]
fn max_repositories_budget_truncates_but_keeps_found_repos() {
    let (_temp, root) = three_repos();
    let (report, found) = walk(
        &config(&root),
        DiscoveryBudget {
            max_repositories: 1,
            ..DiscoveryBudget::default()
        },
    );
    assert!(report.truncated);
    assert!(!report.cancelled);
    assert_eq!(found.len(), 1);
    assert_eq!(report.repositories, found.len());
}

#[test]
fn max_single_path_bytes_budget_truncates_on_a_deep_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    make_repo(&root.join("a/b/c/d/repo"));
    let limit = root.to_string_lossy().len();
    let (report, found) = walk(
        &config(&root),
        DiscoveryBudget {
            max_single_path_bytes: limit,
            ..DiscoveryBudget::default()
        },
    );
    assert!(report.truncated);
    assert!(!report.cancelled);
    assert!(found.is_empty());
    assert_eq!(report.repositories, 0);
    assert_eq!(report.path_bytes, 0);
}

#[test]
fn max_path_bytes_budget_truncates_and_keeps_accounting_consistent() {
    let (_temp, root) = three_repos();
    let (report, found) = walk(
        &config(&root),
        DiscoveryBudget {
            max_path_bytes: 1,
            ..DiscoveryBudget::default()
        },
    );
    assert!(report.truncated);
    assert!(!report.cancelled);
    assert_eq!(report.repositories, found.len());
    assert!(report.path_bytes <= 1);
}

#[test]
fn zero_elapsed_budget_terminates_truncated_as_a_slow_mount_stand_in() {
    let (_temp, root) = three_repos();
    let (report, found) = walk(
        &config(&root),
        DiscoveryBudget {
            max_elapsed: Duration::ZERO,
            ..DiscoveryBudget::default()
        },
    );
    assert!(report.truncated);
    assert!(!report.cancelled);
    assert_eq!(report.visited_entries, 0);
    assert_eq!(report.repositories, found.len());
}

#[test]
fn symlink_loop_does_not_hang_and_counts_no_phantom_repos() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_path_buf();
    make_repo(&root.join("repo"));
    let linked = std::process::Command::new("ln")
        .arg("-s")
        .arg(&root)
        .arg(root.join("loop"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !thegn_core::testenv::require_or_skip("symlink creation via ln -s", linked) {
        return;
    }

    let (report, found) = walk(&config(&root), DiscoveryBudget::default());

    assert_eq!(
        found,
        vec![root.join("repo").to_string_lossy().into_owned()]
    );
    assert_eq!(report.repositories, 1);
    assert!(!report.truncated && !report.cancelled);
}

#[test]
fn cancel_before_first_batch_reports_cancelled_and_emits_nothing() {
    let (_temp, root) = three_repos();
    let mut emitted = 0usize;
    let report = discover_repos_bounded(
        &config(&root),
        DiscoveryBudget::default(),
        &AtomicBool::new(true),
        |batch| {
            emitted += batch.len();
            true
        },
    );
    assert!(report.cancelled);
    assert!(!report.truncated);
    assert_eq!(emitted, 0);
    assert_eq!(report.repositories, 0);
}

#[test]
fn emit_returning_false_stops_the_walk() {
    let (_temp, root) = three_repos();
    let mut calls = 0usize;
    let report = discover_repos_bounded(
        &config(&root),
        DiscoveryBudget {
            batch_size: 1,
            ..DiscoveryBudget::default()
        },
        &AtomicBool::new(false),
        |_| {
            calls += 1;
            false
        },
    );
    assert!(report.cancelled);
    assert!(!report.truncated);
    assert_eq!(calls, 1);
    assert_eq!(report.repositories, 1);
}

#[test]
fn missing_root_is_counted_and_other_roots_still_walk() {
    let (_temp, root) = three_repos();
    let missing = root.join("does-not-exist");
    let (report, found) = walk(
        &config_roots(&[missing, root.clone()]),
        DiscoveryBudget::default(),
    );
    assert_eq!(report.missing_roots, 1);
    assert_eq!(report.repositories, 3);
    assert_eq!(found.len(), 3);
    assert!(!report.truncated && !report.cancelled);
}

#[test]
fn hidden_and_node_modules_dirs_are_pruned_but_a_hidden_root_is_walked() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().to_path_buf();
    let root = base.join(".hidden-root");
    make_repo(&root.join("visible"));
    make_repo(&root.join(".cache/buried"));
    make_repo(&root.join("node_modules/dep"));
    make_repo(&root.join("target/artifact"));

    let (report, found) = walk(&config(&root), DiscoveryBudget::default());

    assert_eq!(
        found,
        vec![root.join("visible").to_string_lossy().into_owned()]
    );
    assert_eq!(report.repositories, 1);
}

#[test]
fn truncated_and_cancelled_are_distinguishable_in_the_report() {
    let (_temp, root) = three_repos();
    let cfg = config(&root);

    let (truncated, _) = walk(
        &cfg,
        DiscoveryBudget {
            max_repositories: 1,
            ..DiscoveryBudget::default()
        },
    );
    assert!(truncated.truncated && !truncated.cancelled);

    let cancelled = discover_repos_bounded(
        &cfg,
        DiscoveryBudget::default(),
        &AtomicBool::new(true),
        |_| true,
    );
    assert!(cancelled.cancelled && !cancelled.truncated);

    let (complete, _) = walk(&cfg, DiscoveryBudget::default());
    assert!(!complete.truncated && !complete.cancelled);
}
