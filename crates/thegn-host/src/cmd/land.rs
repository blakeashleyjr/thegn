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
use thegn_core::outln;
use thegn_core::store::{
    MergeFinalOutcome, MergeFinalStatus, MergeOutcomeObservation, MergeOutcomeOperation,
    MergeOutcomeWrite, WorktreeAuxStore,
};

use crate::integrate::{self, AttemptOutcome};

fn gate_error_message(
    branch: &str,
    reason: &str,
    phase: integrate::GatePhase,
    class: integrate::GateErrorClass,
) -> String {
    let context = crate::cmd::integrate::gate_error_context(Some(phase), Some(class));
    format!(
        "{branch} was NOT gated{context} — {reason}. The branch was not judged; \
         fix the gate environment (see `[merge_queue] gate_setup_command`) \
         and re-run."
    )
}

fn integration_tracking_outcome(
    non_root: bool,
    outcome: &AttemptOutcome,
) -> Option<crate::cmd::session_track::IntegrationOutcome> {
    if !non_root {
        return None;
    }
    match outcome {
        AttemptOutcome::Landed { .. } | AttemptOutcome::UpToDate { .. } => {
            Some(crate::cmd::session_track::IntegrationOutcome::Landed)
        }
        AttemptOutcome::GateFailed { .. } | AttemptOutcome::GateError { .. } => {
            Some(crate::cmd::session_track::IntegrationOutcome::RefusedGate)
        }
        _ => None,
    }
}

#[cfg(test)]
/// Fold `worktree`'s current branch via the shared land helper without queue pinning.
pub(crate) fn land_branch(
    cfg: &Config,
    worktree: &Path,
) -> Result<(String, String, AttemptOutcome)> {
    land_branch_pinned_with_options(cfg, worktree, None, None, false, None)
}

/// Fold `worktree`'s current branch onto the repo target via the fold-actor's
/// CAS land, forcing the land regardless of the configured `auto_land`. Returns
/// `(branch, target, outcome)`. Queue bookkeeping is intentionally left to the
/// caller because this helper is also used by `thegn merge land` and the TUI.
/// This shared helper must not install process-wide signal handlers.
#[cfg(test)]
pub(crate) fn land_branch_pinned(
    cfg: &Config,
    worktree: &Path,
    expected_tip: Option<&str>,
) -> Result<(String, String, AttemptOutcome)> {
    land_branch_pinned_with_options(cfg, worktree, expected_tip, None, false, None)
}

/// What a queue land did, with the lines to show (CLI prints, TUI appends).
pub(crate) struct PulledLand {
    pub branch: String,
    pub target: String,
    pub outcome: AttemptOutcome,
    pub notes: Vec<String>,
}

/// Pure: the refusal when a pull moved the branch past the queued candidate.
fn stale_candidate(
    pre: &crate::provider_git::PreLand,
    expected_tip: Option<&str>,
) -> Option<String> {
    expected_tip?;
    let crate::provider_git::PreLand::Ready {
        report: Some(report),
    } = pre
    else {
        return None;
    };
    let crate::provider_git::PullOutcome::FastForwarded { commits, .. } = &report.outcome else {
        return None;
    };
    Some(format!(
        "pulled {commits} commit(s) from the sandbox into {}; the queued candidate is stale — re-queue it (`thegn merge add`) or land it with `thegn land`",
        report.branch
    ))
}

/// `land_branch_pinned` after `pull_before_land`, as `thegn land` does (THE-2477 Q3).
pub(crate) fn land_branch_pinned_pulled(
    cfg: &Config,
    worktree: &Path,
    expected_tip: Option<&str>,
) -> Result<PulledLand> {
    let pre = crate::provider_git::pull_before_land(cfg, &worktree.to_string_lossy())?;
    let (provider_land, line) = pre_land_quiet(&pre)?;
    if let Some(refusal) = stale_candidate(&pre, expected_tip) {
        bail!("{refusal}");
    }
    let branch_loc_override = provider_land
        .is_some()
        .then(|| thegn_core::remote::GitLoc::Local(worktree.to_path_buf()));
    let (branch, target, outcome) = land_branch_pinned_with_options(
        cfg,
        worktree,
        expected_tip,
        None,
        false,
        branch_loc_override,
    )?;
    Ok(PulledLand {
        branch,
        target,
        outcome,
        notes: line.into_iter().collect(),
    })
}

