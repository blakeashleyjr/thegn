//! Collecting landed worktrees once their grace period is up.
//!
//! The `when` is [`thegn_core::merge_sweep`]; this is the `what`. Under
//! `on_landed = "expire"` a landed branch keeps its worktree — filed into
//! `merged_folder` — until `merged_ttl_secs` has passed, at which point the sweep
//! attempts verified, non-forced worktree removal. Automatic cleanup retains
//! branch refs and a queue hold until safe atomic branch deletion is established;
//! explicit queue dismissal remains available.
//!
//! Runs only in the requested repository, off-loop, and on demand via
//! `thegn merge sweep` / the `sweep-merged` action. There is deliberately **no
//! timer**: a worktree that comes due while thegn sits idle is collected at the
//! next natural wake, which is the whole point of an idle loop that never polls.

use std::path::Path;
use thegn_core::config::{Config, OnLanded};
use thegn_core::db::{Db, MergeQueueRow};
use thegn_core::merge_sweep::{self, MergedEntry};
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;

/// Treat database/Git/path diagnostics as text, never terminal instructions.
pub(crate) fn safe_display(value: &str) -> String {
    value
        .chars()
        .take(512)
        .map(|c| {
            if c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                '�'
            } else {
                c
            }
        })
        .collect()
}

/// What one sweep did, for the caller to report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Branches whose worktree was removed.
    pub collected: Vec<String>,
    /// Worktree keys actually deleted by successful cleanup bookkeeping.
    pub cleared_rows: Vec<String>,
    /// Branches left alone because their worktree had become dirty again.
    pub kept_dirty: Vec<String>,
    /// Refused or failed physical cleanup, with an actionable reason.
    pub kept: Vec<(String, String)>,
    /// Enumeration or post-removal bookkeeping failures, never "collected".
    pub bookkeeping_errors: Vec<String>,
}

impl SweepReport {
    pub fn is_empty(&self) -> bool {
        self.collected.is_empty()
            && self.cleared_rows.is_empty()
            && self.kept_dirty.is_empty()
            && self.kept.is_empty()
            && self.bookkeeping_errors.is_empty()
    }
}

/// The landed rows for `repo_root`'s target, projected for the expiry check.
///
/// Only `landed` rows are candidates. Anything still queued, deferred or
/// mid-flight is someone's open work by definition.
fn landed_entries(db: &Db, target: &str) -> anyhow::Result<Vec<MergeQueueRow>> {
    Ok(db
        .list_merge_queue()?
        .into_iter()
        .filter(|r| r.status == "landed" && r.target_branch == target)
        .collect())
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
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            report
                .bookkeeping_errors
                .push(format!("sweep database unavailable: {error}"));
            return report;
        }
    };
    sweep_with_db(cfg, repo_root, force, &db)
}

/// Manual non-expiry row clearing is a short compare-and-delete transaction;
/// retrying or reassigning an item revokes the old UI selection's authority.
pub(crate) fn clear_selected_landed(db: &Db, selected: &[MergeQueueRow]) -> anyhow::Result<usize> {
    db.transaction(|db| {
        let mut current = db.list_merge_queue()?;
        let mut removed = 0;
        for row in selected {
            if row.status == "landed" && current.iter().any(|now| now == row) {
                db.remove_merge_entry(&row.worktree)?;
                current.retain(|now| now.worktree != row.worktree);
                removed += 1;
            }
        }
        Ok(removed)
    })
}

