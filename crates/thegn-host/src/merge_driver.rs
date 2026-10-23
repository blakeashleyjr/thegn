//! The agent-driven merge-queue driver: drain queued worktree branches one at a
//! time, dispatching a headless CLI agent to rebase/resolve/fix a branch that
//! doesn't land clean, then re-attempting the fold.
//!
//! This is the autopilot on top of the pure fold engine ([`thegn_core::fold`])
//! and the single-branch land primitive ([`crate::integrate::attempt_land`]). The
//! per-branch loop is: try to land → on a textual conflict or a red gate, run the
//! configured `agent_command` *inside the branch's own worktree* (never the
//! canonical checkout — the agent only makes its branch clean; thegn does the
//! object-DB fold + CAS itself) → re-attempt, up to `agent_max_attempts`.
//!
//! Runs synchronously off the event loop (the CLI calls it directly; the host
//! runs it from `spawn_blocking`). It writes `merge_queue` status transitions as
//! it goes so the panel reflects live state, and reports each transition through a
//! `progress` callback the caller uses to print (CLI) or repaint (host).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use thegn_core::attention::MqStatus;
use thegn_core::config::{Config, ConflictHandoff, MergeQueueConfig};
use thegn_core::db::Db;
use thegn_core::store::{
    MergeCandidate, MergeEnqueueIdentity, MergeStatusFields, WorkspaceStore, WorktreeAuxStore,
};
use thegn_core::util;

use crate::integrate::{self, AttemptOutcome};

#[cfg(test)]
#[path = "merge_driver_status_tests.rs"]
mod status_tests;

#[path = "merge_driver_claim.rs"]
mod claim;

#[cfg(test)]
#[path = "merge_driver_claim_tests.rs"]
mod claim_tests;

/// A worktree branch to drain, as read from the `merge_queue` cache.
#[derive(Debug, Clone)]
pub(crate) struct QueueItem {
    pub worktree: String,
    pub branch: String,
    /// The worktree's `location` descriptor (from the queue row): empty = local,
    /// else an ssh/provider blob. Resolves the branch's `GitLoc` so the drain
    /// knows whether to fetch its tip into the target store (cross-host).
    pub location: String,
}

/// One status transition the driver made, handed to the caller's `progress`
/// callback. Metadata matches the attempted best-effort cache write; a failed
/// write does not turn this notification into proof of durable state.
pub(crate) struct DriveStep<'a> {
    /// The queue row's key — lets the host patch its panel row in place.
    pub worktree: &'a str,
    pub branch: &'a str,
    pub status: &'a str,
    pub detail: &'a str,
    pub fields: &'a MergeStatusFields,
}

