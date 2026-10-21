//! Trusted Lead-only producer for commit-bound pipeline gate attestations.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::fsperm::{PrivateDir, ensure_private_dir_beneath};
use thegn_core::pipeline_gate_attest::{
    AttestationContext, AttestationRefusal, GateAttestation, GateOutcome, GateRunner, TrustedGate,
    evaluate_latest,
};
use thegn_core::pipeline_merge_admission::{GatingRow, PipelineMergeRefused, gating_row};
use thegn_core::store::NotificationStore;
use thegn_core::util;

static GATE_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);
use thegn_core::{GateAttestationRefusal, StoredGateAttestation};

pub(super) fn trusted_gate_for(
    row: &thegn_core::issue::AgentDispatch,
    latest: Option<&StoredGateAttestation>,
) -> TrustedGate {
    let Some(latest) = latest else {
        return evaluate_latest(None, Err("no attestation"));
    };
    let Some(attestation) = latest.attestation() else {
        return TrustedGate::Refused {
            attestation_id: latest.id,
            refusal: AttestationRefusal::UntrustedRunner,
        };
    };
    let context = observe_attestation_context(row, &attestation);
    evaluate_latest(
        Some((latest.id, &attestation, latest.outcome, &latest.reason)),
        context.as_ref().map_err(String::as_str),
    )
}

/// Admit a local merge enqueue only when its newest pipeline row has a trusted
/// attestation for both the observed worktree HEAD and the enqueued candidate.
pub(crate) fn pipeline_merge_admission(
    db: &Db,
    worktree: &str,
    candidate_oid: &str,
    admin_why: Option<&str>,
) -> Result<()> {
    if let Some(why) = admin_why
        && why.trim().is_empty()
    {
        anyhow::bail!("--admin requires a non-empty --why");
    }
    let canonical = std::fs::canonicalize(worktree)
        .unwrap_or_else(|_| std::path::PathBuf::from(worktree))
        .to_string_lossy()
        .to_string();
    let rows = db.pipeline_rows_for_worktree(worktree, &canonical)?;
    let (row, gate_state, reason) = match gating_row(&rows) {
        GatingRow::None => return Ok(()),
        GatingRow::Unfinished(row) => (
            row,
            "unfinished".to_string(),
            format!("is still {}", row.status.as_str()),
        ),
        GatingRow::Unrecognized(row) => (
            row,
            "unrecognized".to_string(),
            "has an unrecognized status".to_string(),
        ),
        GatingRow::Finished(row) => {
            let latest = db.latest_gate_attestation(row.id)?;
            let trusted = trusted_gate_for(row, latest.as_ref());
            let reason = if !trusted.is_satisfied() {
                trusted.detail(row.id)
            } else if let Some(attestation) = latest.as_ref()
                && attestation.commit_oid != candidate_oid
            {
                format!(
                    "was gated on {} but the enqueued commit is {candidate_oid}",
                    attestation.commit_oid
                )
            } else {
                return Ok(());
            };
            (row, trusted.token().to_string(), reason)
        }
    };
    if let Some(why) = admin_why {
        let note = thegn_core::pipeline_report::note_text(&format!(
            "merge admitted without a satisfied gate attestation (admin): {why}; gate state {gate_state}: {reason}; candidate {candidate_oid}"
        ))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        db.append_dispatch_note(row.id, &note)?;
        return Ok(());
    }
    Err(anyhow::Error::new(PipelineMergeRefused {
        worktree: worktree.to_string(),
        row: row.id,
        reason,
    }))
}