/// The loc `attempt_land` folds from. `thegn land` lands the branch checked
/// out in `worktree`; its stored loc tells attempt_land whether that worktree is
/// on this host (no ingest) or remote. An override (a provider worktree whose
/// sandbox commits [`crate::provider_git::pull_before_land`] already brought
/// home) skips the lookup.
fn branch_loc_for(
    worktree: &Path,
    branch_loc_override: Option<thegn_core::remote::GitLoc>,
) -> Result<thegn_core::remote::GitLoc> {
    match branch_loc_override {
        Some(loc) => Ok(loc),
        None => thegn_core::remote::GitLoc::for_worktree_op(worktree, "land")
            .map_err(anyhow::Error::msg),
    }
}

fn land_branch_pinned_with_options(
    cfg: &Config,
    worktree: &Path,
    expected_tip: Option<&str>,
    db: Option<&Db>,
    trust_gate: bool,
    branch_loc_override: Option<thegn_core::remote::GitLoc>,
) -> Result<(String, String, AttemptOutcome)> {
    let root = integrate::main_checkout(worktree).context("not inside a git repository")?;
    let branch = match crate::merge_ops::branch_of(worktree) {
        Ok(Some(branch)) => branch,
        Ok(None) => anyhow::bail!("{}: not on a branch (detached HEAD?)", worktree.display()),
        Err(reason) => anyhow::bail!("{}: branch could not be read: {reason}", worktree.display()),
    };
    // THE-515: a refused (ambiguous) trusted overlay must not degrade to the
    // global gate on the one path that forces auto_land on below.
    if let Some(refusal) = cfg.workspace_overlay_refusal(&root) {
        anyhow::bail!("{}: {refusal}", root.display());
    }
    // This IS the manual land, so force it on regardless of queue policy.
    let mut mq = cfg.repo_merge_queue(&root);
    mq.auto_land = true;
    let target = integrate::resolve_target_checked(&mq, &root).map_err(anyhow::Error::msg)?;
    let trusted = if trust_gate {
        let is_root = std::fs::canonicalize(worktree).ok().as_deref() == Some(root.as_path());
        let trusted = if is_root {
            Err("nothing to attest".to_string())
        } else if let Some(db) = db {
            let folded_commit = integrate::resolve_commit_oid(&root, &branch)
                .map_err(|error| format!("could not bind land commit: {error}"));
            let target_tip = integrate::resolve_commit_oid(&root, &target)
                .map_err(|error| format!("could not bind target tip: {error}"));
            match (folded_commit, target_tip) {
                (Ok(folded_commit), Ok(target_tip)) => {
                    crate::cmd::dispatch::gate::trusted_land_gate_id(
                        db,
                        &worktree.to_string_lossy(),
                        &folded_commit,
                        &target_tip,
                        &root,
                    )
                    .and_then(|id| {
                        if let Some(expected) = expected_tip
                            && !expected.eq_ignore_ascii_case(&folded_commit)
                        {
                            Err(format!(
                                "queue pin binding is {expected} but trusted land commit is {folded_commit}"
                            ))
                        } else {
                            Ok((id, folded_commit, target_tip))
                        }
                    })
                }
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        } else {
            Err("trust-gate database is unavailable".to_string())
        };
        match trusted {
            Ok(bound) => Some(bound),
            Err(reason) => {
                outln!("Gate trust refused: {reason}; running the normal gate");
                None
            }
        }
    } else {
        None
    };
    let branch_loc = branch_loc_for(worktree, branch_loc_override)?;
    let outcome = if let Some((attestation_id, folded_commit, target_tip)) = trusted {
        let outcome = integrate::attempt_land_trusted(
            &mq,
            &root,
            &branch,
            &branch_loc,
            &folded_commit,
            &target_tip,
        )?;
        outln!(
            "Gate skipped: trusted attestation {attestation_id} bound to {folded_commit} and target tip {target_tip}"
        );
        outcome
    } else {
        match expected_tip {
            Some(tip) => integrate::attempt_land_pinned(&mq, &root, &branch, &branch_loc, tip)?,
            None => integrate::attempt_land(&mq, &root, &branch, &branch_loc)?,
        }
    };
    Ok((branch, target, outcome))
}

/// `release_sandbox` (`--release-sandbox`): destroy a provider worktree's
/// proven sandbox after a successful land. Off by default: a sandbox holds
/// gitignored state that exists nowhere else, so landing alone never deletes it.
///
/// A provider worktree first pulls its sandbox commits home
/// ([`crate::provider_git::pull_before_land`]) and then lands the **local**
/// branch; a sandbox head kept in a scratch ref refuses the land.
/// `--release-sandbox` on a worktree with no provider sandbox prints why nothing was released.
pub fn run(
    cfg: &Config,
    worktree: Option<String>,
    trust_gate: bool,
    release_sandbox: bool,
) -> Result<()> {
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
    // A provider worktree lands its local branch once the sandbox's commits
    // are home. The pull is explicit here: it may wake or resume the sandbox.
    let pre_land = if non_root {
        crate::provider_git::pull_before_land(cfg, &wt.to_string_lossy())?
    } else {
        crate::provider_git::PreLand::NotProvider
    };
    let provider_land = pre_land_plan(&pre_land)?;
    let (release_sandbox, dropped) = release_flag(&pre_land, release_sandbox);
    if let Some(line) = dropped {
        outln!("{line}");
    }
    let branch_loc_override = provider_land
        .is_some()
        .then(|| thegn_core::remote::GitLoc::Local(wt.clone()));
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
    let guard = crate::platform::InterruptGuard::install();
    let _scope = crate::fold_cancel::scope(move || guard.fired());
    let (branch, target, outcome) = land_branch_pinned_with_options(
        cfg,
        &wt,
        None,
        Some(db.db()),
        trust_gate,
        branch_loc_override,
    )?;
    let tracking_outcome = integration_tracking_outcome(non_root, &outcome);
    if let Some(tracking_outcome) = tracking_outcome
        && let Some(label) = Path::new(&wt).file_name().and_then(|name| name.to_str())
        && let Err(error) = crate::cmd::session_track::track_integration(
            db.db(),
            wt.to_string_lossy().as_ref(),
            label,
            "land",
            tracking_outcome,
        )
    {
        // Land is authoritative for Git; roster tracking is best effort and
        // must not turn a landed or refused fold into an ambiguous retry.
        outln!("! integration tracking unavailable: {error}");
    }
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
        AttemptOutcome::Landed {
            commit, resyncs, ..
        } => {
            // Persist first. If this post-CAS write is degraded, do not run the
            // row-dependent lifecycle filing against an untrusted observation.
            let degraded = record_landed(&commit).or_else(|| file_landed(&branch));
            outln!(
                "✓ landed {branch} → {target} @ {}",
                &commit[..commit.len().min(12)]
            );
            if mq.gate_on
                && !mq.gate_command.trim().is_empty()
                && !integrate::gate_isolated(mq.gate_sandbox)
            {
                outln!("{}", integrate::UNTRUSTED_GATE_LABEL);
            }
            if let Some(detail) = degraded {
                thegn_core::msg::warn(&format!("{branch} landed, but {detail}"));
                outln!("! {detail}");
            }
            crate::integrate::report_resyncs(&target, &resyncs);
            if let Some(sandbox_gone) = provider_land {
                let ops = LiveRelease { cfg, db: db.db() };
                release_after_land(&ops, &wt.to_string_lossy(), release_sandbox, sandbox_gone);
            }
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
            if let Some(sandbox_gone) = provider_land {
                let ops = LiveRelease { cfg, db: db.db() };
                release_after_land(&ops, &wt.to_string_lossy(), release_sandbox, sandbox_gone);
            }
        }
        // A failed land must exit non-zero: `thegn land` is scripted (CI, the
        // fold-actor, git aliases), so an exit-0 conflict/gate-red would look
        // like a success. The message rides the returned error (anyhow prints it).
        AttemptOutcome::Conflict {
            paths,
            submodule_conflicts,
            driver_refusal,
        } => {
            let detail =
                crate::integrate::conflict_details(&paths, &submodule_conflicts).join(", ");
            anyhow::bail!(
                "{branch} conflicts with {target}: {detail}{}",
                driver_refusal
                    .map(|reason| format!("\n{reason}"))
                    .unwrap_or_default()
            );
        }
        AttemptOutcome::GateFailed { log } => {
            // Name what failed. The gate already captured its log; discarding it
            // meant every gate-red land sent the operator to re-run the suite by
            // hand in the gate worktree just to learn which test broke — which
            // happened for THE-11, THE-19, THE-7, THE-22 and THE-51 in one drain.
            let failures = gate_failure_digest(&log);
            let label = if integrate::gate_isolated(mq.gate_sandbox) {
                String::new()
            } else {
                format!(" — {}", integrate::UNTRUSTED_GATE_LABEL)
            };
            if failures.is_empty() {
                anyhow::bail!("{branch} breaks the build (gate red){label}; not landed.");
            }
            anyhow::bail!("{branch} breaks the build (gate red){label}; not landed:\n{failures}");
        }
        AttemptOutcome::GateError {
            reason,
            phase,
            class,
            ..
        } => {
            // The gate never ran, so this says nothing about the branch. Naming
            // it "breaks the build" would be a false accusation.
            anyhow::bail!("{}", gate_error_message(&branch, &reason, phase, class));
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

/// Print the pre-land pull and decide how to land. `None` = not a provider
/// worktree (land from its stored loc); `Some(sandbox_gone)` = land the local
/// branch. A kept sandbox head is an error.
fn pre_land_plan(pre: &crate::provider_git::PreLand) -> Result<Option<bool>> {
    let (provider_land, line) = pre_land_quiet(pre)?;
    if let Some(line) = line {
        outln!("{line}");
    }
    Ok(provider_land)
}

fn pre_land_quiet(pre: &crate::provider_git::PreLand) -> Result<(Option<bool>, Option<String>)> {
    use crate::provider_git::PreLand;
    match pre {
        PreLand::NotProvider => Ok((None, None)),
        PreLand::Ready { report } => {
            let line = report
                .as_ref()
                .map(|report| format!("pulled {}: {}", report.branch, report.summary()));
            Ok((Some(false), line))
        }
        PreLand::SandboxGone => Ok((
            Some(true),
            Some("sandbox gone; landing the local branch".to_owned()),
        )),
        PreLand::Refused(message) => bail!("{message}"),
    }
}

/// Whether `--release-sandbox` can act after this pre-land result, and the
/// line to print when it cannot. Never silent: a dropped flag always says so.
fn release_flag(pre: &crate::provider_git::PreLand, asked: bool) -> (bool, Option<&'static str>) {
    use crate::provider_git::PreLand;
    if !asked {
        return (false, None);
    }
    match pre {
        PreLand::NotProvider => (
            false,
            Some(
                "! --release-sandbox ignored: this worktree has no provider sandbox (its env is not provider-placed, or a complete provider listing lacks it); nothing was released",
            ),
        ),
        PreLand::Ready { report: None } => (
            false,
            Some(
                "! --release-sandbox ignored: no pull ran for this worktree (read-only git policy, or not a provider env); the sandbox is kept",
            ),
        ),
        PreLand::Ready { report: Some(_) } | PreLand::SandboxGone => (true, None),
        PreLand::Refused(_) => (asked, None),
    }
}

/// The I/O the post-land sandbox release needs, as a seam for tests.
trait SandboxRelease {
    /// `(env, sandbox name)` of the worktree's provider sandbox.
    fn target(&self, worktree: &str) -> std::result::Result<(String, String), String>;
    /// Destroy it: `Destroyed` only once a complete inventory no longer lists it.
    fn destroy(
        &self,
        worktree: &str,
        env: &str,
    ) -> std::result::Result<crate::agent_teardown::Teardown, String>;
    /// Point the worktree's stored location back at the local checkout.
    fn clear_location(&self, worktree: &str) -> std::result::Result<(), String>;
    /// Print one user-facing line (a seam so tests can read what was said).
    fn say(&self, line: &str) {
        outln!("{line}");
    }
}

struct LiveRelease<'a> {
    cfg: &'a Config,
    db: &'a Db,
}

impl SandboxRelease for LiveRelease<'_> {
    fn target(&self, worktree: &str) -> std::result::Result<(String, String), String> {
        let env = crate::cmd::env::resolve_for(self.cfg, Some(worktree.to_string()))
            .map_err(|error| format!("resolve the worktree's env: {error:#}"))?;
        let name =
            crate::provider_factory::provider_sandbox_name_checked(self.cfg, worktree, &env.name)
                .map_err(|error| format!("resolve the worktree's provider sandbox: {error:#}"))?
                .filter(|name| !name.is_empty())
                .ok_or_else(|| format!("env {} has no sandbox for {worktree}", env.name))?;
        Ok((env.name, name))
    }

    // off-loop: `thegn land` is a CLI; the destroy blocks on the provider.
    // The helper owns the usable-provider check and the inventory proof.
    fn destroy(
        &self,
        worktree: &str,
        env: &str,
    ) -> std::result::Result<crate::agent_teardown::Teardown, String> {
        if !self.cfg.env.contains_key(env) {
            return Err(format!("env {env} is not configured"));
        }
        let sandbox = self
            .target(worktree)
            .map(|(_, name)| name)
            .unwrap_or_else(|_| "of this worktree".into());
        let notes = crate::provider_git::destroy_guard(
            crate::provider_git::pull_before_destroy(self.cfg, worktree),
            &sandbox,
            false,
        )?;
        for note in &notes {
            self.say(&note.text);
        }
        crate::agent_teardown::destroy_provider_sandbox_with(self.cfg, worktree, env)
    }

    fn clear_location(&self, worktree: &str) -> std::result::Result<(), String> {
        use thegn_core::store::WorkspaceStore;
        self.db
            .restore_worktree_location(worktree, None)
            .map_err(|error| format!("clear the stored provider location: {error:#}"))
    }
}

/// What the post-land release did.
#[derive(Debug, PartialEq, Eq)]
enum Release {
    /// `--release-sandbox` not given: the sandbox stays.
    Kept,
    /// The sandbox was destroyed (proven absent), recycled into the warm pool,
    /// or already gone, and the location cleared.
    Released,
    /// The sandbox was already gone and no release was asked for.
    Gone,
    /// The release failed after the land; reported as a warning only.
    Failed,
}

/// After a successful provider land: keep the sandbox (the default: it holds
/// gitignored state that exists nowhere else) or, with `--release-sandbox`,
/// destroy it and clear the worktree's provider location. Runs after the
/// landed line and its bookkeeping; never an error (the land already happened).
fn release_after_land(
    ops: &dyn SandboxRelease,
    worktree: &str,
    release_sandbox: bool,
    sandbox_gone: bool,
) -> Release {
    use crate::agent_teardown::Teardown;
    if !release_sandbox {
        if sandbox_gone {
            return Release::Gone;
        }
        match ops.target(worktree) {
            Ok((_, name)) => ops.say(&format!(
                "sandbox {name} kept (thegn land --release-sandbox, or env reap, removes it)"
            )),
            Err(_) => {
                ops.say("sandbox kept (thegn land --release-sandbox, or env reap, removes it)")
            }
        }
        return Release::Kept;
    }
    let kept = |name: &str, error: String| {
        thegn_core::msg::warn(&format!(
            "landed, but sandbox {name} was not released: {error}"
        ));
        ops.say(&format!("! sandbox {name} kept: {error}"));
        Release::Failed
    };
    let (env, name) = match ops.target(worktree) {
        Ok(target) => target,
        Err(error) => return kept("of this worktree", error),
    };
    // The location is cleared only on proof: an inventory that no longer
    // lists the sandbox (`sandbox_gone` from the pre-land pull, or a confirmed
    // destroy), or a spare reset into the warm pool.
    let (name, done) = if sandbox_gone {
        let done = format!("released sandbox {name}");
        (name, done)
    } else {
        match ops.destroy(worktree, &env) {
            Ok(Teardown::Destroyed { name }) => {
                let done = format!("released sandbox {name}");
                (name, done)
            }
            Ok(Teardown::Recycled { name }) => {
                let done = format!("returned sandbox {name} to the warm pool");
                (name, done)
            }
            Ok(Teardown::NoSandbox) => {
                ops.say("! no sandbox resolved for this worktree; its location is kept");
                return Release::Failed;
            }
            Err(error) => return kept(&name, error),
        }
    };
    if let Err(error) = ops.clear_location(worktree) {
        let detail = if sandbox_gone {
            format!("sandbox {name} is gone, but {error}")
        } else {
            format!("{done}, but {error}")
        };
        thegn_core::msg::warn(&format!("landed; {detail}"));
        ops.say(&format!("! {detail}"));
        return Release::Failed;
    }
    ops.say(&done);
    Release::Released
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
        operation: MergeOutcomeOperation::RawLand,
        result_oid: Some(result_oid),
        conflict_paths: None,
        error_detail: None,
        // A raw land has no gated plan.
        provenance: None,
    };
    match db.persist_merge_outcome(observed, &outcome)? {
        MergeOutcomeWrite::Written => Ok(()),
        MergeOutcomeWrite::RegistryChanged => {
            bail!("worktree registry changed after Git land; landed row not recorded")
        }
        MergeOutcomeWrite::QueueChanged => {
            bail!("merge queue changed after Git land; landed row not recorded")
        }
        MergeOutcomeWrite::Claimed => {
            bail!("merge row is claimed by an active drain; landed row not recorded")
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
    use std::fmt::Write as _;

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
            writeln!(log, "error[E0{i:03}]: distinct compile error {i}")
                .expect("writing to String cannot fail");
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
mod land_tracking_tests {
    use super::{AttemptOutcome, integration_tracking_outcome};

    #[test]
    fn root_land_writes_no_tracking_row() {
        let outcome = AttemptOutcome::GateFailed { log: String::new() };
        assert!(integration_tracking_outcome(false, &outcome).is_none());
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
        // Put a THE-596-style cleanup hold on the row through the public writer.
        // `Db::conn` is private to thegn-core, so reaching for it here did not
        // compile. This also asserts the case that actually matters: a second land
        // must not wipe an `error_detail` hold a previous collection recorded.
        db.update_merge_status(
            WORKTREE,
            "landed",
            Some("first-fold"),
            None,
            Some("keep-me"),
        )
        .unwrap();
        let before = db.list_merge_queue().unwrap().pop().unwrap();
        assert_eq!(before.error_detail.as_deref(), Some("keep-me"));

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

    #[test]
    fn gate_error_message_names_phase_and_class() {
        let message = super::gate_error_message(
            "feature",
            "timeout",
            crate::integrate::GatePhase::Gate,
            crate::integrate::GateErrorClass::Timeout,
        );
        assert_eq!(
            message,
            "feature was NOT gated (gate/timeout) — timeout. The branch was not judged; \
             fix the gate environment (see `[merge_queue] gate_setup_command`) \
             and re-run."
        );
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "creates an isolated git worktree fixture"
    )]
    fn land_refuses_when_the_location_is_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let wt = temp.path().join("wt");
        std::fs::create_dir(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let output = thegn_core::util::git_cmd(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@e")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@e")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "init", "-q", "-b", "main"],
        );
        git(
            &repo,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "base",
            ],
        );
        let root_head = git(&repo, &["rev-parse", "main"]);
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );
        git(
            &wt,
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@e",
                "commit",
                "--allow-empty",
                "-q",
                "-m",
                "feature",
            ],
        );
        let state = crate::testenv::IsolatedState::new();
        std::fs::create_dir_all(state.state_home().join("thegn/thegn.db")).unwrap();
        let result = super::land_branch_pinned(&thegn_core::config::Config::default(), &wt, None);
        assert!(matches!(result, Err(error) if format!("{error:#}").contains("land refused")));
        assert_eq!(git(&repo, &["rev-parse", "main"]), root_head);
        // A provider worktree whose sandbox commits are home lands the local
        // branch through the override: the stored location is never read.
        let overridden = super::land_branch_pinned_with_options(
            &thegn_core::config::Config::default(),
            &wt,
            None,
            None,
            false,
            Some(thegn_core::remote::GitLoc::Local(wt.clone())),
        );
        if let Err(error) = overridden {
            assert!(
                !format!("{error:#}").contains("cannot read where"),
                "the override must skip the location lookup: {error:#}"
            );
        }
    }

    #[test]
    fn branch_loc_override_skips_the_location_lookup() {
        let state = crate::testenv::IsolatedState::new();
        std::fs::create_dir_all(state.state_home().join("thegn/thegn.db")).unwrap();
        let wt = std::path::PathBuf::from("/wt/provider-land");
        let local = thegn_core::remote::GitLoc::Local(wt.clone());
        assert!(matches!(
            super::branch_loc_for(&wt, Some(local)).unwrap(),
            thegn_core::remote::GitLoc::Local(path) if path == wt
        ));
        let error = super::branch_loc_for(&wt, None).unwrap_err();
        assert!(format!("{error:#}").contains("land refused"), "{error:#}");
    }

    #[test]
    fn pre_land_plan_lands_local_or_refuses() {
        use crate::provider_git::{PreLand, PullOutcome, PullReport};
        assert_eq!(super::pre_land_plan(&PreLand::NotProvider).unwrap(), None);
        let ready = PreLand::Ready {
            report: Some(PullReport {
                branch: "tg/lane".into(),
                outcome: PullOutcome::UpToDate,
                push: None,
            }),
        };
        assert_eq!(super::pre_land_plan(&ready).unwrap(), Some(false));
        assert_eq!(
            super::pre_land_plan(&PreLand::Ready { report: None }).unwrap(),
            Some(false)
        );
        assert_eq!(
            super::pre_land_plan(&PreLand::SandboxGone).unwrap(),
            Some(true)
        );
        let error = super::pre_land_plan(&PreLand::Refused(
            "kept refs/thegn/pull/x ; land refused: merge the kept ref first".into(),
        ))
        .unwrap_err();
        assert!(format!("{error:#}").contains("land refused"), "{error:#}");
    }

    #[test]
    fn pre_land_quiet_table() {
        use crate::provider_git::{PreLand, PullOutcome, PullReport};

        assert_eq!(
            super::pre_land_quiet(&PreLand::NotProvider).unwrap(),
            (None, None)
        );
        assert_eq!(
            super::pre_land_quiet(&PreLand::Ready { report: None }).unwrap(),
            (Some(false), None)
        );
        let report = PullReport {
            branch: "tg/lane".into(),
            outcome: PullOutcome::UpToDate,
            push: None,
        };
        assert_eq!(
            super::pre_land_quiet(&PreLand::Ready {
                report: Some(report),
            })
            .unwrap(),
            (Some(false), Some("pulled tg/lane: up to date".into()))
        );
        assert_eq!(
            super::pre_land_quiet(&PreLand::SandboxGone).unwrap(),
            (
                Some(true),
                Some("sandbox gone; landing the local branch".into())
            )
        );
        let error = super::pre_land_quiet(&PreLand::Refused("land refused".into())).unwrap_err();
        assert_eq!(format!("{error:#}"), "land refused");
    }

    #[test]
    fn stale_candidate_after_a_fast_forward() {
        use crate::provider_git::{PreLand, PullOutcome, PullReport};

        let fast_forwarded = PreLand::Ready {
            report: Some(PullReport {
                branch: "tg/lane".into(),
                outcome: PullOutcome::FastForwarded {
                    from: "old".into(),
                    to: "new".into(),
                    commits: 2,
                },
                push: None,
            }),
        };
        assert_eq!(
            super::stale_candidate(&fast_forwarded, Some("candidate")),
            Some("pulled 2 commit(s) from the sandbox into tg/lane; the queued candidate is stale — re-queue it (`thegn merge add`) or land it with `thegn land`".into())
        );

        let up_to_date = PreLand::Ready {
            report: Some(PullReport {
                branch: "tg/lane".into(),
                outcome: PullOutcome::UpToDate,
                push: None,
            }),
        };
        assert_eq!(super::stale_candidate(&up_to_date, Some("candidate")), None);
        assert_eq!(super::stale_candidate(&fast_forwarded, None), None);
    }

    #[test]
    fn release_flag_is_never_silent() {
        use super::release_flag;
        use crate::provider_git::{PreLand, PullOutcome, PullReport};

        let ready = PreLand::Ready {
            report: Some(PullReport {
                branch: "tg/lane".into(),
                outcome: PullOutcome::UpToDate,
                push: None,
            }),
        };
        let refused = PreLand::Refused("land refused".into());
        let legacy_line = "! --release-sandbox ignored: no pull ran for this worktree (read-only git policy, or not a provider env); the sandbox is kept";
        let cases = [
            (
                PreLand::NotProvider,
                true,
                false,
                Some("nothing was released"),
            ),
            (
                PreLand::Ready { report: None },
                true,
                false,
                Some(legacy_line),
            ),
            (ready, true, true, None),
            (PreLand::SandboxGone, true, true, None),
            (refused, true, true, None),
            (PreLand::NotProvider, false, false, None),
        ];

        for (pre, asked, expected_release, expected_line) in cases {
            let (release, dropped) = release_flag(&pre, asked);
            assert_eq!(release, expected_release);
            match expected_line {
                Some(expected) if expected == legacy_line => assert_eq!(dropped, Some(expected)),
                Some(expected) => assert!(dropped.unwrap().contains(expected)),
                None => assert_eq!(dropped, None),
            }
        }
    }

    type DestroyOutcome = Result<crate::agent_teardown::Teardown, String>;

    struct FakeRelease {
        destroy_outcome: DestroyOutcome,
        clear_error: Option<String>,
        destroys: std::cell::Cell<usize>,
        clears: std::cell::Cell<usize>,
        said: std::cell::RefCell<Vec<String>>,
    }

    impl Default for FakeRelease {
        fn default() -> Self {
            Self::with(Ok(crate::agent_teardown::Teardown::Destroyed {
                name: "tg-lane-abc".into(),
            }))
        }
    }

    impl FakeRelease {
        fn with(destroy_outcome: DestroyOutcome) -> Self {
            Self {
                destroy_outcome,
                clear_error: None,
                destroys: std::cell::Cell::new(0),
                clears: std::cell::Cell::new(0),
                said: std::cell::RefCell::new(Vec::new()),
            }
        }
        fn said(&self) -> String {
            self.said.borrow().join("\n")
        }
    }

    impl super::SandboxRelease for FakeRelease {
        fn target(&self, _worktree: &str) -> Result<(String, String), String> {
            Ok(("sprite".into(), "tg-lane-abc".into()))
        }
        fn destroy(&self, _worktree: &str, env: &str) -> DestroyOutcome {
            assert_eq!(env, "sprite");
            self.destroys.set(self.destroys.get() + 1);
            self.destroy_outcome.clone()
        }
        fn clear_location(&self, _worktree: &str) -> Result<(), String> {
            self.clears.set(self.clears.get() + 1);
            self.clear_error.clone().map_or(Ok(()), Err)
        }
        fn say(&self, line: &str) {
            self.said.borrow_mut().push(line.to_string());
        }
    }

    #[test]
    fn release_is_opt_in_and_never_fails_the_land() {
        use super::{Release, release_after_land};
        let wt = "/wt/provider-land";

        // Default (no --release-sandbox): zero destroy calls, location kept.
        let fake = FakeRelease::default();
        assert_eq!(release_after_land(&fake, wt, false, false), Release::Kept);
        assert_eq!((fake.destroys.get(), fake.clears.get()), (0, 0));
        assert_eq!(release_after_land(&fake, wt, false, true), Release::Gone);
        assert_eq!((fake.destroys.get(), fake.clears.get()), (0, 0));

        // Opt-in: destroy, then point the worktree back at local.
        let fake = FakeRelease::default();
        assert_eq!(
            release_after_land(&fake, wt, true, false),
            Release::Released
        );
        assert_eq!((fake.destroys.get(), fake.clears.get()), (1, 1));

        // A gone sandbox is not destroyed again; its location is cleared.
        let fake = FakeRelease::default();
        assert_eq!(release_after_land(&fake, wt, true, true), Release::Released);
        assert_eq!((fake.destroys.get(), fake.clears.get()), (0, 1));

        // A destroy error is a warning: the location stays provider.
        let fake = FakeRelease::with(Err("api 500".into()));
        assert_eq!(release_after_land(&fake, wt, true, false), Release::Failed);
        assert_eq!((fake.destroys.get(), fake.clears.get()), (1, 0));

        // A location error after the destroy is also only a warning.
        let fake = FakeRelease {
            clear_error: Some("db locked".into()),
            ..FakeRelease::default()
        };
        assert_eq!(release_after_land(&fake, wt, true, false), Release::Failed);
    }

    #[test]
    fn release_clears_the_location_only_on_proof() {
        use super::{Release, release_after_land};
        use crate::agent_teardown::Teardown;
        let wt = "/wt/provider-land";

        // Destroyed (a complete inventory no longer lists it): one clear.
        let fake = FakeRelease::default();
        assert_eq!(
            release_after_land(&fake, wt, true, false),
            Release::Released
        );
        assert_eq!((fake.destroys.get(), fake.clears.get()), (1, 1));
        assert!(
            fake.said().contains("released sandbox tg-lane-abc"),
            "{}",
            fake.said()
        );

        // An unconfirmed destroy: the location is kept and the line says so.
        let fake = FakeRelease::with(Err(
            "destroy of sandbox tg-lane-abc was sent but could not be confirmed".into(),
        ));
        assert_eq!(release_after_land(&fake, wt, true, false), Release::Failed);
        assert_eq!(fake.clears.get(), 0);
        assert!(
            fake.said().contains("! sandbox tg-lane-abc kept:"),
            "{}",
            fake.said()
        );
        assert!(!fake.said().contains("released"), "{}", fake.said());

        // No sandbox resolved: nothing proven, location kept.
        let fake = FakeRelease::with(Ok(Teardown::NoSandbox));
        assert_eq!(release_after_land(&fake, wt, true, false), Release::Failed);
        assert_eq!(fake.clears.get(), 0);
        assert!(
            fake.said().contains("its location is kept"),
            "{}",
            fake.said()
        );

        // A recycled pool spare: the worktree no longer owns it, so clear once.
        let fake = FakeRelease::with(Ok(Teardown::Recycled {
            name: "tg-spare-1".into(),
        }));
        assert_eq!(
            release_after_land(&fake, wt, true, false),
            Release::Released
        );
        assert_eq!(fake.clears.get(), 1);
        assert!(
            fake.said()
                .contains("returned sandbox tg-spare-1 to the warm pool"),
            "{}",
            fake.said()
        );
    }
}