/// Summary of a full drain.
#[derive(Debug, Default, Clone)]
pub(crate) struct DriveOutcome {
    pub landed: Vec<String>,
    pub ready: Vec<String>,
    pub deferred: Vec<String>,
    /// Agent dispatch held by environment/isolation policy, not a code verdict.
    pub agent_blocked: Vec<String>,
    pub needs_human: Vec<String>,
    /// Branches whose gate could not RUN — an environment failure, reported
    /// separately from `deferred` so a caller never reads "the branch is bad"
    /// out of "the gate binary is missing".
    pub gate_error: Vec<String>,
    /// Per-branch gate verdict details for machine and human drain reports.
    pub gate_verdicts: Vec<GateVerdictReport>,
    /// Live checkouts of the target branch and what the ref advance did to them.
    /// Advisory; the CLI warns about the ones it could not fast-forward.
    pub resyncs: Vec<thegn_core::util::CheckoutResync>,
    /// Non-fatal problems worth telling the user about — today, an agent handoff
    /// that was configured but could not be resolved. Surfaced rather than
    /// swallowed: silently degrading to "notify" looks identical to a queue that
    /// simply had nothing to fix.
    pub warnings: Vec<String>,
    /// Authoritative queue writes that failed while the drain was running.
    /// A persistence failure aborts the remaining items so the drain fails closed.
    pub persistence_errors: Vec<String>,
    /// Git advanced, but the landed status could not be persisted.
    pub bookkeeping_failed: Vec<RefAdvancedBookkeepingFailed>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateVerdictReport {
    pub branch: String,
    pub verdict: String,
    pub phase: String,
    pub class: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RefAdvancedBookkeepingFailed {
    pub branch: String,
    pub target: String,
    pub old_oid: String,
    pub new_oid: String,
    pub error: String,
}

/// Why a branch didn't land — the material a fixing agent needs.
enum Failure {
    Conflict {
        paths: Vec<String>,
        submodule_conflicts: Vec<thegn_core::submodule::SubmoduleConflict>,
    },
    Gate(String),
}

#[derive(Debug)]
struct CandidateDrift {
    reason: String,
}

#[derive(Debug)]
struct CandidateUnreadable {
    reason: String,
}

fn enqueue_identity_drift(
    recorded: &MergeEnqueueIdentity,
    live: std::result::Result<&MergeEnqueueIdentity, &str>,
    drain_common: std::result::Result<&str, &str>,
) -> std::result::Result<(), String> {
    let live = live.map_err(|reason| format!("worktree identity unavailable: {reason}"))?;
    let drain_common = drain_common
        .map_err(|reason| format!("drain repository identity unavailable: {reason}"))?;
    if drain_common != recorded.common_dir {
        return Err("queued for a different repository than this drain".into());
    }
    if live.common_dir != recorded.common_dir {
        return Err("worktree now belongs to a different repository than at enqueue".into());
    }
    if live.admin_dir != recorded.admin_dir {
        return Err("worktree registration changed since enqueue (removed and re-created?)".into());
    }
    Ok(())
}

impl std::fmt::Display for CandidateDrift {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for CandidateDrift {}

impl std::fmt::Display for CandidateUnreadable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for CandidateUnreadable {}

fn local_git_text(dir: &Path, args: &[&str]) -> Result<String, thegn_core::git_run::GitTextError> {
    let cancel = AtomicBool::new(false);
    thegn_core::git_run::git_run(
        dir,
        args,
        &thegn_core::git_run::GitPolicy::read(thegn_core::remote::GITLOC_READ_DEADLINE),
        &cancel,
    )
    .into_text()
    .map(|text| text.trim().to_string())
}

fn read_absent(error: &thegn_core::git_run::GitTextError) -> bool {
    matches!(
        error,
        thegn_core::git_run::GitTextError::Failed(thegn_core::git_run::GitOutcome::Exit {
            code: Some(1),
            stderr_tail,
        }) if stderr_tail.is_empty()
    )
}

fn classify_oid(
    result: Result<String, thegn_core::git_run::GitTextError>,
) -> Result<Option<String>, String> {
    match result {
        Ok(text)
            if (text.len() == 40 || text.len() == 64)
                && text
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) =>
        {
            Ok(Some(text))
        }
        Ok(_) => Err("git returned a malformed object id".into()),
        Err(error) if read_absent(&error) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

fn unreadable_detail(reason: &str) -> String {
    format!(
        "candidate unreadable: {reason}; held without spending an attempt, retry the row once Git is readable"
    )
}

fn candidate_drift(
    candidate: Option<&MergeCandidate>,
    adopted: Option<&str>,
    live_tip: Option<&str>,
    held: bool,
    fast_forward: bool,
) -> Result<(), String> {
    let Some(candidate) = candidate else {
        return Ok(());
    };
    let expected = adopted.unwrap_or(&candidate.candidate_oid);
    let Some(live_tip) = live_tip else {
        return Err("branch deleted since enqueue".into());
    };
    if expected == live_tip || (held && fast_forward) {
        Ok(())
    } else {
        Err(format!(
            "branch moved since enqueue (expected {}, found {})",
            &expected[..expected.len().min(12)],
            &live_tip[..live_tip.len().min(12)]
        ))
    }
}

/// The commit the drain must land: the verified live tip of a row with a recorded candidate.
fn verified_land_pin(
    candidate: Option<&MergeCandidate>,
    verified_tip: Option<&str>,
) -> Option<String> {
    candidate.and(verified_tip).map(str::to_owned)
}

/// Land `branch`, pinned to `verified_tip` when the drain verified one.
fn land_verified(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    branch: &str,
    location: &thegn_core::remote::GitLoc,
    verified_tip: Option<&str>,
) -> anyhow::Result<AttemptOutcome> {
    match verified_tip {
        Some(tip) => integrate::attempt_land_pinned(cfg, repo_root, branch, location, tip),
        None => integrate::attempt_land(cfg, repo_root, branch, location),
    }
}

fn conflict_detail(
    paths: &[String],
    conflicts: &[thegn_core::submodule::SubmoduleConflict],
) -> String {
    crate::integrate::conflict_details(paths, conflicts).join("\n")
}

fn conflict_fields(
    paths: &[String],
    conflicts: &[thegn_core::submodule::SubmoduleConflict],
) -> MergeStatusFields {
    let context = crate::integrate::conflict_details(&[], conflicts).join("\n");
    MergeStatusFields {
        result_oid: None,
        conflict_paths: (!paths.is_empty()).then(|| paths.join("\n")),
        error_detail: (!context.is_empty()).then_some(context),
    }
}

fn replace_status(
    db: &Db,
    worktree: &str,
    status: MqStatus,
    fields: &MergeStatusFields,
) -> anyhow::Result<()> {
    db.replace_merge_status(worktree, status.as_str(), fields)
}

/// Queue rows belonging to `root`'s repo (the queue is global; a drain is
/// per-repo because the target ref is). Shared by the CLI (`merge` namespace)
/// and the host's in-app drain so both see exactly one membership rule.
///
/// Membership is resolved from the DB (`worktrees.repo_path`) first — that's
/// host-independent, so a queued worktree living on another machine (whose path
/// can't be `git worktree list`ed on this box) is still attributed to its repo
/// instead of being silently dropped. Only an *unregistered* worktree (an
/// ad-hoc CLI enqueue with no DB row) falls back to the local `main_checkout`.
pub(crate) fn rows_for_repo(
    db: &Db,
    root: &Path,
) -> anyhow::Result<Vec<thegn_core::db::MergeQueueRow>> {
    let root_s = root.to_string_lossy();
    let rows = db.list_merge_queue()?;
    rows.into_iter()
        .filter_map(
            |r| match row_belongs_to_repo(db, &r.worktree, &r.location, &root_s, root) {
                Ok(true) => Some(Ok(r)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            },
        )
        .collect()
}

/// One membership test (see [`rows_for_repo`]): DB repo_path when known, else a
/// local `git worktree list` fallback only for an unregistered local worktree.
fn row_belongs_to_repo(
    db: &Db,
    worktree: &str,
    location: &str,
    root_s: &str,
    root: &Path,
) -> anyhow::Result<bool> {
    if let Some(rp) = db.repo_root_for(worktree)?
        && !rp.is_empty()
    {
        return Ok(rp == root_s);
    }
    if !location.trim().is_empty() && location.trim() != "local" {
        tracing::debug!(
            target: "thegn::merge",
            worktree,
            location,
            "cannot attribute remote merge-queue row without a registered repo"
        );
        return Ok(false);
    }
    Ok(integrate::main_checkout(Path::new(worktree)).as_deref() == Some(root))
}

/// Drain `items` one at a time, landing clean branches and dispatching the agent
/// on the rest. `progress` is invoked only after each durable status write.
pub(crate) fn drive_queue(
    cfg: &MergeQueueConfig,
    full: &Config,
    repo_root: &Path,
    db: &Db,
    items: Vec<QueueItem>,
    progress: impl FnMut(&DriveStep),
) -> DriveOutcome {
    let worktree_branches: HashMap<String, String> = items
        .iter()
        .map(|item| (item.worktree.clone(), item.branch.clone()))
        .collect();
    let expected_oids = RefCell::new(HashMap::<String, String>::new());
    let adopted_oids = RefCell::new(HashMap::<String, Result<String, String>>::new());
    drive_queue_with(
        cfg,
        full,
        repo_root,
        db,
        items,
        progress,
        DriveActions {
            attempt: |branch: &str, location: &thegn_core::remote::GitLoc, held: bool| {
                let mut verified_tip: Option<String> = None;
                if let thegn_core::remote::GitLoc::Local(worktree) = location {
                    let key = worktree.to_string_lossy().into_owned();
                    let candidate = db.merge_candidate(&key)?;
                    if let Some(recorded) = db.merge_enqueue_identity(&key)? {
                        let live = crate::merge_ops::local_enqueue_identity(worktree);
                        let drain_common = repo_root
                            .canonicalize()
                            .map_err(|error| error.to_string())
                            .and_then(|root| {
                                util::git_common_dir(&root)
                                    .canonicalize()
                                    .map_err(|error| error.to_string())
                            })
                            .and_then(|path| {
                                path.to_str()
                                    .map(str::to_owned)
                                    .ok_or_else(|| "not valid UTF-8".to_string())
                            });
                        if let Err(reason) = enqueue_identity_drift(
                            &recorded,
                            live.as_ref().map_err(String::as_str),
                            drain_common.as_deref().map_err(String::as_str),
                        ) {
                            return Err(anyhow::Error::new(CandidateDrift { reason }));
                        }
                    } else if candidate.is_some() {
                        return Err(anyhow::Error::new(CandidateDrift {
                            reason: "queued before its worktree identity was recorded (enqueued by an older thegn)"
                                .into(),
                        }));
                    }
                    if let Some(candidate) = candidate.as_ref() {
                        let expected_ref = format!("refs/heads/{branch}");
                        if candidate.branch_ref != expected_ref {
                            return Err(anyhow::Error::new(CandidateDrift {
                                reason: format!(
                                    "queued branch ref changed (expected {expected_ref}, found {})",
                                    candidate.branch_ref
                                ),
                            }));
                        }
                        let adopted = adopted_oids.borrow().get(&key).cloned();
                        let fixer_adopted = adopted.is_some();
                        if let Some(Err(reason)) = adopted.as_ref() {
                            return Err(anyhow::Error::new(CandidateDrift {
                                reason: format!(
                                    "fixer left the worktree off the queued branch: {reason}"
                                ),
                            }));
                        }
                        let adopted_tip = adopted.and_then(Result::ok);
                        let live_tip = match classify_oid(local_git_text(
                            repo_root,
                            &["rev-parse", "--verify", "-q", &candidate.branch_ref],
                        )) {
                            Ok(tip) => tip,
                            Err(reason) => {
                                return Err(anyhow::Error::new(CandidateUnreadable {
                                    reason: format!(
                                        "queued branch tip {}: {reason}",
                                        candidate.branch_ref
                                    ),
                                }));
                            }
                        };
                        let fast_forward = held
                            && !fixer_adopted
                            && live_tip.as_deref().is_some_and(|tip| {
                                integrate::candidate_is_ancestor(
                                    repo_root,
                                    &candidate.candidate_oid,
                                    tip,
                                )
                            });
                        if fast_forward
                            && let Some(tip) = live_tip.as_deref()
                            && !db.adopt_merge_candidate(&key, candidate, tip)?
                        {
                            return Err(anyhow::Error::new(CandidateDrift {
                                reason:
                                    "queued candidate changed while adopting held-branch repair"
                                        .into(),
                            }));
                        }
                        if let Err(reason) = candidate_drift(
                            Some(candidate),
                            adopted_tip.as_deref(),
                            live_tip.as_deref(),
                            held,
                            fast_forward,
                        ) {
                            return Err(anyhow::Error::new(CandidateDrift { reason }));
                        }
                        verified_tip = verified_land_pin(Some(candidate), live_tip.as_deref());
                    }
                    let ref_name = format!("refs/heads/{branch}");
                    match classify_oid(local_git_text(repo_root, &["rev-parse", &ref_name])) {
                        Ok(Some(oid)) => {
                            expected_oids.borrow_mut().insert(key, oid);
                        }
                        Ok(None) => {
                            expected_oids.borrow_mut().remove(&key);
                        }
                        Err(reason) => {
                            expected_oids.borrow_mut().remove(&key);
                            tracing::warn!(
                                target: "thegn::merge",
                                branch,
                                %reason,
                                "queued branch OID unreadable; fixer dispatch will be held"
                            );
                        }
                    }
                }
                #[cfg(test)]
                {
                    typed_read_tests::run_drain_land_test_hook();
                }
                land_verified(cfg, repo_root, branch, location, verified_tip.as_deref())
            },
            floor: |worktree: &str| {
                if let Some(reason) = crate::agent_run::headless_unsupported() {
                    return crate::agent_run::AgentDispatch::InfraHold(reason);
                }
                let Some(branch) = worktree_branches.get(worktree) else {
                    return crate::agent_run::AgentDispatch::InfraHold(
                        "worktree identity drift: queue item identity is unavailable".into(),
                    );
                };
                let expected = expected_oids.borrow().get(worktree).cloned();
                if let Err(reason) = verify_fixer_checkout_for_dispatch(
                    repo_root,
                    worktree,
                    branch,
                    expected.as_deref(),
                ) {
                    return crate::agent_run::AgentDispatch::InfraHold(format!(
                        "worktree identity drift: {reason}"
                    ));
                }
                crate::agent_run::agent_floor_gate(
                    full,
                    worktree,
                    cfg.agent_sandbox,
                    cfg.agent_isolation_floor,
                    cfg.agent_on_floor_miss,
                )
            },
            runner: |template: &str,
                     worktree: &str,
                     branch: &str,
                     target: &str,
                     failure: &Failure,
                     sandbox: Option<thegn_core::sandbox::SandboxSpec>| {
                let result = run_agent(cfg, template, worktree, branch, target, failure, sandbox);
                let key = worktree.to_string();
                match db.merge_candidate(&key) {
                    Ok(Some(_)) => {
                        let verified = verify_fixer_checkout(repo_root, worktree, branch, None)
                            .and_then(|()| {
                                let head = match classify_oid(local_git_text(
                                    Path::new(worktree),
                                    &["rev-parse", "HEAD"],
                                )) {
                                    Ok(Some(head)) => head,
                                    Ok(None) => {
                                        return Err("worktree HEAD is unavailable".to_string());
                                    }
                                    Err(reason) => {
                                        return Err(format!(
                                            "worktree HEAD is unavailable: {reason}"
                                        ));
                                    }
                                };
                                let branch_ref = format!("refs/heads/{branch}");
                                let tip = match classify_oid(local_git_text(
                                    repo_root,
                                    &["rev-parse", "--verify", "-q", &branch_ref],
                                )) {
                                    Ok(Some(tip)) => tip,
                                    Ok(None) => {
                                        return Err("queued branch tip is unavailable".to_string());
                                    }
                                    Err(reason) => {
                                        return Err(format!(
                                            "queued branch tip is unavailable: {reason}"
                                        ));
                                    }
                                };
                                if head != tip {
                                    return Err(format!(
                                        "worktree HEAD {head} differs from branch tip {tip}"
                                    ));
                                }
                                Ok(tip)
                            });
                        adopted_oids.borrow_mut().insert(key, verified);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        adopted_oids.borrow_mut().insert(
                            key,
                            Err(format!(
                                "could not read queued candidate after fixer: {error}"
                            )),
                        );
                    }
                }
                result
            },
        },
    )
}

/// Whether a `git worktree list --porcelain -z` listing registers exactly `worktree` (THE-376 slice d). A malformed
/// listing is an error, never "not registered".
fn listing_registers(listing: &str, worktree: &Path) -> Result<bool, String> {
    let entries = util::parse_worktree_branches(listing)
        .ok_or_else(|| "the queued repository's worktree list is malformed".to_string())?;
    Ok(entries
        .iter()
        .any(|(path, _branch)| Path::new(path) == worktree))
}

/// Confirm the queued local checkout still names the same registered Git
/// repository, branch, and candidate commit before dispatching a fixer.
fn verify_fixer_checkout(
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    expected_oid: Option<&str>,
) -> Result<(), String> {
    let path = Path::new(worktree);
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect worktree path: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err("worktree path is a symlink".into());
    }
    let canonical_worktree = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve worktree path: {error}"))?;
    if path != canonical_worktree {
        return Err("worktree path uses a non-canonical alias".into());
    }
    let top_level = local_git_text(path, &["rev-parse", "--show-toplevel"])
        .map_err(|error| format!("path is not a Git worktree: {error}"))?;
    // GitPolicy::read permits empty stdout, so this explicit empty check is load-bearing.
    if top_level.is_empty() {
        return Err("path is not a Git worktree".into());
    }
    let canonical_top_level = PathBuf::from(top_level)
        .canonicalize()
        .map_err(|error| format!("cannot resolve Git worktree root: {error}"))?;
    if canonical_worktree != canonical_top_level {
        return Err("path does not identify the Git worktree root".into());
    }
    let expected_common = repo_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve repository root: {error}"))?;
    let expected_common = util::git_common_dir(&expected_common)
        .canonicalize()
        .map_err(|error| format!("cannot resolve queued repository identity: {error}"))?;
    let actual_common = util::git_common_dir(&canonical_worktree)
        .canonicalize()
        .map_err(|error| format!("cannot resolve worktree repository identity: {error}"))?;
    if actual_common != expected_common {
        return Err("worktree belongs to a different Git repository".into());
    }
    let cancel = AtomicBool::new(false);
    let listing = thegn_core::git_run::git_run(
        repo_root,
        &util::WORKTREE_LIST_Z,
        &thegn_core::git_run::GitPolicy::read(thegn_core::remote::GITLOC_READ_DEADLINE),
        &cancel,
    )
    .into_text()
    .map_err(|error| format!("could not list the queued repository's worktrees: {error}"))?;
    let registered = listing_registers(&listing, &canonical_worktree)?;
    if !registered {
        return Err("path is not a registered worktree of the queued repository".into());
    }
    let actual_ref = match local_git_text(&canonical_worktree, &["symbolic-ref", "-q", "HEAD"]) {
        Ok(actual_ref) if !actual_ref.is_empty() => actual_ref,
        Ok(_) => return Err("worktree HEAD is detached or unavailable".into()),
        Err(error) if read_absent(&error) => {
            return Err("worktree HEAD is detached or unavailable".into());
        }
        Err(error) => return Err(format!("worktree HEAD is unavailable: {error}")),
    };
    let expected_ref = format!("refs/heads/{branch}");
    if actual_ref != expected_ref {
        return Err(format!("expected {expected_ref}, found {actual_ref}"));
    }
    if let Some(expected_oid) = expected_oid {
        let actual_oid =
            match classify_oid(local_git_text(&canonical_worktree, &["rev-parse", "HEAD"])) {
                Ok(Some(actual_oid)) => actual_oid,
                Ok(None) => return Err("worktree HEAD is unavailable".into()),
                Err(reason) => return Err(format!("worktree HEAD is unavailable: {reason}")),
            };
        if actual_oid != expected_oid {
            return Err("worktree HEAD moved since the queued candidate was folded".into());
        }
    }
    Ok(())
}

fn verify_fixer_checkout_for_dispatch(
    repo_root: &Path,
    worktree: &str,
    branch: &str,
    expected_oid: Option<&str>,
) -> Result<(), String> {
    let expected_oid =
        expected_oid.ok_or_else(|| "queued candidate OID unavailable".to_string())?;
    verify_fixer_checkout(repo_root, worktree, branch, Some(expected_oid))
}

/// Private injection points exercise the actual loop without launching Git or
/// an agent. Production supplies the same concrete operations as before.
struct DriveActions<A, F, R> {
    attempt: A,
    floor: F,
    runner: R,
}

enum PostRelease {
    Apply(thegn_core::merge_lifecycle::LifecycleEvent),
    Landed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetStampAction {
    SkipHeld,
    RetryWithoutStamp,
    Fail,
}

pub(crate) fn target_stamp_action(
    claim_active_error: bool,
    claim: Option<&thegn_core::store::MergeClaimView>,
    now_ms: i64,
) -> TargetStampAction {
    if !claim_active_error {
        return TargetStampAction::Fail;
    }
    match claim {
        Some(view)
            if view.phase != "unclaimed"
                && view.lease_until_ms.is_some_and(|until| until > now_ms) =>
        {
            TargetStampAction::SkipHeld
        }
        _ => TargetStampAction::RetryWithoutStamp,
    }
}

fn expired_recoverable(view: &thegn_core::store::MergeClaimView, now_ms: i64) -> bool {
    matches!(view.phase.as_str(), "claimed_no_child" | "settled")
        && view.lease_until_ms.is_some_and(|until| until < now_ms)
}

/// Recovery uses a claimable status because core validates this field before
/// its expired-claim admission branch, which ignores it. `queued` cannot admit
/// an unclaimed in-progress row because that branch also checks the row status.
const EXPIRED_RECOVERY_EXPECTED_STATUS: &str = "queued";

/// Only statuses written by the drain under its claim are recoverable here:
/// an expired claim on `folding` or `verifying` means its owner died mid-drain.
/// `landed` is terminal, while `agent_running` may still have a live fixer
/// writing the worktree, so both retain the existing fail-closed warning.
fn recoverable_in_progress_status(status: &str) -> bool {
    matches!(
        thegn_core::attention::MqStatus::parse(status),
        Some(thegn_core::attention::MqStatus::Folding | thegn_core::attention::MqStatus::Verifying)
    )
}

fn drive_queue_with(
    cfg: &MergeQueueConfig,
    full: &Config,
    repo_root: &Path,
    db: &Db,
    items: Vec<QueueItem>,
    mut progress: impl FnMut(&DriveStep),
    mut actions: DriveActions<
        impl FnMut(&str, &thegn_core::remote::GitLoc, bool) -> anyhow::Result<AttemptOutcome>,
        impl FnMut(&str) -> crate::agent_run::AgentDispatch,
        impl FnMut(
            &str,
            &str,
            &str,
            &str,
            &Failure,
            Option<thegn_core::sandbox::SandboxSpec>,
        ) -> crate::agent_run::AgentRunVerdict,
    >,
) -> DriveOutcome {
    let mut out = DriveOutcome::default();
    // Resolved once per drain, not per branch: `agent_command` verbatim, else a
    // named `[[agents]]` entry's headless form, else nothing.
    let agent_cmd = thegn_core::agent_task::resolve_agent(full, &cfg.agent, &cfg.agent_command);
    let wants_agent = cfg.conflict_handoff == ConflictHandoff::Agent;
    if wants_agent && agent_cmd.is_none() && !cfg.agent.trim().is_empty() {
        // A named agent that matched no entry would otherwise degrade to notify
        // in silence, which reads as "nothing needed fixing".
        let msg = format!(
            "merge_queue.agent = {:?} matches no [[agents]]/[[tools]] entry; \
             conflicts will be deferred instead of fixed",
            cfg.agent
        );
        tracing::warn!(target: "thegn::merge", "{msg}");
        out.warnings.push(msg);
    }
    let use_agent = wants_agent && agent_cmd.is_some();
    let target = integrate::resolve_target(cfg, repo_root);

    let total_items = items.len();
    'items: for (index, item) in items.into_iter().enumerate() {
        if crate::fold_cancel::requested() {
            let untouched = total_items - index;
            out.warnings.push(format!(
                "merge drain cancelled; {untouched} queued row(s) left untouched"
            ));
            break 'items;
        }
        let fresh_rows = match db.list_merge_queue() {
            Ok(rows) => rows,
            Err(error) => {
                out.persistence_errors.push(format!(
                    "{}: could not refresh queue row: {error}",
                    item.worktree
                ));
                break 'items;
            }
        };
        let Some(fresh) = fresh_rows
            .into_iter()
            .find(|row| row.worktree == item.worktree)
        else {
            out.warnings.push(format!(
                "{}: queue row disappeared since listing; skipped",
                item.branch
            ));
            continue;
        };
        let registered_repo = match db.repo_root_for(&item.worktree) {
            Ok(repo) => repo.filter(|repo| !repo.is_empty()),
            Err(error) => {
                out.persistence_errors.push(format!(
                    "{}: could not check merge row registration: {error}",
                    item.worktree
                ));
                break 'items;
            }
        };
        let (claim_repo, unregistered) = match registered_repo.as_deref() {
            Some(repo) => (repo.to_owned(), false),
            None => match verify_fixer_checkout(repo_root, &item.worktree, &item.branch, None) {
                Ok(()) => (repo_root.to_string_lossy().into_owned(), true),
                Err(reason) => {
                    out.warnings.push(format!(
                        "{}: unregistered queue row is not attributable to this repository ({reason}); skipped",
                        item.branch
                    ));
                    continue;
                }
            },
        };
        if fresh.branch != item.branch || fresh.location != item.location {
            out.warnings.push(format!(
                "{}: not admitted (queue identity changed since listing); skipped",
                item.branch
            ));
            continue;
        }
        let recovering: bool = if !matches!(
            fresh.status.as_str(),
            "queued"
                | "deferred"
                | "gate_failed"
                | "gate_error"
                | "needs_human"
                | "ready"
                | "agent_blocked"
        ) {
            let active_claim = db.observe_merge_claim(&item.worktree);
            let now_ms = util::now_ms();
            let recoverable = match active_claim {
                Ok(Some(view))
                    if recoverable_in_progress_status(&fresh.status)
                        && expired_recoverable(&view, now_ms) =>
                {
                    true
                }
                Ok(Some(view))
                    if view.phase != "unclaimed"
                        && view.lease_until_ms.is_some_and(|until| until > now_ms) =>
                {
                    out.warnings.push(format!(
                        "{}: claimed by another drain (generation {}, until {}); skipped",
                        item.branch,
                        view.generation,
                        view.lease_until_ms.unwrap_or_default()
                    ));
                    false
                }
                Ok(_) => {
                    out.warnings.push(format!(
                        "{}: {} row is not claimable (stale in-progress state from an earlier drain?); remove and re-add it",
                        item.branch, fresh.status
                    ));
                    false
                }
                Err(error) => {
                    out.persistence_errors.push(format!(
                        "{}: could not observe merge claim: {error}",
                        item.worktree
                    ));
                    break 'items;
                }
            };
            if !recoverable {
                continue;
            }
            true
        } else {
            false
        };
        let expected_status = if recovering {
            EXPIRED_RECOVERY_EXPECTED_STATUS
        } else {
            fresh.status.as_str()
        };
        let admission = if unregistered {
            claim::RowClaim::admit_unregistered(
                db,
                &item,
                &claim_repo,
                expected_status,
                &fresh.target_branch,
            )
        } else {
            claim::RowClaim::admit(
                db,
                &item,
                &claim_repo,
                expected_status,
                &fresh.target_branch,
            )
        };
        let claim = match admission {
            Ok(claim::Admission::Claimed(claim)) => Some(claim),
            Ok(
                claim::Admission::HeldElsewhere(warning) | claim::Admission::NotAdmitted(warning),
            ) => {
                out.warnings.push(warning);
                continue;
            }
            Err(error) => {
                out.persistence_errors.push(format!(
                    "{}: could not claim merge row: {error}",
                    item.worktree
                ));
                break 'items;
            }
        };
        let claim = std::cell::RefCell::new(claim);
        let write_error = std::cell::RefCell::new(None::<String>);
        let post_release = std::cell::RefCell::new(None::<PostRelease>);
        let lifecycle_notes = std::cell::RefCell::new(Vec::<String>::new());
        let stop_drain = std::cell::Cell::new(false);
        let write_fields = |db: &Db, status: MqStatus, fields: &MergeStatusFields| {
            let result = if let Some(claim) = claim.borrow().as_ref() {
                claim.write(db, status, fields)
            } else {
                replace_status(db, &item.worktree, status, fields)
            };
            if let Err(error) = result {
                let message = if error
                    .to_string()
                    .contains(thegn_core::store::MERGE_CLAIM_LOST)
                {
                    format!(
                        "{}: merge claim lost; another drain recovered the row",
                        item.branch
                    )
                } else {
                    format!("{}: {error}", item.worktree)
                };
                *write_error.borrow_mut() = Some(message);
            }
        };
        let set = |db: &Db, status: MqStatus, oid: Option<&str>, detail: Option<&str>| {
            let fields = MergeStatusFields {
                result_oid: oid.map(str::to_owned),
                conflict_paths: None,
                error_detail: detail.map(str::to_owned),
            };
            write_fields(db, status, &fields);
            fields
        };
        macro_rules! stop_on_write_error {
            ($label:lifetime) => {
                let pending = write_error.borrow_mut().take();
                if let Some(error) = pending {
                    if error.contains("merge claim lost; another drain recovered the row") {
                        out.warnings.push(error);
                    } else {
                        tracing::error!(target: "thegn::merge", "queue state write failed: {error}");
                        out.persistence_errors.push(error);
                        stop_drain.set(true);
                    }
                    break $label;
                }
            };
        }
        // Sidebar-folder lifecycle: move the worktree on a settled transition
        // (landed ⇒ Merged/cleanup, failure ⇒ the failed folder). No-op unless
        // `[merge_queue] organize_folders` is on.
        let lifecycle = |db: &Db, event: thegn_core::merge_lifecycle::LifecycleEvent| {
            if claim.borrow().is_some() {
                *post_release.borrow_mut() = Some(PostRelease::Apply(event));
            } else {
                for note in crate::merge_lifecycle::apply_reported(
                    cfg,
                    db,
                    repo_root,
                    &item.worktree,
                    &item.branch,
                    event,
                ) {
                    lifecycle_notes.borrow_mut().push(note.text().to_owned());
                }
            }
        };
        let fields = set(db, MqStatus::Folding, None, None);
        let pending_write_error = write_error.borrow_mut().take();
        if let Some(error) = pending_write_error {
            let claim_lost = error.contains("merge claim lost; another drain recovered the row");
            if claim_lost {
                out.warnings.push(error);
            } else {
                out.persistence_errors.push(error);
            }
            if let Some(claim) = claim.borrow_mut().take()
                && let Err(error) = claim.finish(db)
                && !claim_lost
            {
                out.warnings.push(format!(
                    "{}: could not release merge claim: {error}",
                    item.branch
                ));
            }
            if claim_lost {
                continue 'items;
            }
            break 'items;
        }
        progress(&DriveStep {
            fields: &fields,
            worktree: &item.worktree,
            branch: &item.branch,
            status: MqStatus::Folding.as_str(),
            detail: "",
        });

        // Where the branch's worktree lives — drives the cross-host tip ingest
        // inside attempt_land (empty location = local = same store as target).
        let branch_loc = thegn_core::remote::GitLoc::from_db(
            &item.worktree,
            (!item.location.is_empty()).then_some(item.location.as_str()),
        );

        // Seeded from the persisted count, so the budget survives the drain that
        // spent it. `merge retry` (or a re-enqueue) resets it to 0.
        let mut agent_runs = fresh.agent_attempts;
        'attempt: loop {
            if claim.borrow().as_ref().is_some_and(claim::RowClaim::lost) {
                out.warnings.push(format!(
                    "{}: merge claim lost; another drain recovered the row",
                    item.branch
                ));
                break;
            }
            let held = matches!(
                fresh.status.as_str(),
                "agent_blocked" | "needs_human" | "gate_failed" | "gate_error" | "deferred"
            );
            let attempt = match (actions.attempt)(&item.branch, &branch_loc, held) {
                Ok(a) => a,
                Err(e) => {
                    if let Some(drift) = e.downcast_ref::<CandidateDrift>() {
                        let detail = format!(
                            "identity drift: {}; re-enqueue with `thegn merge add` to adopt the new tip",
                            drift.reason
                        );
                        let fields = set(db, MqStatus::AgentBlocked, None, Some(&detail));
                        stop_on_write_error!('attempt);
                        progress(&DriveStep {
                            fields: &fields,
                            worktree: &item.worktree,
                            branch: &item.branch,
                            status: MqStatus::AgentBlocked.as_str(),
                            detail: &detail,
                        });
                        out.agent_blocked.push(item.branch.clone());
                        break;
                    }
                    if let Some(unreadable) = e.downcast_ref::<CandidateUnreadable>() {
                        let detail = unreadable_detail(&unreadable.reason);
                        let fields = set(db, MqStatus::AgentBlocked, None, Some(&detail));
                        stop_on_write_error!('attempt);
                        progress(&DriveStep {
                            fields: &fields,
                            worktree: &item.worktree,
                            branch: &item.branch,
                            status: MqStatus::AgentBlocked.as_str(),
                            detail: &detail,
                        });
                        out.agent_blocked.push(item.branch.clone());
                        break;
                    }
                    let detail = format!("{e}");
                    let fields = set(db, MqStatus::NeedsHuman, None, Some(&detail));
                    stop_on_write_error!('attempt);
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::NeedsHuman.as_str(),
                        detail: &detail,
                    });
                    out.needs_human.push(item.branch.clone());
                    break;
                }
            };

            let failure = match attempt {
                AttemptOutcome::Landed {
                    commit,
                    old_oid,
                    resyncs,
                } => {
                    // Carried out to the caller so the CLI can warn about any
                    // live checkout of the target left stale by the ref move.
                    out.resyncs.extend(resyncs);
                    let fields = set(db, MqStatus::Landed, Some(&commit), None);
                    let pending_write_error = write_error.borrow_mut().take();
                    if let Some(error) = pending_write_error {
                        out.bookkeeping_failed.push(RefAdvancedBookkeepingFailed {
                            branch: item.branch.clone(),
                            target: target.clone(),
                            old_oid,
                            new_oid: commit,
                            error,
                        });
                        out.landed.push(item.branch.clone());
                        stop_drain.set(true);
                        break 'attempt;
                    }
                    if claim.borrow().is_some() {
                        *post_release.borrow_mut() = Some(PostRelease::Landed(commit.clone()));
                    } else {
                        for note in crate::merge_lifecycle::apply_landed_reported(
                            cfg,
                            db,
                            repo_root,
                            &item.worktree,
                            &item.branch,
                            &commit,
                        ) {
                            lifecycle_notes.borrow_mut().push(note.text().to_owned());
                        }
                    }
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::Landed.as_str(),
                        detail: &commit[..commit.len().min(12)],
                    });
                    out.landed.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::UpToDate { commit } => {
                    let fields = set(db, MqStatus::Landed, Some(&commit), Some("already merged"));
                    stop_on_write_error!('attempt);
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Landed);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::Landed.as_str(),
                        detail: "already merged",
                    });
                    out.landed.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::Ready { tip } => {
                    let fields = set(
                        db,
                        MqStatus::Ready,
                        Some(&tip),
                        Some("gated green — awaiting land"),
                    );
                    stop_on_write_error!('attempt);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::Ready.as_str(),
                        detail: "gated green — awaiting land",
                    });
                    out.ready.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::Unreachable { detail } => {
                    // Branch host unreachable / tip couldn't be fetched in. Hold
                    // (deferred) with the reason — a transient blip is retried on
                    // the next drain; never silently drop the row.
                    let fields = set(db, MqStatus::Deferred, None, Some(&detail));
                    stop_on_write_error!('attempt);
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::Deferred.as_str(),
                        detail: &detail,
                    });
                    out.deferred.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::GateError {
                    reason,
                    log,
                    phase,
                    class,
                } => {
                    // The gate could not RUN. This is a fact about the
                    // environment, not a verdict about the branch, so it must
                    // NOT become a `Failure`: handing it to the fixing agent
                    // would set a coding model loose on source code in response
                    // to `command not found`. Record it as its own state and
                    // stop; the row is retried on the next drain, by which time
                    // the environment may be fixed.
                    let detail = detail_with_log(&reason, &log);
                    let fields = set(db, MqStatus::GateError, None, Some(&detail));
                    stop_on_write_error!('attempt);
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::GateError.as_str(),
                        detail: &detail,
                    });
                    out.gate_error.push(item.branch.clone());
                    out.gate_verdicts.push(GateVerdictReport {
                        branch: item.branch.clone(),
                        verdict: "error".into(),
                        phase: phase.as_str().into(),
                        class: class.as_str().into(),
                        reason: if log.trim().is_empty() {
                            reason
                        } else {
                            format!("{reason}: {}", tail_line(&log))
                        },
                    });
                    break;
                }
                AttemptOutcome::Conflict {
                    paths,
                    submodule_conflicts,
                    driver_refusal,
                } => {
                    if let Some(reason) = driver_refusal {
                        let paths_text = conflict_detail(&paths, &submodule_conflicts);
                        let mut fields = conflict_fields(&paths, &submodule_conflicts);
                        fields.error_detail = Some(if paths_text.is_empty() {
                            reason.clone()
                        } else {
                            format!("{reason}\n{paths_text}")
                        });
                        write_fields(db, MqStatus::NeedsHuman, &fields);
                        let pending_write_error = write_error.borrow_mut().take();
                        if let Some(error) = pending_write_error {
                            if error.contains("merge claim lost; another drain recovered the row") {
                                out.warnings.push(error);
                                break 'attempt;
                            }
                            out.persistence_errors.push(format!(
                                "{}: could not persist driver refusal: {error}",
                                item.worktree
                            ));
                            stop_drain.set(true);
                            break 'attempt;
                        }
                        lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
                        progress(&DriveStep {
                            fields: &fields,
                            worktree: &item.worktree,
                            branch: &item.branch,
                            status: MqStatus::NeedsHuman.as_str(),
                            detail: &reason,
                        });
                        out.needs_human.push(item.branch.clone());
                        break;
                    }
                    Failure::Conflict {
                        paths,
                        submodule_conflicts,
                    }
                }
                AttemptOutcome::GateFailed { log } => {
                    out.gate_verdicts.push(GateVerdictReport {
                        branch: item.branch.clone(),
                        verdict: "failed".into(),
                        phase: "gate".into(),
                        class: "red".into(),
                        reason: if log.trim().is_empty() {
                            "gate_command failed".into()
                        } else {
                            format!("gate_command failed: {}", tail_line(&log))
                        },
                    });
                    Failure::Gate(log)
                }
            };

            // A land failure. Dispatch the agent to fix it, if we still can.
            if use_agent && agent_runs < cfg.agent_max_attempts {
                // The queue stores remote worktree identifiers as opaque host
                // identities. Until a remote executor exists, never reinterpret
                // one as a target-host path for sandbox resolution or `current_dir`.
                let location = item.location.trim();
                if (!location.is_empty() && location != "local") || branch_loc.is_remote() {
                    let reason = format!(
                        "cross-host fixer unsupported: {} lives on a remote host; resolve it there or `merge retry` after moving it",
                        item.branch
                    );
                    thegn_core::msg::warn(&reason);
                    let fields = set(db, MqStatus::AgentBlocked, None, Some(&reason));
                    stop_on_write_error!('attempt);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::AgentBlocked.as_str(),
                        detail: &reason,
                    });
                    out.agent_blocked.push(item.branch.clone());
                    break;
                }
                // Enforce the in-force isolation floor before the handoff.
                // Gate BEFORE consuming an attempt: a fail-closed miss, or an
                // unbuildable sandbox under a demanded floor, is an INFRASTRUCTURE
                // failure — hold the entry and NEVER blame the branch.
                let dispatch = (actions.floor)(&item.worktree);
                let sandbox = match dispatch {
                    crate::agent_run::AgentDispatch::InfraHold(reason) => {
                        thegn_core::msg::warn(&reason);
                        let fields = set(db, MqStatus::AgentBlocked, None, Some(&reason));
                        stop_on_write_error!('attempt);
                        progress(&DriveStep {
                            fields: &fields,
                            worktree: &item.worktree,
                            branch: &item.branch,
                            status: MqStatus::AgentBlocked.as_str(),
                            detail: &reason,
                        });
                        // Held for a later drain; NOT a branch/gate failure.
                        out.agent_blocked.push(item.branch.clone());
                        break;
                    }
                    crate::agent_run::AgentDispatch::RunDegraded(spec, warning) => {
                        thegn_core::msg::warn(&warning);
                        spec
                    }
                    crate::agent_run::AgentDispatch::Run(spec) => spec,
                };
                let budget_exhausted = if let Some(claim) = claim.borrow().as_ref() {
                    if claim.lost() {
                        out.warnings.push(format!(
                            "{}: merge claim lost; another drain recovered the row",
                            item.branch
                        ));
                        break;
                    }
                    match claim.consume_attempt(db, cfg.agent_max_attempts) {
                        Ok(thegn_core::store::MergeAttemptResult::Consumed) => {
                            agent_runs += 1;
                            false
                        }
                        Ok(thegn_core::store::MergeAttemptResult::BudgetExhausted) => {
                            agent_runs = cfg.agent_max_attempts;
                            true
                        }
                        Ok(thegn_core::store::MergeAttemptResult::ClaimLost) => {
                            out.warnings.push(format!(
                                "{}: merge claim lost; another drain recovered the row",
                                item.branch
                            ));
                            break;
                        }
                        Err(error) => {
                            if error
                                .to_string()
                                .contains(thegn_core::store::MERGE_CLAIM_LOST)
                            {
                                out.warnings.push(format!(
                                    "{}: merge claim lost; another drain recovered the row",
                                    item.branch
                                ));
                                break 'attempt;
                            }
                            let detail = format!("could not reserve agent attempt: {error}");
                            out.persistence_errors
                                .push(format!("{}: {detail}", item.worktree));
                            set(db, MqStatus::GateError, None, Some(&detail));
                            stop_on_write_error!('attempt);
                            stop_drain.set(true);
                            break 'attempt;
                        }
                    }
                } else {
                    agent_runs += 1;
                    if let Err(error) = db.set_merge_agent_attempts(&item.worktree, agent_runs) {
                        let detail = format!("could not reserve agent attempt: {error}");
                        out.persistence_errors
                            .push(format!("{}: {detail}", item.worktree));
                        set(db, MqStatus::GateError, None, Some(&detail));
                        stop_on_write_error!('attempt);
                        stop_drain.set(true);
                        break 'attempt;
                    }
                    false
                };
                if !budget_exhausted {
                    let note = format!("agent fixing ({agent_runs}/{})", cfg.agent_max_attempts);
                    let fields = set(db, MqStatus::AgentRunning, None, Some(&note));
                    stop_on_write_error!('attempt);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: MqStatus::AgentRunning.as_str(),
                        detail: &note,
                    });
                    // Run to completion; the re-attempt (top of loop) is the real
                    // arbiter of whether the fix worked for agents that ran.
                    if let Some(template) = agent_cmd.as_deref() {
                        if claim.borrow().as_ref().is_some_and(claim::RowClaim::lost) {
                            out.warnings.push(format!(
                                "{}: merge claim lost; another drain recovered the row",
                                item.branch
                            ));
                            break 'attempt;
                        }
                        let verdict = (actions.runner)(
                            template,
                            &item.worktree,
                            &item.branch,
                            &target,
                            &failure,
                            sandbox,
                        );
                        if let Some(reason) = verdict.not_started() {
                            let mut refund_failed = false;
                            if let Some(row_claim) = claim.borrow().as_ref() {
                                match row_claim.refund_attempt(db) {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        out.warnings.push(format!(
                                            "{}: merge claim lost; another drain recovered the row",
                                            item.branch
                                        ));
                                        break 'attempt;
                                    }
                                    Err(error)
                                        if error
                                            .to_string()
                                            .contains(thegn_core::store::MERGE_CLAIM_LOST) =>
                                    {
                                        out.warnings.push(format!(
                                            "{}: merge claim lost; another drain recovered the row",
                                            item.branch
                                        ));
                                        break 'attempt;
                                    }
                                    Err(error) => {
                                        refund_failed = true;
                                        out.persistence_errors.push(format!(
                                            "{}: could not refund agent attempt: {error}",
                                            item.worktree
                                        ));
                                    }
                                }
                            } else {
                                let refunded_runs = agent_runs.saturating_sub(1);
                                if let Err(error) =
                                    db.set_merge_agent_attempts(&item.worktree, refunded_runs)
                                {
                                    out.persistence_errors.push(format!(
                                        "{}: could not refund agent attempt: {error}",
                                        item.worktree
                                    ));
                                    refund_failed = true;
                                }
                            }
                            let detail = agent_not_started_detail(reason, refund_failed);
                            thegn_core::msg::warn(&detail);
                            let fields = set(db, MqStatus::AgentBlocked, None, Some(&detail));
                            stop_on_write_error!('attempt);
                            progress(&DriveStep {
                                fields: &fields,
                                worktree: &item.worktree,
                                branch: &item.branch,
                                status: MqStatus::AgentBlocked.as_str(),
                                detail: &detail,
                            });
                            out.agent_blocked.push(item.branch.clone());
                            break;
                        }
                    }
                    continue;
                }
            }

            // Out of attempts (or agent handoff disabled) — record the terminal
            // state. A branch we tried to fix and couldn't is `needs_human`;
            // one we never tried keeps the classic deferred/gate_failed status.
            match failure {
                Failure::Conflict {
                    paths,
                    submodule_conflicts,
                } => {
                    let detail = conflict_detail(&paths, &submodule_conflicts);
                    let status = if agent_runs > 0 {
                        MqStatus::NeedsHuman
                    } else {
                        MqStatus::Deferred
                    };
                    let fields = conflict_fields(&paths, &submodule_conflicts);
                    write_fields(db, status, &fields);
                    let pending_write_error = write_error.borrow_mut().take();
                    if let Some(error) = pending_write_error {
                        if error.contains("merge claim lost; another drain recovered the row") {
                            out.warnings.push(error);
                            break 'attempt;
                        }
                        let detail = format!("could not persist conflict outcome: {error}");
                        out.persistence_errors
                            .push(format!("{}: {detail}", item.worktree));
                        set(db, MqStatus::GateError, None, Some(&detail));
                        stop_on_write_error!('attempt);
                        stop_drain.set(true);
                        break 'attempt;
                    }
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: status.as_str(),
                        detail: &detail,
                    });
                    if agent_runs > 0 {
                        out.needs_human.push(item.branch.clone());
                    } else {
                        out.deferred.push(item.branch.clone());
                    }
                }
                Failure::Gate(log) => {
                    let status = if agent_runs > 0 {
                        MqStatus::NeedsHuman
                    } else {
                        MqStatus::GateFailed
                    };
                    // Persist the actual gate output, not a fixed two-word
                    // string: "breaks build" told the user nothing about WHY,
                    // and the log was otherwise discarded entirely.
                    let detail = detail_with_log("breaks build", &log);
                    let fields = set(db, status, None, Some(&detail));
                    stop_on_write_error!('attempt);
                    progress(&DriveStep {
                        fields: &fields,
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: status.as_str(),
                        detail: &tail_line(&log),
                    });
                    if agent_runs > 0 {
                        out.needs_human.push(item.branch.clone());
                    } else {
                        out.deferred.push(item.branch.clone());
                    }
                }
            }
            lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
            break;
        }
        if let Some(claim) = claim.borrow_mut().take() {
            let claim_lost = claim.lost();
            match claim.finish(db) {
                Ok(()) => match post_release.borrow_mut().take() {
                    Some(PostRelease::Apply(event)) => {
                        for note in crate::merge_lifecycle::apply_reported(
                            cfg,
                            db,
                            repo_root,
                            &item.worktree,
                            &item.branch,
                            event,
                        ) {
                            lifecycle_notes.borrow_mut().push(note.text().to_owned());
                        }
                    }
                    Some(PostRelease::Landed(commit)) => {
                        for note in crate::merge_lifecycle::apply_landed_reported(
                            cfg,
                            db,
                            repo_root,
                            &item.worktree,
                            &item.branch,
                            &commit,
                        ) {
                            lifecycle_notes.borrow_mut().push(note.text().to_owned());
                        }
                    }
                    None => {}
                },
                Err(error) if !claim_lost => out.warnings.push(format!(
                    "{}: could not settle and release merge claim: {error}",
                    item.branch
                )),
                Err(_) => {
                    if !out.warnings.iter().any(|warning| {
                        warning.starts_with(&item.branch)
                            && warning.contains("merge claim lost; another drain recovered the row")
                    }) {
                        out.warnings.push(format!(
                            "{}: merge claim lost; another drain recovered the row",
                            item.branch
                        ));
                    }
                }
            }
        }
        out.warnings.extend(lifecycle_notes.into_inner());
        if stop_drain.get() {
            break 'items;
        }
    }
    out
}

/// Build the prompt for a failure and run the configured agent in the branch's
/// worktree. The prompt template comes from `[merge_queue.prompts]`, defaulting
/// to thegn's built-in instructions for the kind; both the template engine and
/// the process mechanics are shared with every other queue that dispatches an
/// agent (`thegn_core::agent_task` + `crate::agent_run`).
///
/// Unconditional: `agent_run::run` carries its own Windows stub, so this must
/// not be cfg-gated — gating it left `drive_queue`'s dispatch arm calling a
/// function that did not exist on the Windows target.
fn agent_not_started_detail(reason: &str, refund_failed: bool) -> String {
    if refund_failed {
        format!("agent did not start: {reason}; held (attempt refund failed)")
    } else {
        format!("agent did not start: {reason}; held, no attempt spent")
    }
}

fn run_agent(
    cfg: &MergeQueueConfig,
    command_template: &str,
    worktree: &str,
    branch: &str,
    target: &str,
    failure: &Failure,
    sandbox: Option<thegn_core::sandbox::SandboxSpec>,
) -> crate::agent_run::AgentRunVerdict {
    let Some((kind, vars, prompt)) = compose(cfg, worktree, branch, target, failure) else {
        return crate::agent_run::AgentRunVerdict::NotStarted(
            "could not compose the fixer prompt".into(),
        );
    };
    crate::agent_run::run_outcome(&crate::agent_run::AgentTaskRun {
        kind,
        worktree,
        prompt: &prompt,
        command_template,
        vars: &vars,
        timeout_secs: cfg.agent_timeout_secs,
        sandbox,
        credential_free: true,
    })
}

/// Map a [`Failure`] to its task kind, template variables, and rendered prompt.
///
/// Split out of [`run_agent`] so the merge queue's own prompt path stays
/// unit-testable now that rendering itself lives in `thegn_core::agent_task` —
/// what needs covering here is the *mapping* (a conflict fills `{paths}`, a red
/// gate fills `{log}`), not the substitution.
fn compose(
    cfg: &MergeQueueConfig,
    worktree: &str,
    branch: &str,
    target: &str,
    failure: &Failure,
) -> Option<(
    thegn_core::agent_task::TaskKind,
    thegn_core::agent_task::TaskVars,
    String,
)> {
    use thegn_core::agent_task::{TaskKind, TaskVars, format_paths, render_kind_prompt};

    let (kind, vars) = match failure {
        Failure::Conflict {
            paths,
            submodule_conflicts,
        } => (
            TaskKind::MergeConflict,
            TaskVars::new().set("paths", format_paths(paths)).set(
                "submodule_conflicts",
                thegn_core::agent_task::format_submodule_conflicts(submodule_conflicts),
            ),
        ),
        Failure::Gate(log) => (
            TaskKind::GateFailure,
            TaskVars::new().set("log", tail_line(log)),
        ),
    };
    let vars = vars
        .set("branch", branch)
        .set("target", target)
        .set("worktree", worktree);

    match render_kind_prompt(kind, cfg.prompts.resolve(kind), &vars) {
        Ok(prompt) => Some((kind, vars, prompt)),
        Err(e) => {
            // `config validate` catches this ahead of time; if a bad template
            // still reaches here, say so rather than sending the agent garbage.
            tracing::warn!(
                target: "thegn::merge",
                kind = %kind,
                error = %e,
                "merge queue: prompt template is invalid; not dispatching"
            );
            None
        }
    }
}

/// A queue row's `error_detail`: a short headline plus the tail of the gate log.
///
/// The log used to be discarded at this boundary — the row kept only the fixed
/// string "breaks build", so neither `merge list` nor the panel could ever say
/// what actually went wrong. Bounded so a runaway gate can't bloat the row.
fn detail_with_log(headline: &str, log: &str) -> String {
    let log = log.trim();
    if log.is_empty() {
        return headline.to_string();
    }
    let mut cut = log.len().saturating_sub(MAX_LOG_LINE_BYTES);
    while cut < log.len() && !log.is_char_boundary(cut) {
        cut += 1;
    }
    let body = if cut > 0 { &log[cut..] } else { log };
    format!("{headline}\n{body}")
}

const MAX_LOG_LINE_BYTES: usize = 2000;

/// The last non-empty line of a log (for a one-line status detail), falling back
/// to a headline when the command produced no output at all — a bare `exit 1`
/// gate would otherwise render as "needs a human — " with nothing after it.
fn tail_line(log: &str) -> String {
    let line = log
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("breaks build (gate exited non-zero, no output)");
    thegn_core::agent_task::bound_text(line, MAX_LOG_LINE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refund_failure_detail_does_not_claim_the_attempt_was_refunded() {
        assert_eq!(
            agent_not_started_detail("spawn denied", true),
            "agent did not start: spawn denied; held (attempt refund failed)"
        );
        assert_eq!(
            agent_not_started_detail("spawn denied", false),
            "agent did not start: spawn denied; held, no attempt spent"
        );
    }

    #[test]
    fn fixer_registration_ignores_a_newline_forged_record() {
        let oid = "a".repeat(40);
        let listing = format!(
            "worktree /r\0HEAD {oid}\0branch refs/heads/main\0\0\
             worktree /tmp/a\nworktree /tmp/target\0HEAD {oid}\0detached\0\0"
        );
        assert_eq!(
            listing_registers(&listing, Path::new("/tmp/target")),
            Ok(false)
        );
        assert_eq!(
            listing_registers(&listing, Path::new("/tmp/a\nworktree /tmp/target")),
            Ok(true)
        );
    }

    #[test]
    fn fixer_registration_refuses_a_malformed_listing() {
        let error = listing_registers(
            "worktree /r\0HEAD 0000000000000000000000000000000000000000\0detached\0",
            Path::new("/r"),
        )
        .unwrap_err();
        assert!(error.contains("malformed"));
    }

    fn uncontained_config() -> Config {
        Config {
            sandbox: thegn_core::config::SandboxConfig {
                isolation_floor: Some(thegn_core::config::IsolationFloor::Off),
                ..thegn_core::config::SandboxConfig::default()
            },
            ..Config::default()
        }
    }

    #[test]
    fn candidate_drift_table() {
        let candidate = MergeCandidate {
            branch_ref: "refs/heads/feat".into(),
            candidate_oid: "a".repeat(40),
        };
        let moved = "b".repeat(40);
        let cases = [
            (None, None, Some(moved.as_str()), false, false, None),
            (
                Some(&candidate),
                None,
                Some(candidate.candidate_oid.as_str()),
                false,
                false,
                None,
            ),
            (
                Some(&candidate),
                None,
                Some(moved.as_str()),
                false,
                false,
                Some("branch moved since enqueue (expected aaaaaaaaaaaa, found bbbbbbbbbbbb)"),
            ),
            (
                Some(&candidate),
                None,
                None,
                false,
                false,
                Some("branch deleted since enqueue"),
            ),
            (
                Some(&candidate),
                Some(moved.as_str()),
                Some(moved.as_str()),
                false,
                false,
                None,
            ),
            (
                Some(&candidate),
                Some(moved.as_str()),
                Some(candidate.candidate_oid.as_str()),
                false,
                false,
                Some("branch moved since enqueue (expected bbbbbbbbbbbb, found aaaaaaaaaaaa)"),
            ),
        ];
        for (candidate, adopted, live, held, fast_forward, expected) in cases {
            assert_eq!(
                candidate_drift(candidate, adopted, live, held, fast_forward)
                    .err()
                    .as_deref(),
                expected
            );
        }
    }

    #[test]
    fn enqueue_identity_drift_table() {
        let recorded = MergeEnqueueIdentity {
            common_dir: "/private/repo/.git".into(),
            admin_dir: "/private/repo/.git/worktrees/feature".into(),
        };
        let same = recorded.clone();
        let other_repo = MergeEnqueueIdentity {
            common_dir: "/private/other/.git".into(),
            admin_dir: "/private/other/.git/worktrees/feature".into(),
        };
        let new_registration = MergeEnqueueIdentity {
            admin_dir: "/private/repo/.git/worktrees/feature-2".into(),
            ..recorded.clone()
        };
        let cases = [
            (
                Err("gone"),
                Ok("/private/repo/.git"),
                Some("worktree identity unavailable: gone"),
            ),
            (
                Ok(&same),
                Err("missing"),
                Some("drain repository identity unavailable: missing"),
            ),
            (
                Ok(&same),
                Ok("/private/other/.git"),
                Some("queued for a different repository than this drain"),
            ),
            (
                Ok(&other_repo),
                Ok("/private/repo/.git"),
                Some("worktree now belongs to a different repository than at enqueue"),
            ),
            (
                Ok(&new_registration),
                Ok("/private/repo/.git"),
                Some("worktree registration changed since enqueue (removed and re-created?)"),
            ),
            (Ok(&same), Ok("/private/repo/.git"), None),
        ];
        for (live, drain, expected) in cases {
            assert_eq!(
                enqueue_identity_drift(&recorded, live, drain)
                    .err()
                    .as_deref(),
                expected
            );
        }
    }

    #[test]
    fn candidate_drift_held_fast_forward_is_adoptable_but_rewrite_is_held() {
        let candidate = MergeCandidate {
            branch_ref: "refs/heads/feat".into(),
            candidate_oid: "a".repeat(40),
        };
        let repaired = "b".repeat(40);
        assert!(candidate_drift(Some(&candidate), None, Some(&repaired), true, true,).is_ok());
        assert!(
            candidate_drift(Some(&candidate), None, Some(&repaired), true, false,)
                .unwrap_err()
                .contains("branch moved since enqueue")
        );
    }

    // Substitution and template rendering themselves are covered in
    // `thegn_core::agent_task`; what these cover is the merge queue's own
    // mapping from a `Failure` onto a task kind and its variables.

    #[test]
    fn conflict_prompt_lists_paths_and_rules() {
        let cfg = MergeQueueConfig::default();
        let (kind, vars, p) = compose(
            &cfg,
            "/w/x",
            "feat-x",
            "main",
            &Failure::Conflict {
                paths: vec!["src/a.rs".into(), "src/b.rs".into()],
                submodule_conflicts: Vec::new(),
            },
        )
        .expect("built-in template renders");
        assert_eq!(kind, thegn_core::agent_task::TaskKind::MergeConflict);
        assert_eq!(vars.get("worktree"), Some("/w/x"));
        assert!(p.contains("feat-x") && p.contains("main"));
        assert!(p.contains("src/a.rs") && p.contains("src/b.rs"));
        assert!(p.contains("Do NOT push"));
        assert!(p.contains("Rebase"));
    }

    #[test]
    fn gate_prompt_includes_log_tail_and_rules() {
        let cfg = MergeQueueConfig::default();
        let (kind, _, p) = compose(
            &cfg,
            "/w/x",
            "feat-x",
            "main",
            &Failure::Gate("error: boom\n".into()),
        )
        .expect("built-in template renders");
        assert_eq!(kind, thegn_core::agent_task::TaskKind::GateFailure);
        assert!(p.contains("fails the test gate"));
        assert!(p.contains("boom"));
        assert!(p.contains("Do NOT push"));
    }

    #[test]
    fn a_configured_prompt_template_replaces_the_builtin() {
        let mut cfg = MergeQueueConfig::default();
        cfg.prompts.conflict = "fix {branch} vs {target}, see:\n{paths}".into();
        let (_, _, p) = compose(
            &cfg,
            "/w/x",
            "feat-x",
            "main",
            &Failure::Conflict {
                paths: vec!["src/a.rs".into()],
                submodule_conflicts: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(p, "fix feat-x vs main, see:\n  - src/a.rs\n");
        // The built-in rules are gone precisely because the user replaced them.
        assert!(!p.contains("Do NOT push"));
    }

    #[test]
    fn conflict_prompt_receives_typed_submodule_details() {
        let mut cfg = MergeQueueConfig::default();
        cfg.prompts.conflict = "{submodule_conflicts}".into();
        let (_, vars, prompt) = compose(
            &cfg,
            "/w/x",
            "feat-x",
            "main",
            &Failure::Conflict {
                paths: vec!["vendor/lib".into()],
                submodule_conflicts: vec![thegn_core::submodule::SubmoduleConflict {
                    path: "vendor/lib".into(),
                    ours_sha: "abc1234".into(),
                    theirs_sha: "def5678".into(),
                }],
            },
        )
        .unwrap();
        let expected = "submodule pointer conflict: vendor/lib (abc1234 vs def5678)";
        assert_eq!(vars.get("submodule_conflicts"), Some(expected));
        assert_eq!(prompt, expected);
    }

    #[test]
    fn an_unset_prompt_template_falls_back_to_the_builtin() {
        let mut cfg = MergeQueueConfig::default();
        // Whitespace-only counts as unset, so blanking a key in config.toml
        // restores the default rather than sending an empty prompt.
        cfg.prompts.gate_failure = "   \n ".into();
        let (_, _, p) = compose(&cfg, "/w/x", "b", "main", &Failure::Gate("boom".into())).unwrap();
        assert!(p.contains("Do NOT push"));
    }

    #[test]
    fn a_broken_prompt_template_refuses_to_dispatch() {
        let mut cfg = MergeQueueConfig::default();
        cfg.prompts.conflict = "fix {branchh}".into();
        assert!(
            compose(
                &cfg,
                "/w/x",
                "b",
                "main",
                &Failure::Conflict {
                    paths: Vec::new(),
                    submodule_conflicts: Vec::new(),
                }
            )
            .is_none(),
            "a typo'd placeholder must not reach the agent as a blank"
        );
    }

    #[test]
    fn tail_line_picks_last_nonempty() {
        assert_eq!(tail_line("a\nb\n\n"), "b");
        // A gate that fails silently (a bare `exit 1`) still needs SOMETHING to
        // show, or the status line reads "needs a human — " and stops.
        assert!(tail_line("").contains("no output"));
        assert!(tail_line("   \n\n").contains("no output"));
    }

    #[test]
    fn gate_prompt_bounds_a_huge_single_line() {
        let log = "x".repeat(300_000);
        let cfg = MergeQueueConfig::default();
        let (kind, _, prompt) = compose(
            &cfg,
            "/work/feature",
            "feature",
            "main",
            &Failure::Gate(log),
        )
        .unwrap();
        assert_eq!(kind, thegn_core::agent_task::TaskKind::GateFailure);
        assert!(prompt.len() <= thegn_core::agent_task::MAX_RENDERED_PROMPT_BYTES);
        assert!(prompt.contains("task data omitted"));
    }

    #[test]
    fn conflict_prompt_caps_many_paths() {
        let paths: Vec<String> = (0..10_000).map(|i| format!("path-{i}")).collect();
        let cfg = MergeQueueConfig::default();
        let (kind, vars, prompt) = compose(
            &cfg,
            "/work/feature",
            "feature",
            "main",
            &Failure::Conflict {
                paths,
                submodule_conflicts: vec![],
            },
        )
        .unwrap();
        assert_eq!(kind, thegn_core::agent_task::TaskKind::MergeConflict);
        assert!(vars.get("paths").unwrap().contains("more path(s) omitted"));
        assert!(prompt.len() <= thegn_core::agent_task::MAX_RENDERED_PROMPT_BYTES);
    }

    #[test]
    fn detail_with_log_keeps_the_headline_and_bounds_the_log() {
        // No log: headline alone, no stray separator.
        assert_eq!(detail_with_log("breaks build", ""), "breaks build");
        assert_eq!(detail_with_log("breaks build", "   \n "), "breaks build");
        // With a log: headline first (that is what one-line renderers show),
        // the log after.
        let d = detail_with_log("breaks build", "line1\nline2");
        assert_eq!(d.lines().next(), Some("breaks build"));
        assert!(d.contains("line2"));
        // Bounded, and still valid UTF-8 at the cut.
        let huge = "é".repeat(4000);
        let d = detail_with_log("boom", &huge);
        assert!(d.len() < 2200, "len {}", d.len());
        assert_eq!(d.lines().next(), Some("boom"));
    }

    // ── End-to-end drive with a fake headless agent (real git) ────────────────
    #[cfg(unix)]
    mod e2e {
        use super::*;
        use std::path::{Path, PathBuf};

        /// Own every fixture path and keep environment restoration alive until
        /// synchronous driver/agent work completes, including assertion failure.
        struct Fixture {
            _env: crate::testenv::EnvVarGuard,
            _root: tempfile::TempDir,
        }

        impl Fixture {
            fn new(tag: &str) -> (Self, PathBuf, PathBuf) {
                let private = tempfile::Builder::new()
                    .prefix(&format!("tg-drive-{tag}-"))
                    .tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
                    .unwrap();
                let base = private.path();
                for name in [
                    "config", "state", "cache", "thegn", "tmp", "appdata", "template", "repo",
                ] {
                    std::fs::create_dir(base.join(name)).unwrap();
                }
                let config = base.join("config");
                let state = base.join("state");
                let cache = base.join("cache");
                let thegn = base.join("thegn");
                let temporary = base.join("tmp");
                let appdata = base.join("appdata");
                let template = base.join("template");
                let global = base.join("gitconfig");
                std::fs::write(&global, "").unwrap();
                // Keep the authored fixture scripts but do not source the user's
                // login startup files. The product's shell dispatch is unchanged.
                let shell = base.join("fixture-shell");
                crate::platform::publish_private_executable(
                    &base.join("fixture-shell.staged"),
                    &shell,
                    b"#!/bin/sh\n[ \"$#\" -eq 2 ] || exit 97\ncase \"$1\" in -lc|-c) ;; *) exit 97 ;; esac\nexec /bin/sh -c \"$2\"\n",
                ).unwrap();
                let env = crate::testenv::EnvVarGuard::set(&[
                    ("XDG_CONFIG_HOME", config.to_str().unwrap()),
                    ("XDG_STATE_HOME", state.to_str().unwrap()),
                    ("XDG_CACHE_HOME", cache.to_str().unwrap()),
                    ("THEGN_DIR", thegn.to_str().unwrap()),
                    ("THEGN_PROFILE", ""),
                    ("TMPDIR", temporary.to_str().unwrap()),
                    ("TMP", temporary.to_str().unwrap()),
                    ("TEMP", temporary.to_str().unwrap()),
                    ("APPDATA", appdata.to_str().unwrap()),
                    ("LOCALAPPDATA", appdata.to_str().unwrap()),
                    ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
                    ("GIT_CONFIG_NOSYSTEM", "1"),
                    ("GIT_CONFIG_COUNT", "0"),
                    ("GIT_CONFIG_PARAMETERS", ""),
                    ("GIT_TEMPLATE_DIR", template.to_str().unwrap()),
                    ("SHELL", shell.to_str().unwrap()),
                    ("ENV", ""),
                    ("BASH_ENV", ""),
                ]);
                // CanonicalHistory uses its own on-disk connection, not the
                // driver's in-memory queue cache. Bootstrap it in this fixture.
                drop(Db::open().expect("initialize private canonical-history registry"));
                let repo = base.join("repo");
                let feature = base.join("feature");
                (
                    Self {
                        _env: env,
                        _root: private,
                    },
                    repo,
                    feature,
                )
            }
        }

        #[expect(clippy::disallowed_methods)]
        fn git(dir: &Path, args: &[&str]) {
            let ok = util::git_cmd(dir)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
        }
        #[expect(clippy::disallowed_methods)]
        fn out(dir: &Path, args: &[&str]) -> String {
            String::from_utf8_lossy(&util::git_cmd(dir).args(args).output().unwrap().stdout)
                .trim()
                .to_string()
        }

        /// A repo on `main` with a linked worktree holding branch `feat` whose one
        /// commit conflicts with `main` on `base.txt`. Retain the owner through use.
        fn conflicting_repo(tag: &str) -> (Fixture, PathBuf, PathBuf) {
            let (fixture, root, feat_wt) = Fixture::new(tag);
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.name", "t"]);
            git(&root, &["config", "user.email", "t@e"]);
            git(&root, &["config", "commit.gpgsign", "false"]);
            std::fs::write(root.join("base.txt"), "base\n").unwrap();
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "-m", "c0"]);
            // feat in a linked worktree, diverging base.txt.
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "feat",
                    feat_wt.to_str().unwrap(),
                    "main",
                ],
            );
            std::fs::write(feat_wt.join("base.txt"), "feat\n").unwrap();
            git(&feat_wt, &["add", "-A"]);
            git(&feat_wt, &["commit", "-q", "-m", "feat edits base"]);
            // main diverges the same file → feat now conflicts with main.
            std::fs::write(root.join("base.txt"), "mainline\n").unwrap();
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "-m", "main edits base"]);
            (fixture, root, feat_wt)
        }

