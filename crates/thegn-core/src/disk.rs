//! Per-worktree disk-usage measurement.
//!
//! Each worktree's `target/` is the dominant disk cost when developing across
//! many worktrees (a single populated `target/` is multiple GiB). This module
//! measures the whole checkout and the `target/` subtree so the UI can surface
//! sizes, warn past a threshold, and offer to reclaim regenerable build bytes.
//!
//! **Cost.** Walking a cold 70G `target/` is seconds-long, so this MUST run
//! off the event loop (the caller scans on `spawn_blocking` and caches the
//! result in the DB). Nothing here touches the compositor.
//!
//! The walk is native and parallel. It used to shell out to `du -sb` twice per
//! worktree — once for the checkout and again for `target/`, re-walking the
//! subtree that dominates the first number — with a single-threaded Rust
//! fallback when `du` was missing. One in-process traversal now produces both
//! totals, which removes a subprocess from a lane that was already logging
//! "background lane full — deferring round".

use std::path::Path;

/// Bytes used by a worktree: the whole checkout and its `target/` subtree, plus
/// when it was last touched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Apparent bytes of the entire worktree directory.
    pub total_bytes: u64,
    /// Apparent bytes of the `target/` subtree (0 if absent).
    pub target_bytes: u64,
    /// Newest file mtime anywhere in the worktree, as Unix seconds; 0 when
    /// nothing was readable. Source edits AND build writes both bump it, so it
    /// is the "last time anyone used this worktree" signal the idle-reclaim
    /// policy needs (`disk_reclaim`). It falls out of the same walk for free —
    /// every entry is already being `stat`ed for its length.
    pub newest_mtime: u64,
}

/// Measure a worktree's disk usage in ONE parallel walk.
///
/// This used to shell out to `du -sb` twice — once for the checkout, then again
/// for `target/` — which is close to double work, because `target/` is the bulk
/// of what it just walked. It was also the third consumer of the fs-watcher
/// storm (see `git_watch::plan_watches`): rounds were arriving faster than they
/// could finish, logging "background lane full — deferring round".
///
/// Now one traversal accumulates both totals, in-process, with no subprocess to
/// spawn and reap. Returns zeroes for a missing path rather than erroring — a
/// vanished worktree simply reports nothing.
pub fn measure_worktree(path: &Path) -> DiskUsage {
    if !path.exists() {
        return DiskUsage::default();
    }
    walk_usage(path)
}

