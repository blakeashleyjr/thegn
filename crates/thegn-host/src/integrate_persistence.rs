//! Pre-fold observations and atomic final queue projection (THE-591).
//!
//! Observations are snapshots, not durable ABA generations. Each queue row is
//! atomic; Git, SQLite and subsequent filesystem lifecycle are not one transaction.

use super::{Candidates, FoldReport, run_fold};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use thegn_core::config::MergeQueueConfig;
use thegn_core::db::Db;
use thegn_core::merge_lifecycle::LifecycleEvent;
use thegn_core::store::{
    MergeFinalOutcome, MergeFinalStatus, MergeOutcomeObservation, MergeOutcomeWrite,
    WorktreeAuxStore,
};

pub(super) struct OutcomeObservations(HashMap<String, MergeOutcomeObservation>);

impl OutcomeObservations {
    pub(super) fn registry_identity(
        &self,
        worktree: &str,
    ) -> Result<Option<&thegn_core::store::MergeRegistryIdentity>> {
        Ok(self
            .0
            .get(worktree)
            .context("missing pre-fold snapshot observation")?
            .registry_identity())
    }
}

pub(super) fn observe_outcomes(db: &Db, candidates: &Candidates) -> Result<OutcomeObservations> {
    let mut observed = HashMap::new();
    for candidate in &candidates.branches {
        let worktree = candidates
            .worktrees
            .get(&candidate.name)
            .context("candidate has no worktree for outcome observation")?;
        anyhow::ensure!(
            !observed.contains_key(worktree),
            "duplicate candidate worktree"
        );
        observed.insert(worktree.clone(), db.observe_merge_outcome(worktree)?);
    }
    Ok(OutcomeObservations(observed))
}

/// Both interactive and CLI entrypoints use this ordering. No observation is
/// acquired after Git/gates run, including when opening the pre-fold DB failed.
pub(crate) fn run_selected_fold(
    config: &MergeQueueConfig,
    repo_root: &Path,
    candidates: &Candidates,
    override_gpg: bool,
) -> Result<FoldReport> {
    run_observed_fold(Db::open(), config, repo_root, candidates, |observations| {
        let tips = super::candidates::selected_snapshot_tips(
            config,
            repo_root,
            candidates,
            observations,
            override_gpg,
        )?;
        run_fold(config, repo_root, tips)
    })
}

fn run_observed_fold(
    database: Result<Db>,
    config: &MergeQueueConfig,
    repo_root: &Path,
    candidates: &Candidates,
    fold: impl FnOnce(Option<&OutcomeObservations>) -> Result<FoldReport>,
) -> Result<FoldReport> {
    let bookkeeping = database.and_then(|db| {
        let observations = observe_outcomes(&db, candidates)?;
        Ok((db, observations))
    });
    let mut report = fold(
        bookkeeping
            .as_ref()
            .ok()
            .map(|(_, observations)| observations),
    )?;
    let persisted = bookkeeping.and_then(|(db, observations)| {
        persist(config, repo_root, &db, candidates, &report, &observations)
    });
    if let Err(error) = persisted {
        // Terminal-safe and bounded independently of the bounded gate log.
        report.bookkeeping_error = Some(
            error
                .to_string()
                .chars()
                .filter(|c| !c.is_control() && super::diagnostic_char(*c))
                .take(2048)
                .collect(),
        );
    }
    Ok(report)
}

struct FinalRow {
    worktree: String,
    branch: String,
    status: MergeFinalStatus,
    result_oid: Option<String>,
    conflict_paths: Option<String>,
    error_detail: Option<String>,
}

