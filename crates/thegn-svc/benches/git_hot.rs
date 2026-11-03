//! Micro-benchmarks for the per-worktree git hot path.
//!
//! `is_dirty` / `ahead_behind` / `current_branch` are what the 2s model-refresh
//! ticker fans across every worktree (`hydrate.rs`) — the dominant idle cost
//! the perf investigation traced. These benches make that cost A/B-able:
//!   * each op individually, and the combined "model scan" (all three), which
//!     is exactly what the ticker does per worktree;
//!   * parametrized by worktree count (1 / 4 / 14) to expose scaling;
//!   * gix-native (`GixGit`) vs subprocess (`CliGit`) so the provider choice is
//!     a measured number, not folklore.
//!   * worktree-vs-HEAD diff entry collection (`diff_files`) at 40 / 2,000
//!     files, with gix-native vs CLI timings.
//!
//! Run all groups with `cargo bench -p thegn-svc --bench git_hot`; run only the
//! diff comparison with `cargo bench -p thegn-svc --bench git_hot -- diff_files`.
//! Debug-vs-release is `cargo bench` (release) vs `cargo bench --profile dev`.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use thegn_core::remote::GitLoc;
use thegn_svc::git::{CliGit, GitBackend, GixGit};

#[path = "support/fixture.rs"]
mod fixture;

const COUNTS: [usize; 3] = [1, 4, 14];

/// One full per-worktree scan, as the ticker fans it: dirty + ahead/behind +
/// current branch for every worktree.
fn model_scan<G: GitBackend>(git: &G, locs: &[GitLoc]) {
    for loc in locs {
        black_box(git.is_dirty(loc).unwrap_or(false));
        black_box(git.ahead_behind(loc).ok().flatten().unwrap_or((0, 0)));
        black_box(git.current_branch(loc).ok());
    }
}

fn bench_git_hot(c: &mut Criterion) {
    // Build one fixture per worktree count up front; the ops are read-only so a
    // single fixture is reused across all iterations.
    let fixtures: Vec<(usize, fixture::GitFixture)> = COUNTS
        .iter()
        .map(|&n| (n, fixture::build(n, n.min(2))))
        .collect();

    let gix = GixGit::new();

    let mut single = c.benchmark_group("gix_ops_14wt");
    if let Some((_, f)) = fixtures.iter().find(|(n, _)| *n == 14) {
        single.bench_function("is_dirty", |b| {
            b.iter(|| {
                for loc in &f.worktrees {
                    black_box(gix.is_dirty(loc).unwrap_or(false));
                }
            })
        });
        single.bench_function("ahead_behind", |b| {
            b.iter(|| {
                for loc in &f.worktrees {
                    black_box(gix.ahead_behind(loc).ok().flatten());
                }
            })
        });
        single.bench_function("current_branch", |b| {
            b.iter(|| {
                for loc in &f.worktrees {
                    black_box(gix.current_branch(loc).ok());
                }
            })
        });
    }
    single.finish();

    // The full model scan, parametrized by worktree count — gix vs CLI.
    let mut scan = c.benchmark_group("model_scan");
    for (n, f) in &fixtures {
        scan.bench_with_input(BenchmarkId::new("gix", n), n, |b, _| {
            b.iter(|| model_scan(&gix, &f.worktrees))
        });
        scan.bench_with_input(BenchmarkId::new("cli", n), n, |b, _| {
            b.iter(|| model_scan(&CliGit, &f.worktrees))
        });
    }
    scan.finish();

    // Compare uncommitted worktree edits against HEAD through both backends.
    // GixGit falls back to git on native errors, so hide git from PATH during
    // its count check and timing; failures then surface instead of timing CLI.
    // Criterion's warm iterations hit native_diff's stat-keyed memo.
    let mut diffs = c.benchmark_group("diff_files");
    for n in [40, 2_000] {
        let f = fixture::build_diff(n);
        let loc = &f.worktrees[0];
        let cli_count = CliGit.diff_files(loc, "HEAD").unwrap().len();
        let empty_bin = f.dir.path().join("empty-bin");
        std::fs::create_dir(&empty_bin).unwrap();

        let path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &empty_bin);
        }
        let gix_count = gix.diff_files(loc, "HEAD").unwrap().len();
        unsafe {
            match path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }

        assert_eq!(
            gix_count, cli_count,
            "diff entry count differs for {n} files"
        );

        let path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", &empty_bin);
        }
        diffs.bench_with_input(BenchmarkId::new("gix", n), &n, |b, _| {
            b.iter(|| black_box(gix.diff_files(loc, "HEAD").unwrap().len()))
        });
        unsafe {
            match path {
                Some(path) => std::env::set_var("PATH", path),
                None => std::env::remove_var("PATH"),
            }
        }

        diffs.bench_with_input(BenchmarkId::new("cli", n), &n, |b, _| {
            b.iter(|| black_box(CliGit.diff_files(loc, "HEAD").unwrap().len()))
        });
    }
    diffs.finish();
}

criterion_group!(benches, bench_git_hot);
criterion_main!(benches);