fn sweep_with_db(cfg: &Config, repo_root: &Path, force: bool, db: &Db) -> SweepReport {
    use crate::merge_lifecycle::CleanupOutcome;
    let mq = cfg.repo_merge_queue(repo_root);
    let mut report = SweepReport::default();
    if mq.on_landed != OnLanded::Expire {
        return report;
    }
    let target = crate::integrate::resolve_target(&mq, repo_root);
    let rows = match landed_entries(db, &target) {
        Ok(rows) => rows,
        Err(error) => {
            report
                .bookkeeping_errors
                .push(format!("sweep enumeration unavailable: {error}"));
            return report;
        }
    };
    let identity = match crate::merge_lifecycle::repository_identity(repo_root) {
        Ok(identity) => identity,
        Err(error) => {
            report
                .bookkeeping_errors
                .push(format!("sweep repository identity unavailable: {error}"));
            return report;
        }
    };
    let entries: Vec<_> = rows
        .iter()
        // A global queue may contain the same target name in unrelated repos.
        // Exclude proven foreign Git identities before applying this repo's
        // clock or reporting holds. Unknown/missing paths remain conservative
        // candidates; the final cleanup admission still proves all ownership.
        .filter(|row| {
            crate::merge_lifecycle::repository_identity(Path::new(&row.worktree))
                .map_or(true, |candidate| candidate == identity)
        })
        .filter(|row| {
            if merge_sweep::CleanupHold::from_detail(row.error_detail.as_deref()).is_some() {
                if force { report.kept.push((row.branch.clone(), "worktree previously collected; branch and queue evidence retained for explicit cleanup (THE-596)".into())); }
                false
            } else { true }
        })
        .map(|r| MergedEntry {
            worktree: r.worktree.clone(),
            branch: r.branch.clone(),
            landed_at: r.updated_at,
        })
        .collect();
    let now = util::now();
    let due: Vec<&MergedEntry> = if force {
        entries.iter().collect()
    } else {
        merge_sweep::due(&entries, now, mq.merged_ttl_secs)
    };
    for entry in due {
        let row = rows
            .iter()
            .find(|r| r.worktree == entry.worktree)
            .expect("projected row");
        if !row.location.is_empty() && row.location != "local" {
            report.kept.push((
                entry.branch.clone(),
                "remote/unknown worktree location".into(),
            ));
            continue;
        }
        let Some(landed) = row.result_oid.as_deref() else {
            report.kept.push((
                entry.branch.clone(),
                "landed commit identity is missing".into(),
            ));
            continue;
        };
        match crate::merge_lifecycle::remove_landed_with_config(
            cfg,
            db,
            repo_root,
            &entry.worktree,
            &entry.branch,
            &target,
            Some(landed),
            row,
            /* delete_branch */ true,
        ) {
            CleanupOutcome::Removed {
                bookkeeping_errors,
                queue_removed,
                ..
            } => {
                report.collected.push(entry.branch.clone());
                if queue_removed {
                    report.cleared_rows.push(entry.worktree.clone());
                }
                report.bookkeeping_errors.extend(bookkeeping_errors);
            }
            CleanupOutcome::KeptDirty => report.kept_dirty.push(entry.branch.clone()),
            CleanupOutcome::Refused { reason } => report.kept.push((
                entry.branch.clone(),
                format!("{}: {reason}", entry.worktree),
            )),
        }
    }
    report
}

