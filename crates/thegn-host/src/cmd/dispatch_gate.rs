//! Trusted Lead-only producer for commit-bound pipeline gate attestations.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thegn_core::config::Config;
use thegn_core::db::Db;
use thegn_core::fsperm::{PrivateDir, ensure_private_dir_beneath};
use thegn_core::pipeline_gate_attest::{GateAttestation, GateOutcome, GateRunner};
use thegn_core::store::NotificationStore;
use thegn_core::util;

static GATE_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);
use thegn_core::{GateAttestationRefusal, StoredGateAttestation};

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
    use super::gate_row;
    use crate::integrate::{GateErrorClass, GatePhase, GateVerdict};
    use std::path::{Path, PathBuf};
    use thegn_core::StoredGateAttestation;
    use thegn_core::config::{Config, MergeQueueConfig};
    use thegn_core::db::Db;
    use thegn_core::fsperm::ensure_private_dir_beneath;
    use thegn_core::issue::NewDispatch;
    use thegn_core::pipeline_gate_attest::{
        AttestationContext, AttestationVerdict, GateAttestation, GateOutcome, GateRunner,
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
}