fn observe_attestation_context(
    row: &thegn_core::issue::AgentDispatch,
    attestation: &GateAttestation,
) -> std::result::Result<AttestationContext, String> {
    let input = PathBuf::from(&row.worktree_path);
    let worktree = std::fs::canonicalize(&input)
        .map_err(|error| format!("cannot canonicalize worktree {}: {error}", input.display()))?;
    let top = util::git_out_allow_empty(&worktree, &["rev-parse", "--show-toplevel"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "worktree root could not be observed".to_string())?;
    let top = std::fs::canonicalize(top.trim())
        .map_err(|error| format!("cannot canonicalize Git worktree root: {error}"))?;
    if top != worktree {
        return Err("dispatch path is not the root of its Git worktree".to_string());
    }
    let branch_ref = util::git_out_allow_empty(&worktree, &["symbolic-ref", "-q", "HEAD"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "branch ref could not be observed (HEAD may be detached)".to_string())?;
    let head_oid = observed_oid(
        &worktree,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        "commit",
    )?;
    let head_tree_oid = observed_oid(&worktree, &["rev-parse", "--verify", "HEAD^{tree}"], "tree")?;
    let repo = util::git_out_allow_empty(
        &worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
    .ok_or_else(|| "repository common directory could not be observed".to_string())?;
    let repo = std::fs::canonicalize(repo)
        .map_err(|error| format!("cannot canonicalize repository common directory: {error}"))?
        .display()
        .to_string();
    let dirty = util::git_out_allow_empty(&worktree, &["status", "--porcelain"])
        .ok_or_else(|| "worktree status could not be observed".to_string())?;
    let log_present = std::fs::metadata(&attestation.log_path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
    Ok(AttestationContext {
        dispatch_row: row.id,
        repo,
        worktree: worktree.display().to_string(),
        branch_ref,
        head_oid,
        head_tree_oid,
        tree_dirty: !dirty.is_empty(),
        log_present,
    })
}

fn observed_oid(
    worktree: &Path,
    args: &[&str],
    label: &str,
) -> std::result::Result<String, String> {
    let value = util::git_out_allow_empty(worktree, args)
        .ok_or_else(|| format!("{label} OID could not be observed"))?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    if !valid_oid(value) {
        return Err(format!("Git returned malformed {label} OID"));
    }
    Ok(value.to_string())
}

#[derive(Debug)]
struct GateRunRefusal {
    id: i64,
    message: String,
}

impl std::fmt::Display for GateRunRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dispatch {}: {}", self.id, self.message)
    }
}

impl std::error::Error for GateRunRefusal {}

fn refusal(id: i64, message: impl Into<String>) -> anyhow::Error {
    GateRunRefusal {
        id,
        message: message.into(),
    }
    .into()
}

pub(super) fn run(cfg: &Config, id: i64, json: bool) -> Result<()> {
    let db = Db::open()?;
    let state = util::xdg_state_home();
    let logs = ensure_private_dir_beneath(&state, Path::new("thegn/pipeline-gates"))
        .context("could not secure pipeline gate log directory")?;
    let stored = gate_row(
        &db,
        cfg,
        id,
        &logs,
        &crate::integrate::gate_tip,
        &util::now_ms,
    )?;
    if json {
        crate::cmd::emit_json(&stored)?;
    } else {
        thegn_core::outln!(
            "gate {} for dispatch {} at {} (log: {})",
            stored.outcome.as_str(),
            stored.dispatch_row,
            &stored.commit_oid[..stored.commit_oid.len().min(12)],
            stored.log_path
        );
    }
    match stored.outcome {
        GateOutcome::Passed => Ok(()),
        GateOutcome::Failed => {
            anyhow::bail!("gate failed for dispatch {id}; see {}", stored.log_path)
        }
        GateOutcome::Error => Err(anyhow::Error::new(crate::cmd::Retryable(anyhow::anyhow!(
            "gate could not run for dispatch {id}: {}; see {}",
            stored.reason,
            stored.log_path
        )))),
    }
}

fn gate_row(
    db: &Db,
    cfg: &Config,
    id: i64,
    logs: &PrivateDir,
    runner: &dyn Fn(
        &Path,
        &str,
        &thegn_core::config::MergeQueueConfig,
    ) -> Result<crate::integrate::GateVerdict>,
    now_ms: &dyn Fn() -> i64,
) -> Result<StoredGateAttestation> {
    let row = db
        .get_dispatch(id)?
        .ok_or_else(|| refusal(id, "row does not exist"))?;
    let worktree_input = PathBuf::from(&row.worktree_path);
    let worktree = std::fs::canonicalize(&worktree_input).map_err(|error| {
        refusal(
            id,
            format!(
                "worktree {} is missing or remote: {error}; restore its local checkout",
                worktree_input.display()
            ),
        )
    })?;
    let top = util::git_out_allow_empty(&worktree, &["rev-parse", "--show-toplevel"])
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            refusal(
                id,
                "worktree is not a local Git worktree; restore its local checkout",
            )
        })?;
    let top = std::fs::canonicalize(top.trim())
        .map_err(|error| refusal(id, format!("cannot resolve worktree root: {error}")))?;
    if top != worktree {
        return Err(refusal(
            id,
            "dispatch path is not the root of its Git worktree",
        ));
    }

    let root = crate::integrate::main_checkout(&worktree).ok_or_else(|| {
        refusal(
            id,
            "worktree has no local repository checkout; restore the repository",
        )
    })?;
    if let Some(reason) = cfg.workspace_overlay_refusal(&root) {
        return Err(refusal(
            id,
            format!(
                "repository configuration is refused: {reason}; repair the trusted workspace overlay"
            ),
        ));
    }
    let mq = cfg.repo_merge_queue(&root);
    if mq.gate_command.trim().is_empty() {
        return Err(refusal(
            id,
            format!(
                "no [merge_queue] gate_command is configured for {}; an empty gate is not evidence; configure it and retry",
                root.display()
            ),
        ));
    }
    let branch_ref = util::git_out_allow_empty(&worktree, &["symbolic-ref", "-q", "HEAD"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            refusal(
                id,
                "worktree has detached HEAD; check out a branch before running the gate",
            )
        })?;
    let commit_oid = required_oid(
        id,
        &worktree,
        &["rev-parse", "--verify", "HEAD^{commit}"],
        "commit",
    )?;
    let tree_oid = required_oid(
        id,
        &worktree,
        &["rev-parse", "--verify", "HEAD^{tree}"],
        "tree",
    )?;
    let repo = util::git_out_allow_empty(
        &worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map(|value| value.trim().to_string())
    .filter(|value| !value.is_empty())
    .ok_or_else(|| refusal(id, "cannot resolve repository common directory"))?;
    let repo = std::fs::canonicalize(repo)
        .map_err(|error| {
            refusal(
                id,
                format!("cannot canonicalize repository common directory: {error}"),
            )
        })?
        .display()
        .to_string();
    let dirty = util::git_out_allow_empty(&worktree, &["status", "--porcelain"])
        .ok_or_else(|| refusal(id, "could not inspect worktree status"))?;
    if !dirty.is_empty() {
        return Err(refusal(
            id,
            "worktree is dirty; commit or stash first: the gate runs the committed tree, not your working files",
        ));
    }

    let started = now_ms();
    let result = runner(&root, &commit_oid, &mq);
    let ended = now_ms();
    let (outcome, exit, reason, verdict_log) = match result {
        Ok(crate::integrate::GateVerdict::Passed) => {
            (GateOutcome::Passed, Some(0), String::new(), String::new())
        }
        Ok(crate::integrate::GateVerdict::Failed { log }) => (
            GateOutcome::Failed,
            None,
            "gate command failed".to_string(),
            log,
        ),
        Ok(crate::integrate::GateVerdict::Error {
            reason,
            log,
            phase,
            class,
        }) => (
            GateOutcome::Error,
            None,
            format!("{}/{}: {reason}", phase.as_str(), class.as_str()),
            log,
        ),
        Err(error) => (
            GateOutcome::Error,
            None,
            format!("runner error: {error:#}"),
            format!("{error:#}"),
        ),
    };
    let sequence = GATE_LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let log_name = format!("row-{id}-{started}-{}-{sequence}.log", std::process::id());
    let body = format!(
        "row: {id}\nrepo: {repo}\nworktree: {}\nbranch: {branch_ref}\ncommit: {commit_oid}\ntree: {tree_oid}\ncommand: {}\nstarted_ms: {started}\nended_ms: {ended}\noutcome: {}\nreason: {reason}\n\n{verdict_log}",
        worktree.display(),
        mq.gate_command,
        outcome.as_str()
    );
    let body = if body.trim().is_empty() {
        "gate runner produced no output\n".to_string()
    } else {
        body
    };
    let log_path = logs
        .write_owner_only_atomic(&log_name, body.as_bytes())?
        .display()
        .to_string();
    let att = GateAttestation {
        dispatch_row: id,
        repo,
        worktree: worktree.display().to_string(),
        branch_ref,
        commit_oid,
        tree_oid,
        command: mq.gate_command,
        started_ms: started,
        ended_ms: ended,
        exit,
        log_path,
        runner: GateRunner::LeadRunner,
    };
    let stored_id = db
        .record_gate_attestation(&att, outcome, &reason)
        .map_err(|error| {
            if error.downcast_ref::<GateAttestationRefusal>().is_some() {
                error
            } else {
                error.context(format!(
                    "could not persist gate attestation for dispatch {id}"
                ))
            }
        })?;
    Ok(StoredGateAttestation {
        id: stored_id,
        dispatch_row: att.dispatch_row,
        repo: att.repo,
        worktree: att.worktree,
        branch_ref: att.branch_ref,
        commit_oid: att.commit_oid,
        tree_oid: att.tree_oid,
        command: att.command,
        started_ms: att.started_ms,
        ended_ms: att.ended_ms,
        exit: att.exit,
        log_path: att.log_path,
        runner: att.runner.as_str().to_string(),
        outcome,
        reason,
    })
}

fn required_oid(id: i64, worktree: &Path, args: &[&str], label: &str) -> Result<String> {
    let value = util::git_out_allow_empty(worktree, args).ok_or_else(|| {
        refusal(
            id,
            format!("cannot resolve {label} OID; repair the worktree and retry"),
        )
    })?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    if !valid_oid(value) {
        return Err(refusal(
            id,
            format!("Git returned malformed {label} OID; repair the repository"),
        ));
    }
    Ok(value.to_string())
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::{gate_row, pipeline_merge_admission, trusted_gate_for};
    use crate::integrate::{GateErrorClass, GatePhase, GateVerdict};
    use std::path::{Path, PathBuf};
    use thegn_core::StoredGateAttestation;
    use thegn_core::config::{Config, MergeQueueConfig};
    use thegn_core::db::Db;
    use thegn_core::fsperm::ensure_private_dir_beneath;
    use thegn_core::issue::{AgentDispatchStatus, NewDispatch};
    use thegn_core::pipeline_gate_attest::{
        AttestationContext, AttestationRefusal, AttestationVerdict, GateAttestation, GateOutcome,
        GateRunner, TrustedGate,
    };
    use thegn_core::store::NotificationStore;
    use thegn_core::util;

    #[expect(
        clippy::disallowed_methods,
        reason = "fixture captures git command output to verify setup"
    )]
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        std::fs::create_dir(&path).unwrap();
        git(&path, &["init", "--quiet"]);
        git(&path, &["config", "user.name", "Test User"]);
        git(&path, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(path.join("file.txt"), "committed\n").unwrap();
        git(&path, &["add", "file.txt"]);
        git(
            &path,
            &["-c", "commit.gpgsign=false", "commit", "-m", "fixture"],
        );
        (dir, path)
    }

    fn setup(
        worktree: &Path,
    ) -> (
        Db,
        i64,
        Config,
        tempfile::TempDir,
        thegn_core::fsperm::PrivateDir,
    ) {
        let db_dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&db_dir.path().join("state.db")).unwrap();
        let id = db
            .put_agent_dispatch(NewDispatch::new(
                "linear:THE-1103",
                &worktree.display().to_string(),
                "fixture",
            ))
            .unwrap();
        let cfg = Config {
            merge_queue: MergeQueueConfig {
                gate_command: "just test".to_string(),
                ..MergeQueueConfig::default()
            },
            ..Config::default()
        };
        let log_dir = tempfile::tempdir().unwrap();
        let logs = ensure_private_dir_beneath(log_dir.path(), Path::new("gates")).unwrap();
        (db, id, cfg, log_dir, logs)
    }

    fn pipeline_setup(
        worktree: &Path,
    ) -> (
        Db,
        i64,
        Config,
        tempfile::TempDir,
        thegn_core::fsperm::PrivateDir,
    ) {
        let db_dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&db_dir.path().join("state.db")).unwrap();
        let path = worktree.display().to_string();
        let id = db
            .put_agent_dispatch(NewDispatch {
                stage: Some("code"),
                artifact_path: Some(".thegn/pipeline/THE-1103/code/1.md"),
                ..NewDispatch::new("linear:THE-1103", &path, "fixture")
            })
            .unwrap();
        let cfg = Config {
            merge_queue: MergeQueueConfig {
                gate_command: "just test".to_string(),
                ..MergeQueueConfig::default()
            },
            ..Config::default()
        };
        let log_dir = tempfile::tempdir().unwrap();
        let logs = ensure_private_dir_beneath(log_dir.path(), Path::new("gates")).unwrap();
        (db, id, cfg, log_dir, logs)
    }

    fn finish_pipeline_row(db: &Db, id: i64) {
        db.update_dispatch_status(id, AgentDispatchStatus::Done)
            .unwrap();
    }

    fn assert_refused(result: anyhow::Result<StoredGateAttestation>, row: i64) {
        let error = result.unwrap_err();
        let refusal = error.downcast_ref::<super::GateRunRefusal>().unwrap();
        assert_eq!(refusal.id, row);
        assert!(!refusal.message.trim().is_empty());
    }

    #[test]
    fn gate_records_a_passing_attestation_bound_to_head() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = setup(&repo_path);
        let commit = git(&repo_path, &["rev-parse", "HEAD"]);
        let tree = git(&repo_path, &["rev-parse", "HEAD^{tree}"]);
        let stored = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        )
        .unwrap();
        assert_eq!(stored.commit_oid, commit);
        assert_eq!(stored.tree_oid, tree);
        assert_eq!(stored.runner, "lead_runner");
        assert!(
            !std::fs::read_to_string(&stored.log_path)
                .unwrap()
                .is_empty()
        );
        let ctx = AttestationContext {
            dispatch_row: id,
            repo: stored.repo.clone(),
            worktree: stored.worktree.clone(),
            branch_ref: stored.branch_ref.clone(),
            head_oid: commit,
            head_tree_oid: tree,
            tree_dirty: false,
            log_present: true,
        };
        let att = GateAttestation {
            dispatch_row: stored.dispatch_row,
            repo: stored.repo.clone(),
            worktree: stored.worktree.clone(),
            branch_ref: stored.branch_ref.clone(),
            commit_oid: stored.commit_oid.clone(),
            tree_oid: stored.tree_oid.clone(),
            command: stored.command.clone(),
            started_ms: stored.started_ms,
            ended_ms: stored.ended_ms,
            exit: stored.exit,
            log_path: stored.log_path.clone(),
            runner: GateRunner::LeadRunner,
        };
        assert_eq!(
            thegn_core::pipeline_gate_attest::attestation_satisfies(&att, &ctx),
            AttestationVerdict::Satisfied
        );
    }

    #[test]
    fn gate_records_a_failed_attestation_and_errors() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = setup(&repo_path);
        let failed = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| {
                Ok(GateVerdict::Failed {
                    log: "failure output".to_string(),
                })
            },
            &|| 101,
        )
        .unwrap();
        assert_eq!(failed.outcome, GateOutcome::Failed);
        assert!(failed.exit.is_none());
        assert!(
            std::fs::read_to_string(&failed.log_path)
                .unwrap()
                .contains("failure output")
        );
        let errored = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| {
                Ok(GateVerdict::Error {
                    reason: "missing tool".to_string(),
                    log: "stderr".to_string(),
                    phase: GatePhase::Gate,
                    class: GateErrorClass::Spawn,
                })
            },
            &|| 102,
        )
        .unwrap();
        assert_eq!(errored.outcome, GateOutcome::Error);
        assert!(errored.exit.is_none());
        assert!(errored.reason.contains("gate/spawn"));
        let runner_error = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Err(anyhow::anyhow!("runner crashed")),
            &|| 103,
        )
        .unwrap();
        assert_eq!(runner_error.outcome, GateOutcome::Error);
        assert!(runner_error.reason.contains("runner error: runner crashed"));
        assert_eq!(db.gate_attestations_for(id, 10).unwrap().len(), 3);
    }

    #[test]
    fn gate_refuses_an_unconfigured_gate_without_recording() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, _, _log_dir, logs) = setup(&repo_path);
        let cfg = Config::default();
        let result = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        );
        assert_refused(result, id);
        assert!(db.gate_attestations_for(id, 10).unwrap().is_empty());
    }

    #[test]
    fn gate_refuses_a_dirty_worktree_without_running() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = setup(&repo_path);
        std::fs::write(repo_path.join("file.txt"), "dirty\n").unwrap();
        let result = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| panic!("dirty worktree must not run gate"),
            &|| 100,
        );
        assert_refused(result, id);
        assert!(db.gate_attestations_for(id, 10).unwrap().is_empty());
    }

    #[test]
    fn gate_refuses_detached_head() {
        let (_repo_dir, repo_path) = repo();
        git(&repo_path, &["checkout", "--detach", "--quiet", "HEAD"]);
        let (db, id, cfg, _log_dir, logs) = setup(&repo_path);
        let result = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        );
        assert_refused(result, id);
        assert!(db.gate_attestations_for(id, 10).unwrap().is_empty());
    }

    #[test]
    fn gate_refuses_a_missing_worktree() {
        let parent = tempfile::tempdir().unwrap();
        let missing = parent.path().join("removed-worktree");
        let (db, id, cfg, _log_dir, logs) = setup(&missing);
        let result = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        );
        assert_refused(result, id);
        assert!(db.gate_attestations_for(id, 10).unwrap().is_empty());
    }

    #[test]
    fn trusted_gate_for_is_satisfied_after_a_gate_run() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = setup(&repo_path);
        gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        )
        .unwrap();
        let row = db.get_dispatch(id).unwrap().unwrap();
        let latest = db.latest_gate_attestation(id).unwrap().unwrap();
        assert!(matches!(
            trusted_gate_for(&row, Some(&latest)),
            TrustedGate::Satisfied { .. }
        ));
    }

    #[test]
    fn trusted_gate_for_is_stale_after_a_new_commit() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = setup(&repo_path);
        gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        )
        .unwrap();
        std::fs::write(repo_path.join("new.txt"), "new commit\n").unwrap();
        git(&repo_path, &["add", "new.txt"]);
        git(
            &repo_path,
            &["-c", "commit.gpgsign=false", "commit", "-m", "new tip"],
        );
        let row = db.get_dispatch(id).unwrap().unwrap();
        let latest = db.latest_gate_attestation(id).unwrap().unwrap();
        assert!(matches!(
            trusted_gate_for(&row, Some(&latest)),
            TrustedGate::Refused {
                refusal: AttestationRefusal::StaleCommit { .. },
                ..
            }
        ));
    }

    #[test]
    fn trusted_gate_for_refuses_a_missing_log() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = setup(&repo_path);
        gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        )
        .unwrap();
        let row = db.get_dispatch(id).unwrap().unwrap();
        let latest = db.latest_gate_attestation(id).unwrap().unwrap();
        std::fs::remove_file(&latest.log_path).unwrap();
        assert!(matches!(
            trusted_gate_for(&row, Some(&latest)),
            TrustedGate::Refused {
                refusal: AttestationRefusal::MissingLog,
                ..
            }
        ));
    }

    #[test]
    fn trusted_gate_for_is_missing_without_an_attestation() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, _cfg, _log_dir, _logs) = setup(&repo_path);
        let row = db.get_dispatch(id).unwrap().unwrap();
        assert_eq!(trusted_gate_for(&row, None), TrustedGate::Missing);
    }

    #[test]
    fn merge_admission_refuses_an_ungated_finished_row() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, _cfg, _log_dir, _logs) = pipeline_setup(&repo_path);
        finish_pipeline_row(&db, id);
        let candidate = git(&repo_path, &["rev-parse", "HEAD"]);
        let result =
            pipeline_merge_admission(&db, &repo_path.display().to_string(), &candidate, None);
        assert!(matches!(
            result,
            Err(error) if error.downcast_ref::<thegn_core::pipeline_merge_admission::PipelineMergeRefused>()
                .is_some_and(|refusal| refusal.reason.contains("dispatch gate"))
        ));
    }

    #[test]
    fn merge_admission_refuses_a_failed_gate_attestation() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = pipeline_setup(&repo_path);
        let failed = gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| {
                Ok(GateVerdict::Failed {
                    log: "fixture failure".to_string(),
                })
            },
            &|| 100,
        )
        .unwrap();
        assert_eq!(failed.outcome, GateOutcome::Failed);
        finish_pipeline_row(&db, id);
        let candidate = git(&repo_path, &["rev-parse", "HEAD"]);
        let result =
            pipeline_merge_admission(&db, &repo_path.display().to_string(), &candidate, None);
        assert!(matches!(
            result,
            Err(error) if error.downcast_ref::<thegn_core::pipeline_merge_admission::PipelineMergeRefused>()
                .is_some_and(|refusal| refusal.reason.contains("did not pass"))
        ));
    }

    #[test]
    fn merge_admission_admits_a_gated_finished_row_for_its_commit() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = pipeline_setup(&repo_path);
        gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        )
        .unwrap();
        finish_pipeline_row(&db, id);
        let candidate = git(&repo_path, &["rev-parse", "HEAD"]);
        pipeline_merge_admission(&db, &repo_path.display().to_string(), &candidate, None).unwrap();
    }

    #[test]
    fn merge_admission_refuses_after_a_new_commit() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, cfg, _log_dir, logs) = pipeline_setup(&repo_path);
        gate_row(
            &db,
            &cfg,
            id,
            &logs,
            &|_, _, _| Ok(GateVerdict::Passed),
            &|| 100,
        )
        .unwrap();
        finish_pipeline_row(&db, id);
        std::fs::write(repo_path.join("next.txt"), "new commit\n").unwrap();
        git(&repo_path, &["add", "next.txt"]);
        git(
            &repo_path,
            &["-c", "commit.gpgsign=false", "commit", "-m", "next"],
        );
        let candidate = git(&repo_path, &["rev-parse", "HEAD"]);
        let result =
            pipeline_merge_admission(&db, &repo_path.display().to_string(), &candidate, None);
        assert!(matches!(
            result,
            Err(error) if error.downcast_ref::<thegn_core::pipeline_merge_admission::PipelineMergeRefused>().is_some()
        ));
    }

    #[test]
    fn merge_admission_refuses_an_unfinished_row() {
        let (_repo_dir, repo_path) = repo();
        let (db, _id, _cfg, _log_dir, _logs) = pipeline_setup(&repo_path);
        let candidate = git(&repo_path, &["rev-parse", "HEAD"]);
        let result =
            pipeline_merge_admission(&db, &repo_path.display().to_string(), &candidate, None);
        assert!(matches!(
            result,
            Err(error) if error.downcast_ref::<thegn_core::pipeline_merge_admission::PipelineMergeRefused>()
                .is_some_and(|refusal| refusal.reason.contains("still queued"))
        ));
    }

    #[test]
    fn merge_admission_admin_override_records_a_note() {
        let (_repo_dir, repo_path) = repo();
        let (db, id, _cfg, _log_dir, _logs) = pipeline_setup(&repo_path);
        finish_pipeline_row(&db, id);
        let candidate = git(&repo_path, &["rev-parse", "HEAD"]);
        pipeline_merge_admission(
            &db,
            &repo_path.display().to_string(),
            &candidate,
            Some("hotfix"),
        )
        .unwrap();
        let notes = db.dispatch_notes(id, None, 0).unwrap();
        assert!(
            notes
                .iter()
                .any(|note| { note.text.contains("hotfix") && note.text.contains(&candidate) })
        );
    }

    #[test]
    fn merge_admission_ignores_worktrees_without_pipeline_rows() {
        let (_repo_dir, repo_path) = repo();
        let db_dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&db_dir.path().join("state.db")).unwrap();
        let candidate = git(&repo_path, &["rev-parse", "HEAD"]);
        pipeline_merge_admission(&db, &repo_path.display().to_string(), &candidate, None).unwrap();
    }
}
