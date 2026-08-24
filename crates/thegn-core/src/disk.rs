//! Per-worktree disk-usage measurement.
//!
//! Each worktree's `target/` is the dominant disk cost when developing across
//! many worktrees (a single populated `target/` is multiple GiB). This module
//! measures the whole checkout and the `target/` subtree so the UI can surface
//! sizes, warn past a threshold, and offer to reclaim regenerable build bytes.
//!
//! **Cost.** A `du` of a cold 70G `target/` is seconds-long, so this MUST run
//! off the event loop (the caller scans on `spawn_blocking` and caches the
//! result in the DB). Nothing here touches the compositor.

use std::path::Path;
use std::process::Command;

use crate::util;

/// Bytes used by a worktree: the whole checkout and its `target/` subtree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskUsage {
    /// Apparent bytes of the entire worktree directory.
    pub total_bytes: u64,
    /// Apparent bytes of the `target/` subtree (0 if absent).
    pub target_bytes: u64,
}

/// Measure a worktree's disk usage. Prefers `du` (a tuned C tool that
/// single-syscalls per dirent and dedups hardlinks) and falls back to a Rust
/// walk when `du` is unavailable. Returns zeroes for a missing path rather than
/// erroring — a vanished worktree simply reports nothing.
pub fn measure_worktree(path: &Path) -> DiskUsage {
    if !path.exists() {
        return DiskUsage::default();
    }
    let target = path.join("target");
    if util::have("du") {
        let total_bytes = du_bytes(path).unwrap_or_else(|| walk_size(path));
        let target_bytes = if target.is_dir() {
            du_bytes(&target).unwrap_or_else(|| walk_size(&target))
        } else {
            0
        };
        DiskUsage {
            total_bytes,
            target_bytes,
        }
    } else {
        DiskUsage {
            total_bytes: walk_size(path),
            target_bytes: if target.is_dir() {
                walk_size(&target)
            } else {
                0
            },
        }
    }
}

/// `du -sb <path>` → leading byte count. `None` if `du` failed or produced
/// unparseable output (caller falls back to the Rust walk).
fn du_bytes(path: &Path) -> Option<u64> {
    let out = Command::new("du").arg("-sb").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // Output is "<bytes>\t<path>"; take the leading integer.
    text.split_whitespace().next()?.parse::<u64>().ok()
}

/// Recursive apparent-size sum, not following symlinks. The fallback when `du`
/// is absent; also the unit-tested path. Best-effort: unreadable entries are
/// skipped rather than aborting the walk.
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn measure_worktree_handles_no_target() {
        let dir = temp_dir("notarget");
        std::fs::write(dir.join("only.txt"), vec![0u8; 100]).unwrap();
        let u = measure_worktree(&dir);
        assert_eq!(u.target_bytes, 0, "no target/ subtree");
        assert!(u.total_bytes >= 100);
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
