//! Collecting landed worktrees once their grace period is up.
//!
//! The `when` is [`thegn_core::merge_sweep`]; this is the `what`. Under
//! `on_landed = "expire"` a landed branch keeps its worktree — filed into
//! `merged_folder` — until `merged_ttl_secs` has passed, at which point the sweep
//! removes the worktree and deletes the branch, exactly as `"remove"` would have
//! done at landing time.
//!
//! Runs at startup and after a fold (both off-loop), and on demand via
//! `thegn merge sweep` / the `sweep-merged` action. There is deliberately **no
//! timer**: a worktree that comes due while thegn sits idle is collected at the
//! next natural wake, which is the whole point of an idle loop that never polls.

use std::path::Path;
use thegn_core::config::{Config, OnLanded};
use thegn_core::db::Db;
use thegn_core::merge_sweep::{self, MergedEntry};
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;

/// What one sweep did, for the caller to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Branches whose worktree was removed.
    pub collected: Vec<String>,
    /// Branches left alone because their worktree had become dirty again.
    pub kept_dirty: Vec<String>,
}

impl SweepReport {
    pub fn is_empty(&self) -> bool {
        self.collected.is_empty() && self.kept_dirty.is_empty()
    }
}

/// The landed rows for `repo_root`'s target, projected for the expiry check.
///
/// Only `landed` rows are candidates. Anything still queued, deferred or
/// mid-flight is someone's open work by definition.
fn landed_entries(db: &Db) -> Vec<MergedEntry> {
    db.list_merge_queue()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.status == "landed")
        .map(|r| MergedEntry {
            worktree: r.worktree,
            branch: r.branch,
            landed_at: r.updated_at,
        })
        .collect()
}

/// Collect every landed worktree whose grace period has elapsed.
///
/// `force` ignores the clock and collects all of them — the manual "clear
/// merged now" gesture. It does NOT ignore the dirty guard: a worktree you have
/// gone back to and edited is never removed by a sweep, deliberate or not, since
/// the gesture means "tidy what I'm done with", never "discard my edits".
pub fn sweep(cfg: &Config, repo_root: &Path, force: bool) -> SweepReport {
    let mq = cfg.repo_merge_queue(repo_root);
    let mut report = SweepReport::default();
    // `expire` is the only mode with a grace period to end. Under `move` the
    // worktree is meant to stay; under `remove`/`detach` it is already gone.
    // A forced sweep still honors this — "clear merged" under `move` would
    // delete worktrees the config says to keep.
    if mq.on_landed != OnLanded::Expire {
        return report;
    }
    let Ok(db) = Db::open() else {
        return report;
    };
    let entries = landed_entries(&db);
    let now = util::now();
    let due: Vec<&MergedEntry> = if force {
        entries.iter().collect()
    } else {
        merge_sweep::due(&entries, now, mq.merged_ttl_secs)
    };
    for entry in due {
        // Re-read dirtiness at collection time, not at landing: the grace period
        // exists precisely so someone can go back into a merged worktree, and
        // doing so must cancel the collection.
        if crate::merge_lifecycle::worktree_is_dirty(&entry.worktree) {
            report.kept_dirty.push(entry.branch.clone());
            continue;
        }
        crate::merge_lifecycle::remove_landed(
            &db,
            repo_root,
            &entry.worktree,
            &entry.branch,
            /* delete_branch */ true,
        );
        report.collected.push(entry.branch.clone());
    }
    report
}

/// Fire-and-forget sweep on a blocking thread — the startup and post-fold entry
/// point. Never on the event loop: it stats worktrees and shells out to git.
pub fn spawn(cfg: Config, repo_root: std::path::PathBuf) {
    tokio::task::spawn_blocking(move || {
        let report = sweep(&cfg, &repo_root, false);
        if !report.collected.is_empty() {
            thegn_core::msg::info(&format!(
                "merge queue: swept {} merged worktree(s): {}",
                report.collected.len(),
                report.collected.join(", ")
            ));
        }
        for b in &report.kept_dirty {
            thegn_core::msg::warn(&format!(
                "merge queue: {b} is past its merged grace period but has uncommitted changes — kept"
            ));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(collected: &[&str], kept: &[&str]) -> SweepReport {
        SweepReport {
            collected: collected.iter().map(|s| (*s).to_string()).collect(),
            kept_dirty: kept.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn an_empty_report_is_empty_only_with_neither_list() {
        assert!(SweepReport::default().is_empty());
        assert!(!report(&["a"], &[]).is_empty());
        assert!(!report(&[], &["a"]).is_empty());
    }

    /// The sweep is inert in every mode but `expire`, forced or not — under
    /// `move` the worktrees are meant to persist, and under `remove`/`detach`
    /// they were already collected at landing.
    #[test]
    fn only_expire_mode_sweeps() {
        let root = std::env::temp_dir();
        for mode in [
            OnLanded::Off,
            OnLanded::Move,
            OnLanded::Detach,
            OnLanded::Remove,
        ] {
            let mut cfg = Config::default();
            cfg.merge_queue.on_landed = mode;
            for force in [false, true] {
                assert!(
                    sweep(&cfg, &root, force).is_empty(),
                    "{mode:?} (force={force}) must not sweep"
                );
            }
        }
    }
}
