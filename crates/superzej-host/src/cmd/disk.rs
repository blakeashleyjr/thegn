//! `superzej disk` / `superzej clean` — per-worktree disk-usage reporting and
//! reclaiming. `disk` scans and prints sizes (and refreshes the cache the live
//! UI reads); `clean` removes a worktree's regenerable `target/` while keeping
//! the checkout.

use anyhow::Result;
use superzej_core::config::Config;
use superzej_core::db::Db;
use superzej_core::store::{WorkspaceStore, WorktreeAuxStore};
use superzej_core::{disk, outln, worktree};

use super::{confirm, resolve_worktree};

/// `superzej disk [--worktree P] [--all]` — measure and print worktree sizes,
/// largest first, with a grand total and a threshold note. Also refreshes the
/// `worktree_disk` cache so a running host paints fresh sizes immediately.
pub fn disk(cfg: &Config, worktree_arg: Option<String>, all: bool) -> Result<()> {
    let db = Db::open()?;

    // Target set: every known worktree (default / --all), or just the resolved one.
    let targets: Vec<(String, String)> = if all || worktree_arg.is_none() {
        db.worktrees()?
            .into_iter()
            .map(|w| (w.worktree, w.branch))
            .collect()
    } else {
        let p = resolve_worktree(worktree_arg)
            .to_string_lossy()
            .into_owned();
        let branch = db
            .worktrees()
            .ok()
            .and_then(|ws| ws.into_iter().find(|w| w.worktree == p).map(|w| w.branch))
            .unwrap_or_default();
        vec![(p, branch)]
    };

    let mut rows: Vec<(String, String, disk::DiskUsage)> = targets
        .into_iter()
        .filter_map(|(path, branch)| {
            let p = std::path::Path::new(&path);
            if !p.is_dir() {
                return None;
            }
            let usage = disk::measure_worktree(p);
            // Refresh the cache the UI reads.
            let _ =
                db.put_worktree_disk(&path, usage.total_bytes as i64, usage.target_bytes as i64);
            Some((path, branch, usage))
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.2.total_bytes));

    if rows.is_empty() {
        outln!("No worktrees found.");
        return Ok(());
    }

    outln!("{:>9}  {:>9}  {}", "SIZE", "TARGET", "WORKTREE");
    let mut grand = 0u64;
    let mut grand_target = 0u64;
    for (path, branch, u) in &rows {
        grand += u.total_bytes;
        grand_target += u.target_bytes;
        let label = if branch.is_empty() {
            path.clone()
        } else {
            format!("{branch}  {path}")
        };
        outln!(
            "{:>9}  {:>9}  {}",
            disk::human(u.total_bytes),
            disk::human(u.target_bytes),
            label
        );
    }
    outln!(
        "{:>9}  {:>9}  {} worktree(s)",
        disk::human(grand),
        disk::human(grand_target),
        rows.len()
    );
    outln!(
        "Reclaimable (target/): {} — `superzej clean --all` to recover.",
        disk::human(grand_target)
    );

    let threshold = cfg.disk.warn_threshold_gb;
    if threshold > 0 && grand > threshold * 1024 * 1024 * 1024 {
        outln!(
            "⚠ Total {} exceeds the {}G warning threshold ([disk].warn_threshold_gb).",
            disk::human(grand),
            threshold
        );
    }
    Ok(())
}

/// `superzej clean [--worktree P] [--all] [--force]` — reclaim `target/` for the
/// resolved worktree (default), or every worktree (`--all`). Refuses the active
/// worktree (`$SUPERZEJ_WORKTREE`); `cargo clean` takes the build lock so a
/// concurrent build serializes rather than corrupts. Prompts unless `--force`.
pub fn clean(cfg: &Config, worktree_arg: Option<String>, all: bool, force: bool) -> Result<()> {
    let db = Db::open()?;
    let active = std::env::var("SUPERZEJ_WORKTREE").unwrap_or_default();

    let targets: Vec<String> = if all {
        db.worktrees()?.into_iter().map(|w| w.worktree).collect()
    } else {
        vec![
            resolve_worktree(worktree_arg)
                .to_string_lossy()
                .into_owned(),
        ]
    };

    let mut total_reclaimed = 0u64;
    let mut cleaned = 0u32;
    for path in targets {
        let p = std::path::Path::new(&path);
        if !p.is_dir() {
            continue;
        }
        if !active.is_empty() && path == active {
            outln!("skip (active worktree): {path}");
            continue;
        }
        let target = p.join("target");
        if !target.is_dir() {
            continue;
        }
        let size = disk::measure_worktree(p).target_bytes;
        if !force
            && !confirm(&format!(
                "Remove {} of build artifacts in {path}?",
                disk::human(size)
            ))
        {
            outln!("skip: {path}");
            continue;
        }
        match worktree::clean_target(p) {
            Ok(reclaimed) => {
                let _ = db.delete_worktree_disk(&path);
                total_reclaimed += reclaimed;
                cleaned += 1;
                outln!("cleaned {} from {path}", disk::human(reclaimed));
            }
            Err(e) => outln!("failed to clean {path}: {e}"),
        }
    }
    // `cfg` is read for symmetry with `disk` (future per-repo policy); silence
    // the unused warning without changing the signature.
    let _ = cfg;
    outln!(
        "Reclaimed {} across {cleaned} worktree(s).",
        disk::human(total_reclaimed)
    );
    Ok(())
}