/// Threads for the size walk. Bounded well below the core count: this runs on
/// the background measurement lane beside builds and the compositor, and the
/// walk is dominated by `stat` latency rather than CPU, so more threads buy
/// little and cost scheduler pressure. Override with `THEGN_DISK_SCAN_THREADS`.
fn scan_threads() -> usize {
    std::env::var("THEGN_DISK_SCAN_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4)
}

/// One parallel walk of `root`, summing apparent bytes for the whole tree and
/// for its `target/` subtree.
///
/// Deliberately walks EVERYTHING — `ignore`'s standard filters are all off. The
/// crate is used here purely as a fast parallel walker; applying gitignore
/// rules would skip `target/`, which is the number this scan exists to report so
/// the UI can offer to reclaim it.
///
/// Symlinks are not followed (matching the previous behavior, and `du`'s
/// default): following them can leave the tree entirely and double-count.
///
/// **Hardlinks are counted per path**, where `du` counts the inode once. This is
/// the *apparent size of the tree as laid out*, and it is exactly what the Rust
/// walk here has always reported — so nothing regressed when `du` was dropped,
/// though the `du`-specific behavior is now gone rather than merely unreachable.
/// Deduplicating would need `st_dev`/`st_ino`, which `std::fs::Metadata` only
/// exposes behind a `#[cfg(unix)]` extension trait, and this crate is
/// substrate-agnostic by contract (`test/platform-cfg-core-ratchet.txt`). Build
/// trees very rarely hardlink, so the divergence is not worth an OS split here.
fn walk_usage(root: &Path) -> DiskUsage {
    use std::sync::atomic::{AtomicU64, Ordering};

    let target = root.join("target");
    let total = AtomicU64::new(0);
    let target_total = AtomicU64::new(0);
    let newest = AtomicU64::new(0);

    let mut b = ignore::WalkBuilder::new(root);
    b.standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .threads(scan_threads());

    b.build_parallel().run(|| {
        Box::new(|entry| {
            let Ok(entry) = entry else {
                // Unreadable entry: skip it rather than aborting the walk.
                return ignore::WalkState::Continue;
            };
            // `file_type` is None only for the root when it can't be stat'd.
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return ignore::WalkState::Continue;
            }
            let Ok(meta) = entry.metadata() else {
                return ignore::WalkState::Continue;
            };
            // mtime BEFORE the zero-length early-out: a build's marker files
            // (`.cargo-lock`, empty stamps) are often zero bytes and are exactly
            // the entries that say "a build ran here".
            if let Ok(mtime) = meta.modified()
                && let Ok(since) = mtime.duration_since(std::time::UNIX_EPOCH)
            {
                newest.fetch_max(since.as_secs(), Ordering::Relaxed);
            }
            let len = meta.len();
            if len == 0 {
                return ignore::WalkState::Continue;
            }
            total.fetch_add(len, Ordering::Relaxed);
            if entry.path().starts_with(&target) {
                target_total.fetch_add(len, Ordering::Relaxed);
            }
            ignore::WalkState::Continue
        })
    });

    DiskUsage {
        total_bytes: total.load(Ordering::Relaxed),
        target_bytes: target_total.load(Ordering::Relaxed),
        newest_mtime: newest.load(Ordering::Relaxed),
    }
}

/// Recursive apparent-size sum, not following symlinks. Retained as the simple,
/// obviously-correct reference the parallel walk is checked against in tests.
pub fn walk_size(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            total += walk_size(&entry.path());
        }
    }
    total
}

/// Whether `child` lies strictly beneath `root` (never equal to it).
fn is_under(root: &Path, child: &Path) -> bool {
    child != root && child.starts_with(root)
}

/// A repo root's *own* bytes: its measured total minus every already-measured
/// worktree strictly beneath it.
///
/// Workspace main checkouts are measured now (they never were), and under
/// `[worktree] worktree_mode = "in_repo"` a repo's worktrees live at
/// `<root>/.worktrees/<slug>` — inside the very tree `du` just walked. Summing
/// the root alongside its children would count those bytes twice in the sidebar
/// badge and the statusbar total.
///
/// Done as arithmetic on values already in the cache rather than a second `du
/// --exclude` pass: `--exclude` is a GNU extension that macOS's `du` lacks, and
/// this costs no extra process. Saturating, so a stale or over-large child can
/// never underflow the root to a wrapped huge number.
pub fn net_root_bytes(root: &Path, root_total: u64, children: &[(&Path, u64)]) -> u64 {
    let nested: u64 = children
        .iter()
        .filter(|(p, _)| is_under(root, p))
        .map(|(_, b)| *b)
        .sum();
    root_total.saturating_sub(nested)
}

/// Grand total across cache entries, counting each byte once: an entry nested
/// inside another is folded into its ancestor instead of summed alongside it.
///
/// Each entry is `(path, total_bytes, target_bytes)`; the return is
/// `(total, target)`. This is what the statusbar's disk-warning rollup reads —
/// a naive `values().sum()` double-counts every `in_repo` worktree and can trip
/// the warning threshold on a repo that is nowhere near it.
pub fn grand_total(entries: &[(&Path, u64, u64)]) -> (u64, u64) {
    entries
        .iter()
        .filter(|(p, _, _)| {
            // Skip any entry that has an ancestor in the same set — its bytes
            // are already inside that ancestor's `du`.
            !entries.iter().any(|(other, _, _)| is_under(other, p))
        })
        .fold((0u64, 0u64), |(t, g), (_, total, target)| {
            (t.saturating_add(*total), g.saturating_add(*target))
        })
}