/// Validate the complete projection before the first write. Category order is
/// never permission to overwrite a contradictory or duplicate worktree outcome.
fn project(candidates: &Candidates, report: &FoldReport) -> Result<Vec<FinalRow>> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |branch: &str, status, result_oid, conflict_paths, error_detail| -> Result<()> {
        let worktree = candidates
            .worktrees
            .get(branch)
            .context("reported branch has no candidate worktree")?;
        anyhow::ensure!(
            seen.insert(worktree.clone()),
            "duplicate or contradictory fold outcome for worktree"
        );
        rows.push(FinalRow {
            worktree: worktree.clone(),
            branch: branch.to_string(),
            status,
            result_oid,
            conflict_paths,
            error_detail,
        });
        Ok(())
    };
    let hold = || {
        format!(
            "{}; not landed\n{}",
            report
                .request_result()
                .err()
                .map(|error| error.to_string())
                .unwrap_or_else(|| "target not advanced for this candidate".into()),
            report.diagnostics
        )
    };
    for landed in &report.landed {
        if report.advanced {
            anyhow::ensure!(!landed.commit.is_empty(), "landed outcome has no commit");
            add(
                &landed.branch,
                MergeFinalStatus::Landed,
                Some(landed.commit.clone()),
                None,
                None,
            )?;
        } else {
            add(
                &landed.branch,
                MergeFinalStatus::GateError,
                None,
                None,
                Some(hold()),
            )?;
        }
    }
    for prepared in &report.prepared {
        add(
            &prepared.branch,
            MergeFinalStatus::GateError,
            None,
            None,
            Some(hold()),
        )?;
    }
    for branch in &report.unprepared {
        add(
            branch,
            MergeFinalStatus::GateError,
            None,
            None,
            Some(format!(
                "fold preparation unavailable; not landed\n{}",
                report.diagnostics
            )),
        )?;
    }
    for deferred in &report.deferred {
        let paths = (!deferred.paths.is_empty()).then(|| {
            super::conflict_details(&deferred.paths, &deferred.submodule_conflicts).join("\n")
        });
        add(
            &deferred.branch,
            if deferred.gate_failed {
                MergeFinalStatus::GateFailed
            } else {
                MergeFinalStatus::Deferred
            },
            None,
            paths,
            deferred.gate_failed.then(|| report.diagnostics.clone()),
        )?;
    }
    Ok(rows)
}