        #[test]
        fn fixer_checkout_verification_rejects_identity_drift() {
            let (_fixture, root, feat_wt) = conflicting_repo("identity-check");
            let feat_oid = out(&feat_wt, &["rev-parse", "HEAD"]);
            verify_fixer_checkout(&root, feat_wt.to_str().unwrap(), "feat", Some(&feat_oid))
                .unwrap();
            assert_eq!(
                verify_fixer_checkout_for_dispatch(&root, feat_wt.to_str().unwrap(), "feat", None)
                    .unwrap_err(),
                "queued candidate OID unavailable"
            );

            let main_oid = out(&root, &["rev-parse", "HEAD"]);
            assert!(
                verify_fixer_checkout(&root, feat_wt.to_str().unwrap(), "feat", Some(&main_oid))
                    .is_err()
            );

            git(&feat_wt, &["switch", "-q", "-c", "other"]);
            assert!(verify_fixer_checkout(&root, feat_wt.to_str().unwrap(), "feat", None).is_err());
            git(&feat_wt, &["switch", "-q", "feat"]);
            git(&feat_wt, &["checkout", "-q", "--detach"]);
            assert!(verify_fixer_checkout(&root, feat_wt.to_str().unwrap(), "feat", None).is_err());

            let alias = feat_wt.with_file_name("feature-alias");
            std::os::unix::fs::symlink(&feat_wt, &alias).unwrap();
            assert!(verify_fixer_checkout(&root, alias.to_str().unwrap(), "feat", None).is_err());
        }

