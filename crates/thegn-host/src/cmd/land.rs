//! `thegn land` — land the current worktree's branch onto the repo's target
//! branch (`main`) through the fold-actor, without the merge-queue machinery.
//!
//! This is the blessed one-shot alternative to `git checkout main && git merge`
//! or a hand-rolled `git update-ref`: the fold runs in the object DB (no target
//! checkout) and advances the target ref by compare-and-swap, so it lands even
//! when the main checkout's working tree is read-only to the caller (a sandboxed
//! agent). On a successful advance it fast-forwards every worktree that has the
//! target checked out — main or linked — and prints the exact resync command for
//! any it had to leave alone (see `thegn_core::util::resync_branch_checkouts`).
//! A running instance also self-heals on the ref move (see
//! [`crate::git_watch::spawn_main_checkout_heal`]), but that is a belt-and-braces
//! second path: the CLI must not depend on an instance being up.
//!
//! Unlike `thegn merge land`, this neither requires `[merge_queue] enabled`
//! nor uses the queue to select work; it shares the fold/gate/CAS core
//! ([`crate::integrate::attempt_land`]) and records its final landed projection
//! so the normal sweep lifecycle can collect the worktree.

use anyhow::{Context, Result, bail};
use std::path::Path;
use thegn_core::config::Config;
use thegn_core::db::{CompatibleDb, Db, SchemaOperation};
use thegn_core::merge_lifecycle::{LifecycleAction, LifecycleEvent, decide};
use thegn_core::store::{
    MergeFinalOutcome, MergeFinalStatus, MergeOutcomeObservation, MergeOutcomeWrite,
    WorktreeAuxStore,
};
use thegn_core::{outln, util};

use crate::integrate::{self, AttemptOutcome};