pub(super) fn persist(
    config: &MergeQueueConfig,
    repo_root: &Path,
    db: &Db,
    candidates: &Candidates,
    report: &FoldReport,
    observations: &OutcomeObservations,
) -> Result<()> {
    let rows = project(candidates, report)?;
    for row in &rows {
        anyhow::ensure!(
            observations.0.contains_key(&row.worktree),
            "missing pre-fold outcome observation"
        );
    }
    let repo = repo_root.to_str().context("repository path is not UTF-8")?;
    for row in rows {
        let observed = &observations.0[&row.worktree];
        let final_row = MergeFinalOutcome {
            worktree: &row.worktree,
            branch: &row.branch,
            repo_root: repo,
            target_branch: &report.target_branch,
            // A local fold must never manufacture remote placement authority.
            location: "",
            status: row.status,
            result_oid: row.result_oid.as_deref(),
            conflict_paths: row.conflict_paths.as_deref(),
            error_detail: row.error_detail.as_deref(),
        };
        match db.persist_merge_outcome(observed, &final_row)? {
            MergeOutcomeWrite::Written => {}
            MergeOutcomeWrite::RegistryChanged => anyhow::bail!(
                "worktree registry changed while folding; outcome bookkeeping skipped"
            ),
            MergeOutcomeWrite::QueueChanged => {
                anyhow::bail!("queue changed while folding; outcome bookkeeping skipped")
            }
        }
        // Only the committed final state authorizes this lifecycle event. No
        // transient Enqueued event is emitted. A later filesystem/DB reassignment
        // still needs the independent lifecycle ownership checks (THE-588).
        match row.status {
            MergeFinalStatus::Landed => crate::merge_lifecycle::apply_landed(
                config,
                db,
                repo_root,
                &row.worktree,
                &row.branch,
                row.result_oid
                    .as_deref()
                    .context("committed landed outcome has no result commit")?,
            ),
            _ => crate::merge_lifecycle::apply(
                config,
                db,
                repo_root,
                &row.worktree,
                &row.branch,
                LifecycleEvent::Failed,
            ),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrate::{DeferredReport, GateOutcome, LandedReport, build_report, empty_plan};
    use thegn_core::fold::{Branch, ConflictKind};
    use thegn_core::store::WorkspaceStore;

    struct Fixture {
        db: Db,
        candidates: Candidates,
        config: MergeQueueConfig,
        repo: std::path::PathBuf,
        db_path: std::path::PathBuf,
        worktree: String,
        original_folder: i64,
        _env: thegn_core::testenv::EnvGuard,
        _root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let root_s = root.path().to_str().unwrap();
            let env = thegn_core::testenv::EnvGuard::set(&[
                ("XDG_STATE_HOME", root_s),
                ("LOCALAPPDATA", root_s),
                ("XDG_CONFIG_HOME", root_s),
            ]);
            let repo = root.path().join("repo");
            let worktree = root.path().join("candidate");
            std::fs::create_dir(&repo).unwrap();
            std::fs::create_dir(&worktree).unwrap();
            std::fs::write(worktree.join("sentinel"), b"retained").unwrap();
            let worktree = worktree.to_string_lossy().into_owned();
            let db_path = root.path().join("test.db");
            let db = Db::open_at(&db_path).unwrap();
            db.put_workspace(repo.to_str().unwrap(), "fixture", "dir")
                .unwrap();
            let original_folder = db
                .ensure_folder(repo.to_str().unwrap(), "Original")
                .unwrap();
            db.put_worktree(
                "fixture/b1",
                repo.to_str().unwrap(),
                &worktree,
                "b1",
                None,
                Some(original_folder),
            )
            .unwrap();
            db.enqueue_merge(&worktree, "b1", "main").unwrap();
            db.update_merge_status(
                &worktree,
                "needs_human",
                Some("old-result"),
                Some("old-conflict"),
                Some("old-error"),
            )
            .unwrap();
            db.set_merge_agent_attempts(&worktree, 2).unwrap();
            let candidates = Candidates {
                branches: vec![Branch {
                    name: "b1".into(),
                    tip: "candidate-tip".into(),
                }],
                skipped_dirty: Vec::new(),
                identities: HashMap::new(),
                pending_snapshots: HashSet::new(),
                worktrees: HashMap::from([("b1".into(), worktree.clone())]),
            };
            let config = MergeQueueConfig {
                organize_folders: true,
                on_landed: thegn_core::config::OnLanded::Move,
                ..Default::default()
            };
            Self {
                db,
                candidates,
                config,
                repo,
                db_path,
                worktree,
                original_folder,
                _env: env,
                _root: root,
            }
        }

        fn report(&self, status: MergeFinalStatus) -> FoldReport {
            let mut report = build_report(
                "main",
                "original",
                &empty_plan("original"),
                &[],
                GateOutcome::Skipped,
                0,
                "private gate evidence",
            );
            match status {
                MergeFinalStatus::Landed => {
                    report.advanced = true;
                    report.landed.push(LandedReport {
                        branch: "b1".into(),
                        commit: "actual-cas-commit".into(),
                    });
                }
                MergeFinalStatus::GateError => report.prepared.push(LandedReport {
                    branch: "b1".into(),
                    commit: "prepared-only".into(),
                }),
                _ => report.deferred.push(DeferredReport {
                    branch: "b1".into(),
                    paths: vec!["conflicted-file".into()],
                    kind: ConflictKind::Textual,
                    submodule_conflicts: Vec::new(),
                    gate_failed: status == MergeFinalStatus::GateFailed,
                }),
            }
            report
        }

        fn assert_no_lifecycle(&self) {
            let rows = self.db.worktrees().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].folder_id, Some(self.original_folder));
            assert_eq!(
                std::fs::read(Path::new(&self.worktree).join("sentinel")).unwrap(),
                b"retained"
            );
        }
    }

    #[test]
    fn final_projection_never_enqueues_and_lifecycle_observes_committed_status() {
        for status in [
            MergeFinalStatus::Landed,
            MergeFinalStatus::GateError,
            MergeFinalStatus::Deferred,
            MergeFinalStatus::GateFailed,
        ] {
            let fixture = Fixture::new();
            let old = fixture.db.list_merge_queue().unwrap().remove(0);
            let observed = observe_outcomes(&fixture.db, &fixture.candidates).unwrap();
            let sql = rusqlite::Connection::open(&fixture.db_path).unwrap();
            sql.execute_batch(&format!(
                "CREATE TRIGGER no_queued_insert BEFORE INSERT ON merge_queue WHEN NEW.status='queued' BEGIN SELECT RAISE(ABORT,'transient queued'); END;
                 CREATE TRIGGER no_queued_update BEFORE UPDATE ON merge_queue WHEN NEW.status='queued' BEGIN SELECT RAISE(ABORT,'transient queued'); END;
                 CREATE TRIGGER final_before_folder BEFORE UPDATE OF folder_id ON worktrees WHEN (SELECT status FROM merge_queue WHERE worktree=NEW.worktree) IS NOT '{}' BEGIN SELECT RAISE(ABORT,'lifecycle before final'); END;",
                status.as_str()
            )).unwrap();
            let report = fixture.report(status);
            persist(
                &fixture.config,
                &fixture.repo,
                &fixture.db,
                &fixture.candidates,
                &report,
                &observed,
            )
            .unwrap();
            let row = fixture.db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.status, status.as_str());
            assert_eq!(row.queued_at, old.queued_at);
            assert_eq!(row.agent_attempts, old.agent_attempts);
            assert_eq!(
                row.result_oid.as_deref(),
                (status == MergeFinalStatus::Landed).then_some("actual-cas-commit")
            );
            assert_eq!(
                row.conflict_paths.as_deref(),
                matches!(
                    status,
                    MergeFinalStatus::Deferred | MergeFinalStatus::GateFailed
                )
                .then_some("conflicted-file")
            );
            assert_eq!(
                row.error_detail.is_some(),
                matches!(
                    status,
                    MergeFinalStatus::GateError | MergeFinalStatus::GateFailed
                )
            );
            let folder = fixture.db.worktrees().unwrap()[0].folder_id;
            assert_ne!(folder, Some(fixture.original_folder));
            let expected = if status == MergeFinalStatus::Landed {
                &fixture.config.merged_folder
            } else {
                &fixture.config.failed_folder
            };
            assert!(
                fixture
                    .db
                    .folders_for_workspace(fixture.repo.to_str().unwrap())
                    .unwrap()
                    .iter()
                    .any(|row| Some(row.folder_id) == folder && &row.name == expected)
            );
        }
    }

    #[test]
    fn failed_final_write_never_runs_lifecycle_and_retains_existing_row() {
        let fixture = Fixture::new();
        let old = fixture.db.list_merge_queue().unwrap();
        let observed = observe_outcomes(&fixture.db, &fixture.candidates).unwrap();
        let sql = rusqlite::Connection::open(&fixture.db_path).unwrap();
        sql.execute_batch("CREATE TRIGGER fail_final BEFORE UPDATE ON merge_queue BEGIN SELECT RAISE(ABORT,'private write failure'); END;").unwrap();
        assert!(
            persist(
                &fixture.config,
                &fixture.repo,
                &fixture.db,
                &fixture.candidates,
                &fixture.report(MergeFinalStatus::Landed),
                &observed
            )
            .is_err()
        );
        assert_eq!(fixture.db.list_merge_queue().unwrap(), old);
        fixture.assert_no_lifecycle();
    }

    #[test]
    fn stale_queue_observation_does_not_move_folders_or_overwrite_new_owner() {
        let fixture = Fixture::new();
        let observed = observe_outcomes(&fixture.db, &fixture.candidates).unwrap();
        let other = Db::open_at(&fixture.db_path).unwrap();
        other
            .enqueue_merge(&fixture.worktree, "replacement", "other-target")
            .unwrap();
        let replacement = other.list_merge_queue().unwrap();
        assert!(
            persist(
                &fixture.config,
                &fixture.repo,
                &fixture.db,
                &fixture.candidates,
                &fixture.report(MergeFinalStatus::Landed),
                &observed
            )
            .is_err()
        );
        assert_eq!(fixture.db.list_merge_queue().unwrap(), replacement);
        fixture.assert_no_lifecycle();
    }

    #[test]
    fn registry_reassignment_refuses_final_state_without_lifecycle() {
        let fixture = Fixture::new();
        let observed = observe_outcomes(&fixture.db, &fixture.candidates).unwrap();
        let old = fixture.db.list_merge_queue().unwrap();
        let other = Db::open_at(&fixture.db_path).unwrap();
        other
            .put_worktree(
                "fixture/replacement",
                fixture.repo.to_str().unwrap(),
                &fixture.worktree,
                "replacement",
                None,
                Some(fixture.original_folder),
            )
            .unwrap();
        assert!(
            persist(
                &fixture.config,
                &fixture.repo,
                &fixture.db,
                &fixture.candidates,
                &fixture.report(MergeFinalStatus::Landed),
                &observed
            )
            .is_err()
        );
        assert_eq!(fixture.db.list_merge_queue().unwrap(), old);
        fixture.assert_no_lifecycle();
    }

    #[test]
    fn contradictory_projection_is_rejected_before_any_row_or_lifecycle_write() {
        let fixture = Fixture::new();
        let old = fixture.db.list_merge_queue().unwrap();
        let observed = observe_outcomes(&fixture.db, &fixture.candidates).unwrap();
        let mut report = fixture.report(MergeFinalStatus::Landed);
        report.unprepared.push("b1".into());
        assert!(
            persist(
                &fixture.config,
                &fixture.repo,
                &fixture.db,
                &fixture.candidates,
                &report,
                &observed
            )
            .is_err()
        );
        assert_eq!(fixture.db.list_merge_queue().unwrap(), old);
        fixture.assert_no_lifecycle();
    }

    #[test]
    fn observation_is_captured_before_fold_and_never_refreshed_afterwards() {
        let fixture = Fixture::new();
        let db = Db::open_at(&fixture.db_path).unwrap();
        let report = run_observed_fold(
            Ok(db),
            &fixture.config,
            &fixture.repo,
            &fixture.candidates,
            |_| {
                fixture
                    .db
                    .enqueue_merge(&fixture.worktree, "replacement", "other-target")?;
                Ok(fixture.report(MergeFinalStatus::Landed))
            },
        )
        .unwrap();
        assert!(
            report.advanced,
            "Git outcome is independent from refused bookkeeping"
        );
        assert!(
            report
                .bookkeeping_error
                .as_deref()
                .unwrap()
                .contains("queue changed")
        );
        assert!(report.request_result().is_err());
        assert_eq!(
            fixture.db.list_merge_queue().unwrap()[0].branch,
            "replacement"
        );
        fixture.assert_no_lifecycle();
    }

    #[test]
    fn failed_pre_fold_observation_skips_later_persistence_without_fresh_guard() {
        let fixture = Fixture::new();
        let old = fixture.db.list_merge_queue().unwrap();
        let report = run_observed_fold(
            Err(anyhow::anyhow!(
                "private observation unavailable\u{061c}\u{200e}\u{200f}\u{202e}\u{2066}\u{2069}"
            )),
            &fixture.config,
            &fixture.repo,
            &fixture.candidates,
            |_| Ok(fixture.report(MergeFinalStatus::Landed)),
        )
        .unwrap();
        assert!(report.advanced);
        assert!(
            report
                .bookkeeping_error
                .as_deref()
                .unwrap()
                .chars()
                .all(super::super::diagnostic_char)
        );
        assert!(
            report
                .bookkeeping_error
                .as_deref()
                .unwrap()
                .contains("private observation unavailable")
        );
        assert!(report.request_result().is_err());
        assert_eq!(fixture.db.list_merge_queue().unwrap(), old);
        fixture.assert_no_lifecycle();
    }

    #[test]
    fn missing_observation_and_bystander_report_are_not_permission_to_write() {
        let fixture = Fixture::new();
        let old = fixture.db.list_merge_queue().unwrap();
        let report = fixture.report(MergeFinalStatus::Landed);
        assert!(
            persist(
                &fixture.config,
                &fixture.repo,
                &fixture.db,
                &fixture.candidates,
                &report,
                &OutcomeObservations(HashMap::new())
            )
            .is_err()
        );
        let observed = observe_outcomes(&fixture.db, &fixture.candidates).unwrap();
        let mut report = report;
        report.unprepared.push("bystander".into());
        assert!(
            persist(
                &fixture.config,
                &fixture.repo,
                &fixture.db,
                &fixture.candidates,
                &report,
                &observed
            )
            .is_err()
        );
        assert_eq!(fixture.db.list_merge_queue().unwrap(), old);
        fixture.assert_no_lifecycle();
    }
}