        #[test]
        fn fixer_registration_accepts_a_registered_newline_worktree() {
            let (_fixture, root, _feat_wt) = conflicting_repo("nl-reg");
            let nl = root.parent().unwrap().join("nl\nwt");
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "nlb",
                    nl.to_str().unwrap(),
                    "main",
                ],
            );
            verify_fixer_checkout(&root, nl.to_str().unwrap(), "nlb", None).unwrap();
        }

        #[test]
        fn switched_queued_worktree_holds_without_dispatch_or_attempt_spend() {
            let (_fixture, root, feat_wt) = conflicting_repo("identity-hold");
            let sentinel = root.join("wrong-checkout-was-written");
            let db = Db::open_memory().unwrap();
            db.put_worktree(
                "feat",
                root.to_str().unwrap(),
                feat_wt.to_str().unwrap(),
                "feat",
                None,
                None,
            )
            .unwrap();
            let queued = item(&db, &feat_wt);
            git(&feat_wt, &["switch", "-q", "-c", "other"]);

            let outcome = drive_queue(
                &cfg(&format!("touch {} && {FIXER}", sentinel.display()), 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );

            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert!(
                !sentinel.exists(),
                "identity drift must not launch the fixer"
            );
            assert_eq!(db.list_merge_queue().unwrap()[0].agent_attempts, 0);
        }

        #[test]
        fn unsupported_platform_holds_before_merge_agent_attempt_or_spawn() {
            let (_fixture, root, feat_wt) = conflicting_repo("unsupported-agent");
            let sentinel = root.join("unsupported-agent-was-run");
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let unsupported = crate::agent_run::force_headless_unsupported();
            let outcome = drive_queue(
                &cfg(&format!("touch {} && {FIXER}", sentinel.display()), 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            drop(unsupported);

            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert!(!sentinel.exists());
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.agent_attempts, 0);
            assert!(row.error_detail.unwrap().contains("THE-235"));
        }

        #[test]
        fn missing_queued_branch_is_a_gate_error_not_a_branch_failure() {
            let (_fixture, root, feat_wt) = conflicting_repo("missing-branch");
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            git(&root, &["update-ref", "-d", "refs/heads/feat"]);

            let outcome = drive_queue(
                &cfg(FIXER, 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );

            assert_eq!(outcome.gate_error, ["feat"]);
            assert_eq!(outcome.gate_verdicts.len(), 1);
            assert_eq!(outcome.gate_verdicts[0].branch, "feat");
            assert_eq!(outcome.gate_verdicts[0].verdict, "error");
            assert!(!outcome.gate_verdicts[0].class.is_empty());
            assert!(outcome.needs_human.is_empty());
            assert_eq!(db.list_merge_queue().unwrap()[0].agent_attempts, 0);
        }

        /// A cleanly foldable linked branch, used to reach commit signing
        /// without first taking the conflict/agent path.
        fn clean_repo(tag: &str) -> (Fixture, PathBuf, PathBuf) {
            let (fixture, root, feat_wt) = Fixture::new(tag);
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.name", "t"]);
            git(&root, &["config", "user.email", "t@e"]);
            git(&root, &["config", "commit.gpgsign", "false"]);
            std::fs::write(root.join("base.txt"), "base\n").unwrap();
            git(&root, &["add", "-A"]);
            git(&root, &["commit", "-q", "-m", "c0"]);
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "feat",
                    feat_wt.to_str().unwrap(),
                    "main",
                ],
            );
            std::fs::write(feat_wt.join("feat.txt"), "feat\n").unwrap();
            git(&feat_wt, &["add", "-A"]);
            git(&feat_wt, &["commit", "-q", "-m", "feat"]);
            (fixture, root, feat_wt)
        }

        fn cfg(agent_command: &str, max: u32) -> MergeQueueConfig {
            MergeQueueConfig {
                target_branch: "main".into(),
                gate_on: false,
                gate_command: String::new(),
                agent_command: agent_command.into(),
                agent_max_attempts: max,
                agent_timeout_secs: 60,
                // Isolate the driver mechanics from the sidebar lifecycle: with the
                // shipped default (organize_folders + on_landed = "expire") a land
                // would refile the test worktree. That path is covered by the
                // merge_lifecycle tests and the smoke drain, not here.
                organize_folders: false,
                ..MergeQueueConfig::default()
            }
        }

        #[test]
        fn agent_resolves_conflict_and_branch_lands() {
            let (_fixture, root, feat_wt) = conflicting_repo("resolve");
            let before = out(&root, &["rev-parse", "main"]);
            // The "agent": rebase feat onto main as a disjoint change so it folds clean.
            let agent = "git reset --hard main -q && echo feat > feat.txt && \
                         git add -A && git commit -q -m resolved";
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let out_ = drive_queue(
                &cfg(agent, 2),
                &uncontained_config(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert_eq!(
                out_.landed,
                ["feat"],
                "branch should land after the agent fix"
            );
            assert!(out_.needs_human.is_empty());
            assert_ne!(out(&root, &["rev-parse", "main"]), before, "main advanced");
        }

        #[test]
        fn missing_fixer_command_holds_without_spending_an_attempt() {
            let (_fixture, root, feat_wt) = conflicting_repo("missing-fixer");
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let outcome = drive_queue(
                &cfg("thegn-no-such-agent-b62", 2),
                &uncontained_config(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );

            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert!(outcome.needs_human.is_empty());
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.agent_attempts, 0);
            assert_eq!(row.status, MqStatus::AgentBlocked.as_str());
            assert!(row.error_detail.unwrap().contains("did not start"));
        }

        #[test]
        fn agent_that_cannot_fix_marks_needs_human() {
            let (_fixture, root, feat_wt) = conflicting_repo("giveup");
            let before = out(&root, &["rev-parse", "main"]);
            // A no-op "agent" never resolves the conflict.
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let out_ = drive_queue(
                &cfg("true", 1),
                &uncontained_config(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert_eq!(out_.needs_human, ["feat"]);
            assert!(out_.landed.is_empty());
            assert_eq!(out(&root, &["rev-parse", "main"]), before, "main held");
        }

        #[test]
        fn signing_infrastructure_failure_never_blames_branch_or_wakes_agent() {
            let (_fixture, root, feat_wt) = clean_repo("sign-infra");
            let before_main = out(&root, &["rev-parse", "main"]);
            let before_feat = out(&root, &["rev-parse", "feat"]);
            git(&root, &["config", "gpg.program", "false"]);
            let sentinel = root.join("agent-must-not-run");
            let mut mq = cfg(&format!("touch {}", sentinel.display()), 1);
            mq.sign_commits = true;
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let mut statuses = Vec::new();
            let outcome = drive_queue(&mq, &Config::default(), &root, &db, vec![queued], |step| {
                statuses.push(step.status.to_string())
            });
            assert_eq!(outcome.gate_error, ["feat"]);
            assert!(outcome.needs_human.is_empty(), "{outcome:?}");
            assert!(!sentinel.exists(), "signing failure woke the fixing agent");
            assert!(statuses.iter().any(|status| status == "gate_error"));
            assert!(!statuses.iter().any(|status| status == "needs_human"));
            assert_eq!(out(&root, &["rev-parse", "main"]), before_main);
            assert_eq!(out(&root, &["rev-parse", "feat"]), before_feat);
        }

        /// The fixing script, as a `sh` one-liner. Ends in `&& true` so the
        /// unknown-provider fallback (`<command> {prompt}`) can append the
        /// prompt harmlessly — `true` ignores its arguments.
        const FIXER: &str = "git reset --hard main -q && echo feat > feat.txt && \
                             git add -A && git commit -q -m resolved && true";

        fn item(db: &Db, feat_wt: &std::path::Path) -> QueueItem {
            let worktree = feat_wt.to_string_lossy().into_owned();
            db.enqueue_merge(&worktree, "feat", "main").unwrap();
            QueueItem {
                worktree,
                branch: "feat".into(),
                location: String::new(),
            }
        }

        fn bound_item(db: &Db, feat_wt: &Path) -> QueueItem {
            let worktree = feat_wt.to_string_lossy().into_owned();
            db.enqueue_merge_candidate_with_identity(
                &worktree,
                "feat",
                "main",
                &MergeCandidate {
                    branch_ref: "refs/heads/feat".into(),
                    candidate_oid: out(feat_wt, &["rev-parse", "HEAD"]),
                },
                &crate::merge_ops::local_enqueue_identity(feat_wt).unwrap(),
            )
            .unwrap();
            QueueItem {
                worktree,
                branch: "feat".into(),
                location: String::new(),
            }
        }

        fn identity_item(db: &Db, feat_wt: &Path) -> QueueItem {
            let worktree = feat_wt.to_string_lossy().into_owned();
            db.enqueue_merge_candidate_with_identity(
                &worktree,
                "feat",
                "main",
                &MergeCandidate {
                    branch_ref: "refs/heads/feat".into(),
                    candidate_oid: out(feat_wt, &["rev-parse", "HEAD"]),
                },
                &crate::merge_ops::local_enqueue_identity(feat_wt).unwrap(),
            )
            .unwrap();
            QueueItem {
                worktree,
                branch: "feat".into(),
                location: String::new(),
            }
        }

        #[test]
        fn enqueue_identity_recreated_in_other_repo_holds_without_fold_or_agent() {
            let (_fixture, root, feat_wt) = clean_repo("enqueue-identity-recreated");
            let main_before = out(&root, &["rev-parse", "main"]);
            let db = Db::open_memory().unwrap();
            db.put_worktree(
                "feat",
                root.to_str().unwrap(),
                feat_wt.to_str().unwrap(),
                "feat",
                None,
                None,
            )
            .unwrap();
            let queued = identity_item(&db, &feat_wt);
            git(
                &root,
                &["worktree", "remove", "--force", feat_wt.to_str().unwrap()],
            );

            let other = root.parent().unwrap().join("other");
            std::fs::create_dir_all(&other).unwrap();
            git(&other, &["init", "-q", "-b", "main"]);
            git(&other, &["config", "user.name", "t"]);
            git(&other, &["config", "user.email", "t@e"]);
            git(&other, &["config", "commit.gpgsign", "false"]);
            std::fs::write(other.join("base.txt"), "other base\n").unwrap();
            git(&other, &["add", "-A"]);
            git(&other, &["commit", "-q", "-m", "other base"]);
            git(
                &other,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "feat",
                    feat_wt.to_str().unwrap(),
                    "main",
                ],
            );

            let sentinel = root.join("fixer-ran");
            let outcome = drive_queue(
                &cfg(&format!("touch {}", sentinel.display()), 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert!(outcome.landed.is_empty());
            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert_eq!(out(&root, &["rev-parse", "main"]), main_before);
            assert!(!sentinel.exists());
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.status, "agent_blocked");
            assert_eq!(row.agent_attempts, 0);
            let detail = row.error_detail.unwrap();
            assert!(detail.contains("identity drift"), "{detail}");
            assert!(detail.contains("different repository"), "{detail}");
        }

        #[test]
        fn enqueue_identity_deleted_worktree_holds_without_fold() {
            let (_fixture, root, feat_wt) = clean_repo("enqueue-identity-deleted");
            let main_before = out(&root, &["rev-parse", "main"]);
            let db = Db::open_memory().unwrap();
            db.put_worktree(
                "feat",
                root.to_str().unwrap(),
                feat_wt.to_str().unwrap(),
                "feat",
                None,
                None,
            )
            .unwrap();
            let queued = identity_item(&db, &feat_wt);
            git(
                &root,
                &["worktree", "remove", "--force", feat_wt.to_str().unwrap()],
            );

            let sentinel = root.join("fixer-ran");
            let outcome = drive_queue(
                &cfg(&format!("touch {}", sentinel.display()), 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert!(outcome.landed.is_empty());
            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert_eq!(out(&root, &["rev-parse", "main"]), main_before);
            assert!(!sentinel.exists());
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.status, "agent_blocked");
            assert_eq!(row.agent_attempts, 0);
            let detail = row.error_detail.unwrap();
            assert!(detail.contains("identity drift"), "{detail}");
            assert!(detail.contains("worktree identity unavailable"), "{detail}");
        }

        #[test]
        fn enqueue_identity_unchanged_row_lands() {
            let (_fixture, root, feat_wt) = clean_repo("enqueue-identity-unchanged");
            let db = Db::open_memory().unwrap();
            db.put_worktree(
                "feat",
                root.to_str().unwrap(),
                feat_wt.to_str().unwrap(),
                "feat",
                None,
                None,
            )
            .unwrap();
            let queued = identity_item(&db, &feat_wt);
            let outcome = drive_queue(
                &cfg("", 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert_eq!(outcome.landed, ["feat"]);
        }

        #[test]
        fn enqueue_identity_legacy_bound_row_holds_without_fold_or_agent() {
            let (_fixture, root, feat_wt) = clean_repo("legacy-identity");
            let main_before = out(&root, &["rev-parse", "main"]);
            let db = Db::open_memory().unwrap();
            db.put_worktree(
                "feat",
                root.to_str().unwrap(),
                feat_wt.to_str().unwrap(),
                "feat",
                None,
                None,
            )
            .unwrap();
            let worktree = feat_wt.to_string_lossy().into_owned();
            db.enqueue_merge_candidate(
                &worktree,
                "feat",
                "main",
                &MergeCandidate {
                    branch_ref: "refs/heads/feat".into(),
                    candidate_oid: out(&feat_wt, &["rev-parse", "HEAD"]),
                },
            )
            .unwrap();
            let queued = QueueItem {
                worktree: worktree.clone(),
                branch: "feat".into(),
                location: String::new(),
            };
            let sentinel = root.join("legacy-fixer-ran");
            let outcome = drive_queue(
                &cfg(&format!("touch {}", sentinel.display()), 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );

            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert!(outcome.landed.is_empty());
            assert_eq!(out(&root, &["rev-parse", "main"]), main_before);
            assert!(!sentinel.exists());
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.status, "agent_blocked");
            assert_eq!(row.agent_attempts, 0);
            let detail = row.error_detail.unwrap();
            assert!(detail.contains("identity drift"), "{detail}");
            assert!(detail.contains("older thegn"), "{detail}");
            assert!(detail.contains("re-enqueue"), "{detail}");
        }

        #[test]
        fn enqueue_identity_legacy_row_reenqueue_adopts_and_lands() {
            let (_fixture, root, feat_wt) = clean_repo("legacy-identity-reenqueue");
            let db = Db::open_memory().unwrap();
            db.put_worktree(
                "feat",
                root.to_str().unwrap(),
                feat_wt.to_str().unwrap(),
                "feat",
                None,
                None,
            )
            .unwrap();
            let worktree = feat_wt.to_string_lossy().into_owned();
            db.enqueue_merge_candidate(
                &worktree,
                "feat",
                "main",
                &MergeCandidate {
                    branch_ref: "refs/heads/feat".into(),
                    candidate_oid: out(&feat_wt, &["rev-parse", "HEAD"]),
                },
            )
            .unwrap();
            let queued = QueueItem {
                worktree: worktree.clone(),
                branch: "feat".into(),
                location: String::new(),
            };
            let first = drive_queue(
                &cfg("true", 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert_eq!(first.agent_blocked, ["feat"]);
            assert!(first.landed.is_empty());

            crate::merge_ops::enqueue_bound_local(&db, &worktree, "feat", "main").unwrap();
            assert!(db.merge_enqueue_identity(&worktree).unwrap().is_some());
            let outcome = drive_queue(
                &cfg("", 2),
                &Config::default(),
                &root,
                &db,
                vec![QueueItem {
                    worktree,
                    branch: "feat".into(),
                    location: String::new(),
                }],
                |_| {},
            );
            assert_eq!(outcome.landed, ["feat"]);
        }

        #[test]
        fn candidate_drift_force_update_holds_without_fold_or_agent() {
            for force_back in [false, true] {
                let tag = if force_back { "force-back" } else { "advance" };
                let (_fixture, root, feat_wt) = conflicting_repo(tag);
                let main_before = out(&root, &["rev-parse", "main"]);
                let db = Db::open_memory().unwrap();
                let queued = bound_item(&db, &feat_wt);
                if force_back {
                    git(&feat_wt, &["reset", "--hard", "HEAD~1"]);
                } else {
                    std::fs::write(feat_wt.join("extra.txt"), "advanced\n").unwrap();
                    git(&feat_wt, &["add", "-A"]);
                    git(
                        &feat_wt,
                        &[
                            "-c",
                            "commit.gpgsign=false",
                            "commit",
                            "-q",
                            "-m",
                            "advanced",
                        ],
                    );
                }
                let sentinel = root.join("fixer-ran");
                let outcome = drive_queue(
                    &cfg(&format!("touch {} && {FIXER}", sentinel.display()), 2),
                    &Config::default(),
                    &root,
                    &db,
                    vec![queued],
                    |_| {},
                );
                assert_eq!(out(&root, &["rev-parse", "main"]), main_before);
                assert_eq!(outcome.agent_blocked, ["feat"]);
                assert!(outcome.landed.is_empty());
                assert!(!sentinel.exists());
                let row = db.list_merge_queue().unwrap().remove(0);
                assert_eq!(row.status, "agent_blocked");
                assert_eq!(row.agent_attempts, 0);
                let detail = row.error_detail.unwrap();
                assert!(detail.contains("identity drift"), "{detail}");
                assert!(detail.contains("re-enqueue"), "{detail}");
            }
        }

        #[test]
        fn candidate_drift_deleted_branch_holds() {
            let (_fixture, root, feat_wt) = conflicting_repo("deleted-branch");
            let main_before = out(&root, &["rev-parse", "main"]);
            let db = Db::open_memory().unwrap();
            db.put_worktree(
                "feat",
                root.to_str().unwrap(),
                feat_wt.to_str().unwrap(),
                "feat",
                None,
                None,
            )
            .unwrap();
            let queued = bound_item(&db, &feat_wt);
            git(&feat_wt, &["checkout", "-q", "--detach"]);
            git(&root, &["branch", "-D", "feat"]);
            let sentinel = root.join("fixer-ran");
            let outcome = drive_queue(
                &cfg(&format!("touch {} && {FIXER}", sentinel.display()), 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert_eq!(out(&root, &["rev-parse", "main"]), main_before);
            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert!(outcome.landed.is_empty());
            assert!(!sentinel.exists());
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.status, "agent_blocked");
            assert_eq!(row.agent_attempts, 0);
            assert!(row.error_detail.unwrap().contains("identity drift"));
        }

        #[test]
        fn candidate_drift_fixer_commit_is_adopted_and_lands() {
            let (_fixture, root, feat_wt) = conflicting_repo("candidate-fixer-commit");
            let db = Db::open_memory().unwrap();
            let queued = bound_item(&db, &feat_wt);
            let outcome = drive_queue(
                &cfg(FIXER, 2),
                &uncontained_config(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert_eq!(outcome.landed, ["feat"]);
        }

        #[test]
        fn candidate_drift_held_fast_forward_is_adopted_and_lands_same_row() {
            let (_fixture, root, feat_wt) = clean_repo("held-fast-forward");
            let db = Db::open_memory().unwrap();
            let queued = bound_item(&db, &feat_wt);
            let before = db.list_merge_queue().unwrap().remove(0);
            db.replace_merge_status(
                &queued.worktree,
                "agent_blocked",
                &MergeStatusFields::default(),
            )
            .unwrap();
            std::fs::write(feat_wt.join("repair.txt"), "human repair\n").unwrap();
            git(&feat_wt, &["add", "repair.txt"]);
            git(
                &feat_wt,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    "human repair",
                ],
            );
            let repaired_tip = out(&feat_wt, &["rev-parse", "HEAD"]);

            let outcome = drive_queue(
                &cfg("true", 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );

            assert_eq!(outcome.landed, ["feat"]);
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.queued_at, before.queued_at);
            assert_eq!(row.agent_attempts, 0);
            assert_eq!(
                db.merge_candidate(&row.worktree)
                    .unwrap()
                    .unwrap()
                    .candidate_oid,
                repaired_tip
            );
        }

        #[test]
        fn candidate_drift_held_rewrite_stays_held() {
            let (_fixture, root, feat_wt) = clean_repo("held-rewrite");
            let db = Db::open_memory().unwrap();
            let queued = bound_item(&db, &feat_wt);
            db.replace_merge_status(
                &queued.worktree,
                "agent_blocked",
                &MergeStatusFields::default(),
            )
            .unwrap();
            std::fs::write(feat_wt.join("repair.txt"), "human repair\n").unwrap();
            git(&feat_wt, &["add", "repair.txt"]);
            git(
                &feat_wt,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    "human repair",
                ],
            );
            git(&feat_wt, &["reset", "--hard", "main"]);

            let outcome = drive_queue(
                &cfg("true", 2),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );

            assert_eq!(outcome.agent_blocked, ["feat"]);
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.agent_attempts, 0);
            assert!(row.error_detail.unwrap().contains("identity drift"));
        }

        #[test]
        fn candidate_drift_fixer_that_switches_branch_holds_before_next_fold() {
            let (_fixture, root, feat_wt) = conflicting_repo("candidate-fixer-switch");
            let main_before = out(&root, &["rev-parse", "main"]);
            let db = Db::open_memory().unwrap();
            let queued = bound_item(&db, &feat_wt);
            let sentinel = root.join("fixer-ran");
            let fixer = format!("touch {} && git switch -q -c other", sentinel.display());
            let mut statuses = Vec::new();
            let outcome = drive_queue(
                &cfg(&fixer, 2),
                &uncontained_config(),
                &root,
                &db,
                vec![queued],
                |step| statuses.push(step.status.to_string()),
            );
            assert_eq!(out(&root, &["rev-parse", "main"]), main_before);
            assert_eq!(outcome.agent_blocked, ["feat"]);
            assert!(sentinel.exists());
            assert_eq!(
                statuses
                    .iter()
                    .filter(|status| status.as_str() == "folding")
                    .count(),
                1,
                "the drift hold must happen before a second fold"
            );
            let row = db.list_merge_queue().unwrap().remove(0);
            assert_eq!(row.status, "agent_blocked");
            assert_eq!(row.agent_attempts, 1);
            let detail = row.error_detail.unwrap();
            assert!(
                detail.contains("fixer left the worktree off the queued branch"),
                "{detail}"
            );
            assert!(detail.contains("re-enqueue"), "{detail}");
        }

        #[test]
        fn candidate_drift_unbound_row_keeps_old_behaviour() {
            let (_fixture, root, feat_wt) = clean_repo("candidate-unbound");
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            std::fs::write(feat_wt.join("extra.txt"), "advanced\n").unwrap();
            git(&feat_wt, &["add", "-A"]);
            git(
                &feat_wt,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    "advanced",
                ],
            );
            let outcome = drive_queue(
                &cfg("true", 1),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );
            assert_eq!(outcome.landed, ["feat"]);
        }

        #[test]
        fn cancelled_drain_leaves_rows_queued() {
            let (_fixture, root, feat_wt) = clean_repo("cancelled-drain");
            let db = Db::open_memory().unwrap();
            let first = item(&db, &feat_wt);
            db.put_worktree(
                "second",
                root.to_str().unwrap(),
                "/queued/second",
                "second",
                None,
                None,
            )
            .unwrap();
            db.enqueue_merge("/queued/second", "second", "main")
                .unwrap();
            let second = QueueItem {
                worktree: "/queued/second".into(),
                branch: "second".into(),
                location: String::new(),
            };
            let _scope = crate::fold_cancel::scope(|| true);
            let outcome = drive_queue(
                &cfg("", 1),
                &Config::default(),
                &root,
                &db,
                vec![first, second],
                |_| {},
            );
            let rows = db.list_merge_queue().unwrap();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|row| row.status == "queued"));
            assert_eq!(outcome.warnings.len(), 1);
            assert!(outcome.warnings[0].contains("2 queued row(s) left untouched"));
            assert!(outcome.landed.is_empty());
        }

        #[test]
        fn cancelled_drain_during_gate_records_gate_error_and_leaves_later_rows_queued() {
            if thegn_core::sandbox_backend::host_os()
                == thegn_core::sandbox_backend::HostOs::Windows
            {
                return;
            }
            let (_fixture, root, feat_wt) = clean_repo("cancelled-drain-gate");
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            db.put_worktree(
                "second",
                root.to_str().unwrap(),
                "/queued/second",
                "second",
                None,
                None,
            )
            .unwrap();
            db.enqueue_merge("/queued/second", "second", "main")
                .unwrap();
            let later = QueueItem {
                worktree: "/queued/second".into(),
                branch: "second".into(),
                location: String::new(),
            };
            let counter = root.join("gate-count");
            std::fs::write(&counter, "").unwrap();
            let command = format!(
                "printf x >> {} ; exec sleep 30",
                thegn_core::util::sh_quote(&counter.to_string_lossy()),
            );
            let mut mq = cfg("", 1);
            mq.gate_on = true;
            mq.gate_command = command;
            let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let probe = std::sync::Arc::clone(&cancelled);
            let _scope =
                crate::fold_cancel::scope(move || probe.load(std::sync::atomic::Ordering::Acquire));
            let gate_marker = counter.clone();
            let cancel_signal = std::sync::Arc::clone(&cancelled);
            let waiter = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if std::fs::metadata(&gate_marker).is_ok_and(|metadata| metadata.len() > 0) {
                        cancel_signal.store(true, std::sync::atomic::Ordering::Release);
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                cancel_signal.store(true, std::sync::atomic::Ordering::Release);
                false
            });
            let outcome = drive_queue(
                &mq,
                &Config::default(),
                &root,
                &db,
                vec![queued, later],
                |_| {},
            );
            assert!(
                waiter.join().unwrap(),
                "the gate must start before cancellation"
            );
            assert!(
                std::fs::metadata(counter).is_ok_and(|metadata| metadata.len() > 0),
                "the gate probe must have fired during the in-flight attempt"
            );
            let rows = db.list_merge_queue().unwrap();
            assert_eq!(rows.len(), 2);
            let feat_worktree = feat_wt.to_string_lossy().into_owned();
            assert_eq!(
                rows.iter()
                    .find(|row| row.worktree == feat_worktree)
                    .unwrap()
                    .status,
                "gate_error",
                "an in-flight cancelled gate is an infrastructure error"
            );
            assert_eq!(
                rows.iter()
                    .find(|row| row.worktree == "/queued/second")
                    .unwrap()
                    .status,
                "queued",
                "rows not yet started remain queued"
            );
            assert_eq!(outcome.gate_error, ["feat"]);
            assert_eq!(outcome.gate_verdicts.len(), 1);
            assert_eq!(outcome.gate_verdicts[0].class, "cancelled");
            assert_eq!(outcome.warnings.len(), 1);
            assert!(outcome.warnings[0].contains("1 queued row(s) left untouched"));
        }

        #[test]
        fn a_named_agent_entry_is_dispatched_without_an_agent_command() {
            let (_fixture, root, feat_wt) = conflicting_repo("byname");
            let full = Config {
                agents: vec![thegn_core::config::NamedCommand {
                    name: "fixer".into(),
                    command: FIXER.into(),
                    hints: Vec::new(),
                    provider: None,
                    harness: None,
                    resume: false,
                    route_via_proxy: false,
                    model: None,
                    env: Default::default(),
                    permissions: Vec::new(),
                    drawer_scope: None,
                    drawer_cwd: None,
                }],
                sandbox: uncontained_config().sandbox,
                ..Config::default()
            };
            // `agent_command` empty — resolution must come from `agent`.
            let mut mq = cfg("", 2);
            mq.agent = "fixer".into();

            let db = Db::open_memory().unwrap();
            let mut steps = Vec::new();
            let out_ = drive_queue(&mq, &full, &root, &db, vec![item(&db, &feat_wt)], |step| {
                steps.push((step.status.to_owned(), step.detail.to_owned()));
            });
            assert_eq!(
                out_.landed,
                ["feat"],
                "a named [[agents]] entry should have been dispatched: {out_:?}; steps={steps:?}"
            );
            assert!(out_.warnings.is_empty());
        }

        #[test]
        fn a_named_agent_that_resolves_to_nothing_warns_instead_of_going_quiet() {
            let (_fixture, root, feat_wt) = conflicting_repo("noagent");
            let mut mq = cfg("", 2);
            mq.agent = "not-configured".into();

            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &mq,
                &uncontained_config(),
                &root,
                &db,
                vec![item(&db, &feat_wt)],
                |_| {},
            );
            // No agent ran, so the branch takes the classic deferred path...
            assert_eq!(out_.deferred, ["feat"]);
            assert!(out_.needs_human.is_empty());
            // ...but the reason is reported rather than looking like a clean no-op.
            assert_eq!(out_.warnings.len(), 1, "{:?}", out_.warnings);
            assert!(out_.warnings[0].contains("not-configured"));
        }

        #[test]
        fn a_custom_prompt_template_reaches_the_agent() {
            let (_fixture, root, feat_wt) = conflicting_repo("prompt");
            // The "agent" only does its job if the sentinel from the configured
            // template is present in the prompt it was handed, so a landing is
            // proof the custom text (not the built-in) got through.
            let agent = "printf '%s' \"$THEGN_TASK_PROMPT\" | grep -q MAGIC-SENTINEL && ";
            let mut mq = cfg(&format!("{agent}{FIXER}"), 2);
            mq.prompts.conflict = "MAGIC-SENTINEL fix {branch} onto {target}:\n{paths}".into();

            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &mq,
                &uncontained_config(),
                &root,
                &db,
                vec![item(&db, &feat_wt)],
                |_| {},
            );
            assert_eq!(
                out_.landed,
                ["feat"],
                "the configured prompt template should have reached the agent"
            );
        }

        #[test]
        fn the_builtin_prompt_carries_no_sentinel() {
            // The negative control for the test above: with no configured
            // template the same agent finds no sentinel and never fixes anything,
            // so `landed` there really was caused by the override.
            let (_fixture, root, feat_wt) = conflicting_repo("nosentinel");
            let agent = "printf '%s' \"$THEGN_TASK_PROMPT\" | grep -q MAGIC-SENTINEL && ";
            let mq = cfg(&format!("{agent}{FIXER}"), 1);

            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &mq,
                &uncontained_config(),
                &root,
                &db,
                vec![item(&db, &feat_wt)],
                |_| {},
            );
            assert_eq!(out_.needs_human, ["feat"]);
            assert!(out_.landed.is_empty());
        }

        #[test]
        fn unregistered_worktree_is_claimed_while_draining() {
            let (_fixture, root, feat_wt) = clean_repo("unregistered-claim");
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let observed_worktree = queued.worktree.clone();
            let outcome = drive_queue_with(
                &cfg("", 1),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
                DriveActions {
                    attempt: |_: &str, _: &thegn_core::remote::GitLoc, _: bool| {
                        let view = db.observe_merge_claim(&observed_worktree)?.unwrap();
                        assert_ne!(view.phase, "unclaimed");
                        assert!(
                            view.lease_until_ms
                                .is_some_and(|until| until > util::now_ms())
                        );
                        Ok(AttemptOutcome::Ready { tip: "tip".into() })
                    },
                    floor: |_: &str| panic!("notify-only"),
                    runner:
                        |_: &str,
                         _: &str,
                         _: &str,
                         _: &str,
                         _: &Failure,
                         _: Option<thegn_core::sandbox::SandboxSpec>| {
                            crate::agent_run::AgentRunVerdict::Exited(Some(1))
                        },
                },
            );

            assert_eq!(outcome.ready, ["feat"]);
            assert_eq!(
                db.observe_merge_claim(&observed_worktree)
                    .unwrap()
                    .unwrap()
                    .phase,
                "unclaimed"
            );
        }

        #[test]
        fn unregistered_worktree_on_another_branch_is_skipped() {
            let (_fixture, root, feat_wt) = clean_repo("unregistered-wrong-branch");
            git(&feat_wt, &["checkout", "-q", "-b", "other"]);
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let worktree = queued.worktree.clone();
            let outcome = drive_queue_with(
                &cfg("", 1),
                &Config::default(),
                &root,
                &db,
                vec![queued],
                |_| {},
                DriveActions {
                    attempt: |_: &str, _: &thegn_core::remote::GitLoc, _: bool| {
                        panic!("unattributable row must not be attempted")
                    },
                    floor: |_: &str| panic!("notify-only"),
                    runner:
                        |_: &str,
                         _: &str,
                         _: &str,
                         _: &str,
                         _: &Failure,
                         _: Option<thegn_core::sandbox::SandboxSpec>| {
                            crate::agent_run::AgentRunVerdict::Exited(Some(1))
                        },
                },
            );

            assert_eq!(outcome.warnings.len(), 1);
            assert!(outcome.warnings[0].contains("refs/heads/feat"));
            assert!(outcome.warnings[0].contains("not attributable"));
            assert_eq!(db.list_merge_queue().unwrap()[0].status, "queued");
            assert_eq!(
                db.observe_merge_claim(&worktree).unwrap().unwrap().phase,
                "unclaimed"
            );
        }

        #[test]
        fn lifecycle_note_refused_cleanup_reaches_drain_warnings() {
            let (_fixture, root, feat_wt) = clean_repo("lifecycle-note-refused");
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let destroy_claim = crate::worktree_lifecycle::try_scoped_destroy_path(&feat_wt)
                .expect("test should own the first destroy claim");
            let config = MergeQueueConfig {
                organize_folders: true,
                on_landed: thegn_core::config::OnLanded::Remove,
                ..cfg("", 1)
            };

            let outcome = drive_queue(
                &config,
                &uncontained_config(),
                &root,
                &db,
                vec![queued],
                |_| {},
            );

            assert_eq!(outcome.landed, ["feat"]);
            assert!(
                outcome
                    .warnings
                    .iter()
                    .any(|warning| { warning.starts_with("merge cleanup: kept feat — ") })
            );
            drop(destroy_claim);
        }
    }
}

#[cfg(test)]
mod typed_read_tests {
    use std::cell::RefCell;

    use super::{
        CandidateUnreadable, Config, DriveActions, Failure, MergeCandidate, MergeQueueConfig,
        QueueItem, classify_oid, drive_queue, drive_queue_with, local_git_text, unreadable_detail,
        verified_land_pin,
    };
    use thegn_core::git_run::{GitOutcome, GitTextError};

    thread_local! {
        static DRAIN_LAND_TEST_HOOK: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    fn set_drain_land_test_hook(hook: impl FnOnce() + 'static) {
        DRAIN_LAND_TEST_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn run_drain_land_test_hook() {
        let hook = DRAIN_LAND_TEST_HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    #[test]
    fn classify_oid_keeps_exit_one_as_absent_and_everything_else_as_failure() {
        let absent = Err(GitTextError::Failed(GitOutcome::Exit {
            code: Some(1),
            stderr_tail: String::new(),
        }));
        assert!(matches!(classify_oid(absent), Ok(None)));

        let stderr_failure = Err(GitTextError::Failed(GitOutcome::Exit {
            code: Some(1),
            stderr_tail: "fatal: x".into(),
        }));
        assert!(
            classify_oid(stderr_failure)
                .unwrap_err()
                .contains("fatal: x")
        );

        let exit_failure = Err(GitTextError::Failed(GitOutcome::Exit {
            code: Some(128),
            stderr_tail: "fatal: repository failure".into(),
        }));
        assert!(classify_oid(exit_failure).is_err());
        assert!(
            classify_oid(Err(GitTextError::Failed(GitOutcome::Timeout)))
                .unwrap_err()
                .contains("timed out")
        );

        assert!(matches!(classify_oid(Ok("a".repeat(40))), Ok(Some(_))));
        assert!(matches!(classify_oid(Ok("b".repeat(64))), Ok(Some(_))));
        for malformed in [String::new(), "HEAD".into(), "A".repeat(40)] {
            assert!(
                classify_oid(Ok(malformed))
                    .unwrap_err()
                    .contains("malformed object id")
            );
        }
    }

    #[test]
    fn queued_tip_read_in_a_non_repository_is_a_failure_not_absence() {
        // This fixture relies on /tmp being outside the checked-out repository so
        // Git's upward repository discovery cannot find the outer worktree.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let result = classify_oid(local_git_text(
            dir.path(),
            &["rev-parse", "--verify", "-q", "refs/heads/feat"],
        ));
        assert!(result.is_err(), "non-repository read was treated as absent");
        assert!(
            result
                .unwrap_err()
                .to_lowercase()
                .contains("not a git repository")
        );
    }

    #[test]
    fn queued_tip_read_distinguishes_present_and_absent_refs() {
        // Positive control: Git's clean exit-1 answer for a missing verified ref remains absence.
        #[expect(clippy::disallowed_methods, reason = "create an isolated Git fixture")]
        fn git(dir: &std::path::Path, args: &[&str]) {
            let status = thegn_core::util::git_cmd(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed with {status}");
        }

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@e"]);
        git(dir.path(), &["config", "user.name", "t"]);
        git(
            dir.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "base",
            ],
        );

        let present = classify_oid(local_git_text(
            dir.path(),
            &["rev-parse", "--verify", "-q", "refs/heads/main"],
        ));
        assert!(matches!(present, Ok(Some(oid)) if oid.len() == 40 || oid.len() == 64));
        assert!(matches!(
            classify_oid(local_git_text(
                dir.path(),
                &["rev-parse", "--verify", "-q", "refs/heads/nope"],
            )),
            Ok(None)
        ));
    }

    #[test]
    fn unreadable_queued_tip_holds_row_without_attempt_or_fixer() {
        use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

        #[expect(clippy::disallowed_methods, reason = "create an isolated Git fixture")]
        fn git(dir: &std::path::Path, args: &[&str]) {
            let status = thegn_core::util::git_cmd(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed with {status}");
        }

        let repo = tempfile::tempdir_in("/tmp").unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        let db = thegn_core::db::Db::open_memory().unwrap();
        let repo_path = repo.path().to_string_lossy().into_owned();
        db.put_worktree("feat", &repo_path, &repo_path, "feat", None, None)
            .unwrap();
        db.enqueue_merge(&repo_path, "feat", "main").unwrap();
        let queued = QueueItem {
            worktree: repo_path.clone(),
            branch: "feat".into(),
            location: String::new(),
        };

        let outcome = drive_queue_with(
            &MergeQueueConfig {
                organize_folders: false,
                ..MergeQueueConfig::default()
            },
            &Config::default(),
            repo.path(),
            &db,
            vec![queued],
            |_| {},
            DriveActions {
                attempt: |_: &str,
                          _: &thegn_core::remote::GitLoc,
                          _: bool|
                 -> anyhow::Result<super::AttemptOutcome> {
                    Err(anyhow::Error::new(CandidateUnreadable {
                        reason: "queued branch tip refs/heads/feat: Git command timed out".into(),
                    }))
                },
                floor: |_: &str| panic!("unreadable candidate must not dispatch a fixer"),
                runner: |_: &str,
                         _: &str,
                         _: &str,
                         _: &str,
                         _: &Failure,
                         _: Option<thegn_core::sandbox::SandboxSpec>| {
                    panic!("unreadable candidate must not run a fixer")
                },
            },
        );

        assert_eq!(outcome.agent_blocked, ["feat"]);
        assert!(outcome.landed.is_empty());
        assert!(outcome.needs_human.is_empty());
        let row = db.list_merge_queue().unwrap().remove(0);
        assert_eq!(row.status, "agent_blocked");
        assert_eq!(row.agent_attempts, 0);
        let detail = row.error_detail.unwrap();
        assert!(detail.contains("candidate unreadable"));
        assert!(!detail.contains("identity drift"));
        assert!(!detail.contains("re-enqueue"));
    }

    #[test]
    fn unreadable_detail_is_not_identity_drift() {
        let detail = unreadable_detail("Git command timed out");
        assert!(detail.contains("candidate unreadable"));
        assert!(detail.contains("Git command timed out"));
        assert!(!detail.contains("identity drift"));
        assert!(!detail.contains("re-enqueue"));
    }

    #[test]
    fn drain_land_pin_is_the_verified_tip_of_a_candidate_row() {
        let candidate = MergeCandidate {
            branch_ref: "refs/heads/feat".into(),
            candidate_oid: "a".repeat(40),
        };
        let verified_tip = "b".repeat(40);
        assert_eq!(
            verified_land_pin(Some(&candidate), Some(&verified_tip)),
            Some(verified_tip)
        );
    }

    #[test]
    fn drain_land_pin_is_none_without_a_candidate() {
        let candidate = MergeCandidate {
            branch_ref: "refs/heads/feat".into(),
            candidate_oid: "a".repeat(40),
        };
        let verified_tip = "b".repeat(40);
        assert_eq!(verified_land_pin(None, Some(&verified_tip)), None);
        assert_eq!(verified_land_pin(Some(&candidate), None), None);
    }

    #[test]
    fn drain_passes_verified_tip_when_branch_moves_before_landing() {
        use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};

        #[expect(clippy::disallowed_methods, reason = "create an isolated Git fixture")]
        fn git(dir: &std::path::Path, args: &[&str]) {
            let status = thegn_core::util::git_cmd(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed with {status}");
        }

        let tmp_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let dir = tempfile::tempdir_in(tmp_root).unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@e"]);
        git(dir.path(), &["config", "user.name", "t"]);
        git(
            dir.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "base",
            ],
        );
        git(dir.path(), &["checkout", "-q", "-b", "feat"]);
        git(
            dir.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "first feature commit",
            ],
        );

        let main_before = local_git_text(dir.path(), &["rev-parse", "refs/heads/main"])
            .unwrap()
            .trim()
            .to_owned();
        let worktree = dir.path().to_string_lossy().into_owned();
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree("feat", &worktree, &worktree, "feat", None, None)
            .unwrap();
        db.enqueue_merge_candidate_with_identity(
            &worktree,
            "feat",
            "main",
            &MergeCandidate {
                branch_ref: "refs/heads/feat".into(),
                candidate_oid: local_git_text(dir.path(), &["rev-parse", "refs/heads/feat"])
                    .unwrap()
                    .trim()
                    .to_owned(),
            },
            &crate::merge_ops::local_enqueue_identity(dir.path()).unwrap(),
        )
        .unwrap();

        // This runs after drive_queue verifies the queued candidate and just
        // before it calls the production land path, reproducing a concurrent
        // branch advance at the boundary the pin protects.
        let hook_dir = dir.path().to_path_buf();
        set_drain_land_test_hook(move || {
            git(
                &hook_dir,
                &[
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    "second feature commit",
                ],
            );
        });

        let outcome = drive_queue(
            &MergeQueueConfig {
                target_branch: "main".into(),
                organize_folders: false,
                ..MergeQueueConfig::default()
            },
            &Config::default(),
            dir.path(),
            &db,
            vec![QueueItem {
                worktree,
                branch: "feat".into(),
                location: String::new(),
            }],
            |_| {},
        );
        assert!(
            outcome.gate_error == ["feat"],
            "expected moved branch to be refused, got {outcome:?}"
        );
        let expected_class = if thegn_core::sandbox_backend::host_os()
            == thegn_core::sandbox_backend::HostOs::Windows
        {
            "preparation"
        } else {
            "source_moved"
        };
        assert_eq!(outcome.gate_verdicts[0].class, expected_class);
        let main_after = local_git_text(dir.path(), &["rev-parse", "refs/heads/main"])
            .unwrap()
            .trim()
            .to_owned();
        assert_eq!(main_after, main_before, "refused landing changed main");
        let row = db.list_merge_queue().unwrap().remove(0);
        assert_eq!(row.status, "gate_error");
    }
}