/// Fold `worktree`'s current branch onto the repo target via the fold-actor's
/// CAS land, forcing the land regardless of the configured `auto_land`. Returns
/// `(branch, target, outcome)`. Queue bookkeeping is intentionally left to the
/// caller because this helper is also used by `thegn merge land`.
pub(crate) fn land_branch(
    cfg: &Config,
    worktree: &Path,
) -> Result<(String, String, AttemptOutcome)> {
    let root = integrate::main_checkout(worktree).context("not inside a git repository")?;
    let branch = util::git_out(worktree, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{}: not on a branch (detached HEAD?)", worktree.display()))?;
    // THE-515: a refused (ambiguous) trusted overlay must not degrade to the
    // global gate on the one path that forces auto_land on below.
    if let Some(refusal) = cfg.workspace_overlay_refusal(&root) {
        anyhow::bail!("{}: {refusal}", root.display());
    }
    // This IS the manual land, so force it on regardless of queue policy.
    let mut mq = cfg.repo_merge_queue(&root);
    mq.auto_land = true;
    let target = integrate::resolve_target(&mq, &root);
    // `thegn land` lands the branch checked out in `worktree`; its loc tells
    // attempt_land whether that worktree is on this host (no ingest) or remote.
    let branch_loc = thegn_core::remote::GitLoc::for_worktree(worktree);
    let outcome = integrate::attempt_land(&mq, &root, &branch, &branch_loc)?;
    Ok((branch, target, outcome))
}

pub fn run(cfg: &Config, worktree: Option<String>) -> Result<()> {
    let wt = super::resolve_worktree(worktree);
    let root = integrate::main_checkout(&wt).context("not inside a git repository")?;
    let mq = cfg.repo_merge_queue(&root);
    let non_root = Path::new(&wt) != root;
    let lifecycle_required = non_root
        && !matches!(
            decide(&mq, LifecycleEvent::LandedInPlace),
            LifecycleAction::Noop
        );
    // A manual land must record its final queue row even when sidebar filing is
    // disabled. Root lands remain the explicit no-row path.
    let operation = if non_root {
        SchemaOperation::LandLifecycleBookkeeping
    } else {
        SchemaOperation::LandRemoteTargetGuard
    };
    // Keep the compatibility handle (and therefore its shared schema lease)
    // through the git fold and the post-land write. A controller can migrate
    // either before this preflight or after the operation, never in between.
    let db = open_land_db(operation)?;
    if let Some(msg) = crate::merge_ops::remote_target_guard(db.db(), &root)? {
        anyhow::bail!("{msg}");
    }
    // Capture the registry/queue identity before any fold, gate, or CAS work.
    // A later reassignment must degrade bookkeeping rather than overwrite the
    // new owner. Root lands intentionally do not observe or write a row.
    let observed = if non_root {
        Some(
            db.db()
                .observe_merge_outcome(&wt.to_string_lossy())
                .context("capturing merge outcome observation")?,
        )
    } else {
        None
    };
    let (branch, target, outcome) = land_branch(cfg, &wt)?;
    // On a successful land, file the worktree into the Merged folder — the same
    // destination a queue land reaches under `move`/`expire`. `thegn land` shares
    // the fold/gate/CAS core with the queue but deliberately leaves the worktree
    // in place (no worktree/branch removal); its landed queue row starts the
    // same grace-period lifecycle as a queue land, so
    // `LandedInPlace` (file, never remove) is the deliberate event: it degrades
    // the destructive `remove`/`detach` arms to a plain filing because a scripted
    // `thegn land` is typically run from *inside* the worktree being landed and
    // must not delete the caller's cwd. Under `on_landed = "off"` it instead
    // clears any stale "Merging"/"Needs attention" membership its enqueue left, so
    // a fold-actor land never strands the worktree — the sidebar/queue de-sync.
    // The schema/feature preflight above means an older compatible DB cannot
    // silently skip this write. A later I/O failure occurs after git has already
    // landed, so report an explicit degraded result rather than returning an
    // ambiguous non-zero status that encourages a destructive retry.
    let file_landed = |branch: &str| -> Option<String> {
        if !lifecycle_required {
            return None;
        }
        crate::merge_lifecycle::apply_landed_in_place_checked(
            // Repo-resolved, so a `[workspace.<slug>]` folder setting is
            // honored here as well as on the land itself.
            &mq,
            db.db(),
            &root,
            &wt.to_string_lossy(),
            branch,
        )
        .err()
        .map(|error| format!("sidebar lifecycle bookkeeping unavailable: {error}"))
    };
    let record_landed = |commit: &str| -> Option<String> {
        if !non_root {
            return None;
        }
        let Some(observed) = observed.as_ref() else {
            return Some("landed row observation unavailable".into());
        };
        persist_landed_row(db.db(), observed, &wt, &branch, &root, &target, commit)
            .err()
            .map(|error| format!("landed-row bookkeeping unavailable: {error}"))
    };
    match outcome {
        AttemptOutcome::Landed { commit, resyncs } => {
            // Persist first. If this post-CAS write is degraded, do not run the
            // row-dependent lifecycle filing against an untrusted observation.
            let degraded = record_landed(&commit).or_else(|| file_landed(&branch));
            outln!(
                "✓ landed {branch} → {target} @ {}",
                &commit[..commit.len().min(12)]
            );
            if let Some(detail) = degraded {
                thegn_core::msg::warn(&format!("{branch} landed, but {detail}"));
                outln!("! {detail}");
            }
            crate::integrate::report_resyncs(&target, &resyncs);
        }
        AttemptOutcome::UpToDate { commit } => {
            // UpToDate has the same verified commit identity and must repair
            // the missing-row case without refreshing an existing landed row.
            let degraded = record_landed(&commit).or_else(|| file_landed(&branch));
            outln!("{branch} already in {target}.");
            if let Some(detail) = degraded {
                thegn_core::msg::warn(&format!("{branch} is up to date, but {detail}"));
                outln!("! {detail}");
            }
        }
        // A failed land must exit non-zero: `thegn land` is scripted (CI, the
        // fold-actor, git aliases), so an exit-0 conflict/gate-red would look
        // like a success. The message rides the returned error (anyhow prints it).
        AttemptOutcome::Conflict {
            paths,
            submodule_conflicts,
        } => {
            let detail =
                crate::integrate::conflict_details(&paths, &submodule_conflicts).join(", ");
            anyhow::bail!("{branch} conflicts with {target}: {detail}");
        }
        AttemptOutcome::GateFailed { log } => {
            // Name what failed. The gate already captured its log; discarding it
            // meant every gate-red land sent the operator to re-run the suite by
            // hand in the gate worktree just to learn which test broke — which
            // happened for THE-11, THE-19, THE-7, THE-22 and THE-51 in one drain.
            let failures = gate_failure_digest(&log);
            if failures.is_empty() {
                anyhow::bail!("{branch} breaks the build (gate red); not landed.");
            }
            anyhow::bail!("{branch} breaks the build (gate red); not landed:\n{failures}");
        }
        AttemptOutcome::GateError { reason, .. } => {
            // The gate never ran, so this says nothing about the branch. Naming
            // it "breaks the build" would be a false accusation.
            anyhow::bail!(
                "{branch} was NOT gated — {reason}. The branch was not judged; \
                 fix the gate environment (see `[merge_queue] gate_setup_command`) \
                 and re-run."
            );
        }
        AttemptOutcome::Unreachable { detail } => {
            anyhow::bail!("{branch}: {detail}");
        }
        AttemptOutcome::Ready { .. } => {
            // Unreachable with auto_land forced on, but handle for completeness.
            anyhow::bail!("{branch} is ready but was not landed.");
        }
    }
    Ok(())
}

/// Persist the final landed projection for a non-root manual land. The caller
/// has already established the Git outcome; this function only uses the
/// validated observation-aware writer and never hand-rolls queue SQL.
fn persist_landed_row(
    db: &Db,
    observed: &MergeOutcomeObservation,
    worktree: &Path,
    branch: &str,
    repo_root: &Path,
    target_branch: &str,
    result_oid: &str,
) -> Result<()> {
    let worktree = worktree
        .to_str()
        .context("manual land worktree path is not UTF-8")?;
    let repo_root = repo_root
        .to_str()
        .context("manual land repository path is not UTF-8")?;
    let outcome = MergeFinalOutcome {
        worktree,
        branch,
        repo_root,
        target_branch,
        location: "",
        status: MergeFinalStatus::Landed,
        result_oid: Some(result_oid),
        conflict_paths: None,
        error_detail: None,
    };
    match db.persist_merge_outcome(observed, &outcome)? {
        MergeOutcomeWrite::Written => Ok(()),
        MergeOutcomeWrite::RegistryChanged => {
            bail!("worktree registry changed after Git land; landed row not recorded")
        }
        MergeOutcomeWrite::QueueChanged => {
            bail!("merge queue changed after Git land; landed row not recorded")
        }
    }
}

/// Compatibility access itself never creates or migrates. On a truly fresh
/// install, bootstrap is an explicit, separately-authorized normal open; the
/// retry then returns the same declared no-migration handle as every other
/// `land` invocation.
fn open_land_db(operation: SchemaOperation) -> Result<CompatibleDb> {
    if let Some(db) = Db::open_compatible(operation)? {
        return Ok(db);
    }
    drop(Db::open().context("initializing state DB for land")?);
    Db::open_compatible(operation)?
        .with_context(|| "state DB disappeared while preparing land compatibility access")
}

/// Pull the actionable lines out of a gate log: nextest `FAIL [...]` rows,
/// compiler `error[EXXXX]` lines, and panic sites. Bounded, because a gate log
/// can be tens of thousands of lines and the point is to name the failure, not
/// to reprint the run.
fn gate_failure_digest(log: &str) -> String {
    const MAX: usize = 12;
    let mut seen: Vec<&str> = Vec::new();
    for line in log.lines() {
        let t = line.trim();
        let interesting = t.starts_with("FAIL [")
            || t.starts_with("error[")
            || (t.starts_with("error:") && !t.contains("test run failed"))
            || t.contains("panicked at");
        if interesting && !seen.contains(&t) {
            seen.push(t);
            if seen.len() == MAX {
                break;
            }
        }
    }
    seen.iter()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod land_digest_tests {
    use super::gate_failure_digest;

    #[test]
    fn digest_names_failing_tests_and_skips_the_noise() {
        let log = "\
   Compiling thegn-core v0.1.0
        PASS [   0.01s] (1/2) thegn-core a::b
        FAIL [   0.03s] (2/2) thegn-core notification::tests::as_str_matches_serde_snake_case
    thread 'x' panicked at crates/thegn-core/src/notification.rs:440:9:
     Summary [  9.2s] 2 tests run: 1 passed, 1 failed
error: test run failed
";
        let d = gate_failure_digest(log);
        assert!(d.contains("as_str_matches_serde_snake_case"), "{d}");
        assert!(d.contains("panicked at"), "{d}");
        // "test run failed" is the exit banner, not the cause — it would push a
        // real failure out of a bounded digest.
        assert!(!d.contains("test run failed"), "{d}");
        assert!(!d.contains("PASS ["), "{d}");
        assert!(!d.contains("Compiling"), "{d}");
    }

    #[test]
    fn digest_is_bounded_and_deduplicated() {
        let mut log = String::new();
        for _ in 0..50 {
            log.push_str("        FAIL [ 0.01s] (1/1) same::test\n");
        }
        for i in 0..50 {
            log.push_str(&format!("error[E0{i:03}]: distinct compile error {i}\n"));
        }
        let d = gate_failure_digest(&log);
        assert_eq!(d.lines().count(), 12, "digest must stay bounded: {d}");
        assert_eq!(
            d.matches("same::test").count(),
            1,
            "duplicates collapse: {d}"
        );
    }

    #[test]
    fn an_empty_or_clean_log_yields_nothing_to_report() {
        assert!(gate_failure_digest("").is_empty());
        assert!(gate_failure_digest("     Summary [ 1s] 10 tests run: 10 passed\n").is_empty());
    }
}

#[cfg(test)]
mod landed_row_tests {
    use super::persist_landed_row;
    use std::path::Path;
    use thegn_core::db::Db;
    use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

    const ROOT: &str = "/repo";
    const WORKTREE: &str = "/repo/feature";

    fn fixture() -> Db {
        let db = Db::open_memory().unwrap();
        db.put_worktree("repo-feature", ROOT, WORKTREE, "feature", None, None)
            .unwrap();
        db
    }

    #[test]
    fn manual_land_persists_the_exact_fold_identity_for_a_new_row() {
        let db = fixture();
        let observed = db.observe_merge_outcome(WORKTREE).unwrap();
        persist_landed_row(
            &db,
            &observed,
            Path::new(WORKTREE),
            "feature",
            Path::new(ROOT),
            "main",
            "folded-commit",
        )
        .unwrap();
        let row = db.list_merge_queue().unwrap().pop().unwrap();
        assert_eq!(row.status, "landed");
        assert_eq!(row.target_branch, "main");
        assert_eq!(row.result_oid.as_deref(), Some("folded-commit"));
    }

    #[test]
    fn manual_land_reports_a_changed_queue_without_overwriting_it() {
        let db = fixture();
        let observed = db.observe_merge_outcome(WORKTREE).unwrap();
        db.enqueue_merge(WORKTREE, "replacement", "release")
            .unwrap();
        let error = persist_landed_row(
            &db,
            &observed,
            Path::new(WORKTREE),
            "feature",
            Path::new(ROOT),
            "main",
            "folded-commit",
        )
        .unwrap_err();
        assert!(error.to_string().contains("merge queue changed"));
        let row = db.list_merge_queue().unwrap().pop().unwrap();
        assert_eq!(row.branch, "replacement");
        assert_eq!(row.status, "queued");
        assert!(row.result_oid.is_none());
    }

    #[test]
    fn manual_land_projection_keeps_an_existing_landed_row_unchanged() {
        let db = fixture();
        let observed = db.observe_merge_outcome(WORKTREE).unwrap();
        persist_landed_row(
            &db,
            &observed,
            Path::new(WORKTREE),
            "feature",
            Path::new(ROOT),
            "main",
            "first-fold",
        )
        .unwrap();
        db.conn()
            .execute(
                "UPDATE merge_queue SET queued_at=11, updated_at=12, result_oid='first-fold', error_detail='keep-me'",
                [],
            )
            .unwrap();
        let before = db.list_merge_queue().unwrap().pop().unwrap();

        let observed = db.observe_merge_outcome(WORKTREE).unwrap();
        persist_landed_row(
            &db,
            &observed,
            Path::new(WORKTREE),
            "feature",
            Path::new(ROOT),
            "main",
            "second-fold-must-not-replace",
        )
        .unwrap();

        assert_eq!(db.list_merge_queue().unwrap().pop().unwrap(), before);
    }
}
