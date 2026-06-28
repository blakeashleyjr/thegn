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
        let dir = std::env::temp_dir().join(format!("sz-disk-{tag}-{}", std::process::id()));
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
}