/// Human-readable byte count: `B`, `K`, `M`, `G`, `T` (binary units, one
/// decimal place above bytes, trimmed of a trailing `.0`).
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes}B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let s = format!("{value:.1}");
    let s = s.strip_suffix(".0").unwrap_or(&s);
    format!("{s}{}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tg-disk-{tag}-{}", std::process::id()));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn human_formats_binary_units() {
        assert_eq!(human(0), "0B");
        assert_eq!(human(512), "512B");
        assert_eq!(human(1024), "1KB");
        assert_eq!(human(1536), "1.5KB");
        assert_eq!(human(1024 * 1024), "1MB");
        assert_eq!(human(70 * 1024 * 1024 * 1024), "70GB");
        assert_eq!(human(1024_u64.pow(4)), "1TB");
    }

    #[test]
    fn walk_size_sums_files_and_recurses() {
        let dir = temp_dir("walk");
        std::fs::write(dir.join("a.bin"), vec![0u8; 1000]).unwrap();
        let sub = dir.join("target");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("b.bin"), vec![0u8; 2000]).unwrap();

        assert_eq!(walk_size(&sub), 2000);
        assert_eq!(walk_size(&dir), 3000);
        assert_eq!(walk_size(&dir.join("missing")), 0);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn measure_worktree_splits_target_from_total() {
        let dir = temp_dir("measure");
        std::fs::write(dir.join("src.rs"), vec![0u8; 1000]).unwrap();
        let target = dir.join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("artifact"), vec![0u8; 4000]).unwrap();

        let u = measure_worktree(&dir);
        // `du` rounds to block size, so assert relationships, not exact bytes.
        assert!(u.target_bytes >= 4000, "target counts the artifact");
        assert!(u.total_bytes >= u.target_bytes, "total includes target");
        assert!(u.total_bytes >= 5000, "total counts source + target");

        // Missing path → zeroes, never panics.
        assert_eq!(measure_worktree(&dir.join("gone")), DiskUsage::default());
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parallel_walk_agrees_with_the_reference_recursion() {
        // The parallel walk and the simple recursion must produce the same
        // number on a nested tree. `walk_size` is the obviously-correct
        // reference; `measure_worktree` is the fast path that replaced `du`.
        let dir = temp_dir("agree");
        for (i, sub) in ["", "a", "a/b", "target", "target/deep/deeper"]
            .iter()
            .enumerate()
        {
            let d = if sub.is_empty() {
                dir.clone()
            } else {
                dir.join(sub)
            };
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("f.bin"), vec![0u8; 100 * (i + 1)]).unwrap();
        }
        let u = measure_worktree(&dir);
        assert_eq!(
            u.total_bytes,
            walk_size(&dir),
            "total matches the reference"
        );
        assert_eq!(
            u.target_bytes,
            walk_size(&dir.join("target")),
            "target subtotal matches the reference"
        );
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardlinks_are_counted_per_path() {
        // Documented divergence from `du`, which dedups by inode: this reports
        // the apparent size of the tree as laid out, matching what the Rust walk
        // here always did. Pinned so the choice is deliberate rather than
        // rediscovered as a bug — see `walk_usage`'s doc comment for why
        // deduplicating is not worth a `#[cfg(unix)]` in this crate.
        let dir = temp_dir("hardlink");
        std::fs::write(dir.join("orig.bin"), vec![0u8; 5000]).unwrap();
        std::fs::hard_link(dir.join("orig.bin"), dir.join("link.bin")).unwrap();
        let u = measure_worktree(&dir);
        assert_eq!(u.total_bytes, 10_000, "both links are counted");
        assert_eq!(
            u.total_bytes,
            walk_size(&dir),
            "and the fast walk still agrees with the reference recursion"
        );
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_mtime_tracks_the_most_recent_touch_including_zero_length_markers() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("src.rs"), b"fn main() {}").unwrap();
        // A build's lock marker is zero-length; it must still count as a touch.
        std::fs::write(dir.join("target/debug/.cargo-lock"), b"").unwrap();
        let u = measure_worktree(dir);
        let now = crate::util::now() as u64;
        assert!(u.newest_mtime > 0, "an mtime was recorded");
        assert!(
            u.newest_mtime + 300 >= now && u.newest_mtime <= now + 300,
            "newest_mtime {} is around now {now}",
            u.newest_mtime
        );
        // An empty directory has no files at all, so no mtime is reported.
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(measure_worktree(empty.path()).newest_mtime, 0);
    }

    #[test]
    fn measure_worktree_handles_no_target() {
        let dir = temp_dir("notarget");
        std::fs::write(dir.join("only.txt"), vec![0u8; 100]).unwrap();
        let u = measure_worktree(&dir);
        assert_eq!(u.target_bytes, 0, "no target/ subtree");
        assert!(u.total_bytes >= 100);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_submodule_bytes_stay_in_the_owning_physical_total() {
        let dir = temp_dir("submodule-physical");
        std::fs::write(
            dir.join(".gitmodules"),
            b"[submodule \"lib\"]\npath = lib\nurl = x\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("lib/src")).unwrap();
        std::fs::write(dir.join("lib/src/lib.rs"), vec![0u8; 7000]).unwrap();

        let usage = measure_worktree(&dir);
        assert_eq!(
            usage.total_bytes,
            walk_size(&dir),
            "a checked-out submodule is physical content of its owner"
        );
        assert!(usage.total_bytes >= 7000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn net_root_bytes_subtracts_in_repo_worktrees() {
        // `worktree_mode = "in_repo"`: the worktrees live inside the root's tree.
        let children = [
            (p("/repo/.worktrees/feat"), 300u64),
            (p("/repo/.worktrees/fix"), 200),
        ];
        assert_eq!(net_root_bytes(p("/repo"), 1000, &children), 500);
    }

    #[test]
    fn net_root_bytes_ignores_siblings_and_the_root_itself() {
        // `worktree_mode = "global"` (the default): worktrees live elsewhere, so
        // nothing is subtracted.
        let children = [(p("/elsewhere/feat"), 300u64), (p("/repo"), 1000)];
        assert_eq!(net_root_bytes(p("/repo"), 1000, &children), 1000);
    }

    /// A child measured before the root shrank would otherwise wrap to a huge
    /// number and blow past the disk warning threshold.
    #[test]
    fn net_root_bytes_saturates_on_a_stale_child() {
        let children = [(p("/repo/.worktrees/feat"), 9_000u64)];
        assert_eq!(net_root_bytes(p("/repo"), 1000, &children), 0);
    }

    #[test]
    fn net_root_bytes_with_no_children_is_the_total() {
        assert_eq!(net_root_bytes(p("/repo"), 1234, &[]), 1234);
    }

    #[test]
    fn grand_total_counts_nested_paths_once() {
        let entries = [
            (p("/repo"), 1000u64, 400u64),
            (p("/repo/.worktrees/feat"), 300, 100),
            (p("/other"), 50, 0),
        ];
        // The nested worktree is already inside /repo's du: 1000 + 50, not 1350.
        assert_eq!(grand_total(&entries), (1050, 400));
    }

    #[test]
    fn grand_total_sums_disjoint_paths() {
        let entries = [
            (p("/wt/a"), 100u64, 40u64),
            (p("/wt/b"), 200, 60),
            (p("/wt/c"), 300, 0),
        ];
        assert_eq!(grand_total(&entries), (600, 100));
    }

    #[test]
    fn grand_total_of_nothing_is_zero() {
        assert_eq!(grand_total(&[]), (0, 0));
    }

    /// A sibling whose path merely shares a textual prefix (`/wt/ab` vs `/wt/a`)
    /// is NOT nested — `starts_with` is component-wise, and this pins that.
    #[test]
    fn grand_total_does_not_treat_prefix_siblings_as_nested() {
        let entries = [(p("/wt/a"), 100u64, 0u64), (p("/wt/ab"), 200, 0)];
        assert_eq!(grand_total(&entries), (300, 0));
    }
}