/// Fire-and-forget sweep on a blocking thread — the startup and post-fold entry
/// point. Never on the event loop: it resolves the repo root, stats worktrees
/// and shells out to git. `dir` may be any path inside the repo (the launch dir
/// at startup, an arbitrary worktree path after a fold); a non-repo path is a
/// no-op.
pub fn spawn(cfg: Config, dir: std::path::PathBuf) {
    tokio::task::spawn_blocking(move || {
        // Resolve the sweep root off-thread too (THE-78): `toplevel` shells out
        // to git, so it must not run on the pre-first-frame launch path.
        let Some(repo_root) = thegn_core::repo::toplevel(&dir) else {
            return;
        };
        let report = sweep(&cfg, &repo_root, false);
        if !report.collected.is_empty() {
            thegn_core::msg::info(&format!(
                "merge queue: swept {} merged worktree(s): {}",
                report.collected.len(),
                report
                    .collected
                    .iter()
                    .map(|b| safe_display(b))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for b in &report.kept_dirty {
            thegn_core::msg::warn(&format!(
                "merge queue: {} is past its merged grace period but has uncommitted, untracked or ignored work — kept",
                safe_display(b)
            ));
        }
        for (branch, reason) in &report.kept {
            thegn_core::msg::warn(&format!(
                "merge cleanup: kept {} — {}",
                safe_display(branch),
                safe_display(reason)
            ));
        }
        for error in &report.bookkeeping_errors {
            thegn_core::msg::warn(&safe_display(error));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::store::WorkspaceStore;

    fn local_config() -> Config {
        let mut cfg = Config::default();
        cfg.sandbox.enabled = false;
        cfg
    }

    #[expect(clippy::disallowed_methods)]
    fn fixture(
        parent: &Path,
        name: &str,
        db: &Db,
        isolation: &crate::merge_lifecycle::TestIsolation,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = parent.join(name);
        let wt = parent.join(format!("{name}-feature"));
        std::fs::create_dir(&root).unwrap();
        let git = |args: &[&str]| {
            let output = isolation
                .git(&root)
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.name", "private fixture"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(root.join("tracked"), "keep\n").unwrap();
        git(&["add", "tracked"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            wt.to_str().unwrap(),
            "main",
        ]);
        db.put_worktree(
            "feature",
            root.to_str().unwrap(),
            wt.to_str().unwrap(),
            "feature",
            None,
            None,
        )
        .unwrap();
        db.enqueue_merge(wt.to_str().unwrap(), "feature", "main")
            .unwrap();
        let oid = git(&["rev-parse", "HEAD"]);
        db.update_merge_status(wt.to_str().unwrap(), "landed", Some(&oid), None, None)
            .unwrap();
        (root, wt)
    }

    #[test]
    fn shared_database_collects_only_verified_repository_and_reports_actual_removal() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "one", &db, &isolation);
        let (foreign_root, foreign) = fixture(&parent, "two", &db, &isolation);
        let oid = db
            .list_merge_queue()
            .unwrap()
            .into_iter()
            .find(|row| row.worktree == wt.to_str().unwrap())
            .unwrap()
            .result_oid
            .unwrap();
        db.enqueue_merge(root.to_str().unwrap(), "forged-main-row", "main")
            .unwrap();
        db.update_merge_status(root.to_str().unwrap(), "landed", Some(&oid), None, None)
            .unwrap();
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert_eq!(report.collected, ["feature"]);
        assert_eq!(
            report.kept.len(),
            1,
            "foreign is outside the sweep; forged main is refused"
        );
        assert_eq!(
            report.bookkeeping_errors.len(),
            1,
            "branch cleanup explicitly held"
        );
        assert!(report.cleared_rows.is_empty());
        assert!(!wt.exists());
        assert!(
            root.join("tracked").exists()
                && foreign_root.join("tracked").exists()
                && foreign.join("tracked").exists()
        );
        let rows = db.list_merge_queue().unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().any(|r| Path::new(&r.worktree) == foreign));
        assert!(rows.iter().any(|r| Path::new(&r.worktree) == root));
    }

    #[test]
    fn dirty_conflicted_claim_and_wrong_target_rows_keep_retry_metadata() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "dirty", &db, &isolation);
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        std::fs::write(wt.join("untracked"), "private user content").unwrap();
        let before = db.list_merge_queue().unwrap();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty());
        assert_eq!(report.kept_dirty, ["feature"]);
        assert_eq!(db.list_merge_queue().unwrap(), before);
        let claim = crate::worktree_lifecycle::try_scoped_destroy_path(&wt).unwrap();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty());
        assert_eq!(report.kept.len(), 1);
        assert_eq!(db.list_merge_queue().unwrap(), before);
        drop(claim);
        cfg.merge_queue.target_branch = "other".into();
        assert!(sweep_with_db(&cfg, &root, true, &db).is_empty());
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert_eq!(
            std::fs::read_to_string(wt.join("untracked")).unwrap(),
            "private user content"
        );
    }

    #[test]
    #[expect(clippy::disallowed_methods)]
    fn held_cleanup_survives_refresh_without_repeating_missing_worktree_errors_or_touching_refs() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "held", &db, &isolation);
        let selected = db.list_merge_queue().unwrap().remove(0);
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let first = sweep_with_db(&cfg, &root, true, &db);
        assert_eq!(first.collected, ["feature"]);
        assert!(first.cleared_rows.is_empty());
        assert!(!wt.exists());
        let mut held = selected;
        held.error_detail = Some(merge_sweep::CleanupHold::BranchRetained.marker().into());
        assert_eq!(db.list_merge_queue().unwrap(), [held.clone()]);
        // These are the real generic reconciliation/cache-pruning calls.
        db.del_worktree(wt.to_str().unwrap()).unwrap();
        db.del_worktree(wt.to_str().unwrap()).unwrap();
        assert_eq!(db.list_merge_queue().unwrap(), [held]);
        for args in [
            vec!["branch", "victim"],
            vec!["symbolic-ref", "refs/heads/feature", "refs/heads/victim"],
            vec!["symbolic-ref", "refs/heads/main", "refs/heads/victim"],
        ] {
            let out = isolation.git(&root).args(args).output().unwrap();
            assert!(out.status.success());
        }
        let refs = || {
            let out = isolation
                .git(&root)
                .args([
                    "for-each-ref",
                    "--format=%(refname) %(symref) %(objectname)",
                ])
                .output()
                .unwrap();
            assert!(out.status.success());
            out.stdout
        };
        let before = refs();
        assert!(sweep_with_db(&cfg, &root, false, &db).is_empty());
        let forced = sweep_with_db(&cfg, &root, true, &db);
        assert!(forced.collected.is_empty() && forced.cleared_rows.is_empty());
        assert_eq!(forced.kept.len(), 1);
        assert!(forced.kept[0].1.contains("previously collected"));
        assert_eq!(
            refs(),
            before,
            "source, victim and symbolic target all survive"
        );
        db.retry_merge_entry(wt.to_str().unwrap()).unwrap();
        let retried = db.list_merge_queue().unwrap().remove(0);
        assert_eq!(retried.status, "queued");
        assert!(retried.error_detail.is_none());
    }

    #[test]
    fn foreign_cached_repository_is_refused_before_teardown() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db = Db::open_memory().unwrap();
        let (root, wt) = fixture(&parent, "one", &db, &isolation);
        let (foreign, _) = fixture(&parent, "two", &db, &isolation);
        db.put_worktree(
            "feature",
            foreign.to_str().unwrap(),
            wt.to_str().unwrap(),
            "feature",
            None,
            None,
        )
        .unwrap();
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        let before = db.list_merge_queue().unwrap();
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty());
        assert!(
            report
                .kept
                .iter()
                .any(|(_, why)| why.contains("cached repository"))
        );
        assert_eq!(db.list_merge_queue().unwrap(), before);
        assert!(wt.join("tracked").exists());
    }

    #[test]
    #[expect(clippy::disallowed_methods)]
    fn malformed_remote_cache_refuses_before_hooks_and_preserves_worktree_refs_queue() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db_path = parent.join("private.db");
        let db = Db::open_at(&db_path).unwrap();
        let (root, wt) = fixture(&parent, "malformed", &db, &isolation);
        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "UPDATE worktrees SET location='remote-placement', position='malformed' WHERE worktree=?1",
                [wt.to_str().unwrap()],
            )
            .unwrap();
        assert!(
            db.worktrees().unwrap().is_empty(),
            "exercise hidden legacy row"
        );
        assert!(db.worktree_record(wt.to_str().unwrap()).is_err());
        let refs = || {
            let output = isolation
                .git(&root)
                .args([
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before_refs = refs();
        let before_queue = db.list_merge_queue().unwrap();
        let before_contents = std::fs::read(wt.join("tracked")).unwrap();
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        cfg.hooks.pre_destroy = vec![thegn_core::hooks::HookEntry::Command(
            "printf ran > cleanup-hook".into(),
        )];
        let report = sweep_with_db(&cfg, &root, true, &db);
        assert!(report.collected.is_empty() && report.cleared_rows.is_empty());
        assert_eq!(report.kept.len(), 1);
        assert!(
            report.kept[0]
                .1
                .contains("worktree cache identity unavailable")
        );
        assert_eq!(refs(), before_refs);
        assert_eq!(db.list_merge_queue().unwrap(), before_queue);
        assert_eq!(std::fs::read(wt.join("tracked")).unwrap(), before_contents);
        assert!(!wt.join("cleanup-hook").exists());
        assert!(!root.join("cleanup-hook").exists());
    }

    #[test]
    #[expect(clippy::disallowed_methods)]
    fn unknown_resource_metadata_refuses_before_hooks_and_preserves_worktree_refs_queue() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        for kind in [
            "tenancy-key",
            "tenancy-association",
            "dispatch",
            "pending-dispatch",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().canonicalize().unwrap();
            let db_path = parent.join("private.db");
            let db = Db::open_at(&db_path).unwrap();
            let (root, wt) = fixture(&parent, kind, &db, &isolation);
            let fixture_conn = rusqlite::Connection::open(&db_path).unwrap();
            let path = wt.to_str().unwrap();
            match kind {
                "tenancy-key" | "tenancy-association" => {
                    let (sandbox, association) = if kind == "tenancy-key" {
                        (path, "")
                    } else {
                        ("private-reservation", path)
                    };
                    fixture_conn.execute(
                        "INSERT INTO host_tenancy(sandbox,host_id,worktree,cpu_floor_milli,mem_floor_mb,reserved_at) VALUES(?1,'invalid-host-id',?2,0,0,0)",
                        [sandbox, association],
                    ).unwrap();
                }
                _ => {
                    let (active, pending, status) = if kind == "dispatch" {
                        (path, "", "unknown-future-state")
                    } else {
                        ("private-other", path, "done")
                    };
                    fixture_conn.execute(
                        "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status,pending_worktree_path) VALUES('private',?1,'private',0,?2,?3)",
                        [active, status, pending],
                    ).unwrap();
                }
            }
            let refs = || {
                let output = isolation
                    .git(&root)
                    .args([
                        "for-each-ref",
                        "--format=%(refname) %(objectname)",
                        "refs/heads",
                    ])
                    .output()
                    .unwrap();
                assert!(output.status.success());
                output.stdout
            };
            let before_refs = refs();
            let before_queue = db.list_merge_queue().unwrap();
            let mut cfg = local_config();
            cfg.merge_queue.on_landed = OnLanded::Expire;
            cfg.merge_queue.target_branch = "main".into();
            cfg.hooks.pre_destroy = vec![thegn_core::hooks::HookEntry::Command(
                "printf ran > cleanup-hook".into(),
            )];
            let report = sweep_with_db(&cfg, &root, true, &db);
            assert!(
                report.collected.is_empty() && report.cleared_rows.is_empty(),
                "{kind}"
            );
            assert_eq!(report.kept.len(), 1, "{kind}");
            assert!(
                report.kept[0]
                    .1
                    .contains("runtime/session/dispatch ownership"),
                "{kind}: {report:?}"
            );
            assert_eq!(refs(), before_refs, "{kind}");
            assert_eq!(db.list_merge_queue().unwrap(), before_queue, "{kind}");
            assert_eq!(
                std::fs::read(wt.join("tracked")).unwrap(),
                b"keep\n",
                "{kind}"
            );
            assert!(!wt.join("cleanup-hook").exists(), "{kind}");
            assert!(!root.join("cleanup-hook").exists(), "{kind}");
        }
    }

    #[test]
    fn post_admission_unknown_dispatch_stops_removal_after_the_pre_destroy_hook() {
        assert_post_admission_mutation_stops_removal(LateCleanupMutation::UnknownDispatch);
    }

    #[test]
    fn post_admission_malformed_registry_stops_removal_after_the_pre_destroy_hook() {
        assert_post_admission_mutation_stops_removal(LateCleanupMutation::MalformedRegistry);
    }

    #[derive(Clone, Copy)]
    enum LateCleanupMutation {
        UnknownDispatch,
        MalformedRegistry,
    }

    #[expect(clippy::disallowed_methods)]
    fn assert_post_admission_mutation_stops_removal(mutation: LateCleanupMutation) {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().canonicalize().unwrap();
        let db_path = parent.join("private.db");
        let db = Db::open_at(&db_path).unwrap();
        let (root, wt) = fixture(&parent, "late-resource", &db, &isolation);
        assert!(db.worktree_record(wt.to_str().unwrap()).unwrap().is_some());
        let before_queue = db.list_merge_queue().unwrap();
        let refs = || {
            let output = isolation
                .git(&root)
                .args([
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/heads",
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let before_refs = refs();
        let started = parent.join("hook-started");
        let release = parent.join("hook-release");
        let mut cfg = local_config();
        cfg.merge_queue.on_landed = OnLanded::Expire;
        cfg.merge_queue.target_branch = "main".into();
        cfg.hooks.pre_destroy = vec![thegn_core::hooks::HookEntry::Spec(
            thegn_core::hooks::HookEntrySpec {
                command: format!(
                    "printf started > {}; while [ ! -e {} ]; do sleep 0.01; done",
                    util::sh_quote(&started.to_string_lossy()),
                    util::sh_quote(&release.to_string_lossy()),
                ),
                wait: Some(true),
                timeout_secs: Some(10),
                on_failure: Some(thegn_core::hooks::HookFailure::Warn),
            },
        )];
        let writer_started = started.clone();
        let writer_release = release.clone();
        let writer_path = wt.to_str().unwrap().to_owned();
        // Scoped ownership joins the writer even if the sweep/assertion panics.
        let report = std::thread::scope(|scope| {
            let writer = std::thread::Builder::new().name("private-late-resource".into())
                .spawn_scoped(scope, move || -> anyhow::Result<()> {
                    struct Release(std::path::PathBuf);
                    impl Drop for Release {
                        fn drop(&mut self) {
                            if let Err(error) = std::fs::write(&self.0, "release") {
                                eprintln!("private hook release failed: {error}");
                            }
                        }
                    }
                    let _release = Release(writer_release);
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !writer_started.exists() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    anyhow::ensure!(writer_started.exists(), "pre-destroy rendezvous did not start");
                    let writer_conn = rusqlite::Connection::open(&db_path)?;
                    let changed = match mutation {
                        LateCleanupMutation::UnknownDispatch => writer_conn.execute(
                            "INSERT INTO agent_dispatches(issue_id,worktree_path,agent_name,dispatched_at_ms,status) VALUES('private',?1,'private',0,'unknown-after-admission')",
                            [writer_path],
                        )?,
                        // Change only an unrelated decoder field AFTER the real
                        // hook starts. Initial registry admission was valid.
                        LateCleanupMutation::MalformedRegistry => writer_conn.execute(
                            "UPDATE worktrees SET position='malformed-after-admission' WHERE worktree=?1",
                            [writer_path],
                        )?,
                    };
                    anyhow::ensure!(changed == 1, "private mutation did not reach its row");
                    Ok(())
                }).unwrap();
            let report = sweep_with_db(&cfg, &root, true, &db);
            writer.join().unwrap().unwrap();
            report
        });
        assert!(
            started.exists() && release.exists(),
            "the earlier hook really ran"
        );
        assert!(report.collected.is_empty() && report.cleared_rows.is_empty());
        assert_eq!(report.kept.len(), 1);
        match mutation {
            LateCleanupMutation::UnknownDispatch => {
                assert!(
                    report.kept[0]
                        .1
                        .contains("runtime/session/dispatch ownership"),
                    "{report:?}"
                );
                assert!(
                    thegn_core::store::NotificationStore::has_cleanup_dispatch(
                        &db,
                        wt.to_str().unwrap()
                    )
                    .unwrap()
                );
            }
            LateCleanupMutation::MalformedRegistry => {
                let decode_error = db.worktree_record(wt.to_str().unwrap()).unwrap_err();
                assert!(
                    report.kept[0].1.contains(&decode_error.to_string()),
                    "{report:?}"
                );
                assert!(
                    !thegn_core::store::NotificationStore::has_cleanup_dispatch(
                        &db,
                        wt.to_str().unwrap()
                    )
                    .unwrap()
                );
            }
        }
        assert_eq!(refs(), before_refs);
        assert_eq!(db.list_merge_queue().unwrap(), before_queue);
        assert_eq!(std::fs::read(wt.join("tracked")).unwrap(), b"keep\n");
    }

    #[test]
    fn diagnostics_never_emit_terminal_controls_and_are_bounded() {
        assert_eq!(safe_display("α\u{061c}\u{200e}\u{200f}β"), "α���β");
        assert_eq!(safe_display("a\x1b[31m\n\u{202e}b"), "a�[31m��b");
        assert_eq!(safe_display(&"x".repeat(1000)).len(), 512);
    }

    #[test]
    fn selected_landed_row_cannot_authorize_requeued_or_revoked_cleanup() {
        let isolation = crate::merge_lifecycle::TestIsolation::new();
        for revoke in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().canonicalize().unwrap();
            let db = Db::open_memory().unwrap();
            let (root, wt) = fixture(&parent, "selected", &db, &isolation);
            let selected = db.list_merge_queue().unwrap().remove(0);
            if revoke {
                db.remove_merge_entry(wt.to_str().unwrap()).unwrap();
            } else {
                db.enqueue_merge(wt.to_str().unwrap(), "feature", "main")
                    .unwrap();
            }
            let before = db.list_merge_queue().unwrap();
            let outcome = crate::merge_lifecycle::remove_landed_with_config(
                &Config::default(),
                &db,
                &root,
                wt.to_str().unwrap(),
                "feature",
                "main",
                selected.result_oid.as_deref(),
                &selected,
                true,
            );
            assert!(
                matches!(outcome, crate::merge_lifecycle::CleanupOutcome::Refused { reason } if reason.contains("selected landed"))
            );
            assert_eq!(db.list_merge_queue().unwrap(), before);
            assert!(wt.join("tracked").exists());
        }
    }

    #[test]
    fn manual_clear_only_deletes_exact_selected_landed_rows() {
        let db = Db::open_memory().unwrap();
        for wt in ["unchanged", "retried", "revoked"] {
            db.enqueue_merge(wt, "feature", "main").unwrap();
            db.update_merge_status(wt, "landed", Some("fixture-oid"), None, None)
                .unwrap();
        }
        let mut selected = db.list_merge_queue().unwrap();
        selected.extend(selected.clone()); // duplicate UI selection must not inflate success
        db.enqueue_merge("retried", "feature", "main").unwrap();
        db.remove_merge_entry("revoked").unwrap();
        assert_eq!(clear_selected_landed(&db, &selected).unwrap(), 1);
        let remaining = db.list_merge_queue().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].worktree, "retried");
        assert_eq!(remaining[0].status, "queued");
    }

    fn report(collected: &[&str], kept: &[&str]) -> SweepReport {
        SweepReport {
            collected: collected.iter().map(|s| (*s).to_string()).collect(),
            kept_dirty: kept.iter().map(|s| (*s).to_string()).collect(),
            ..SweepReport::default()
        }
    }

    #[test]
    fn an_empty_report_is_empty_only_with_neither_list() {
        assert!(SweepReport::default().is_empty());
        assert!(!report(&["a"], &[]).is_empty());
        assert!(!report(&[], &["a"]).is_empty());
        assert!(
            !SweepReport {
                kept: vec![("a".into(), "refused".into())],
                ..Default::default()
            }
            .is_empty()
        );
        assert!(
            !SweepReport {
                bookkeeping_errors: vec!["unavailable".into()],
                ..Default::default()
            }
            .is_empty()
        );
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
