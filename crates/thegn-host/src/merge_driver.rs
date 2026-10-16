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

use thegn_core::attention::MqStatus;
use thegn_core::config::{Config, ConflictHandoff, MergeQueueConfig};
use thegn_core::db::Db;
use thegn_core::store::{MergeStatusFields, WorkspaceStore, WorktreeAuxStore};
use thegn_core::util;

use crate::integrate::{self, AttemptOutcome};

#[cfg(test)]
#[path = "merge_driver_status_tests.rs"]
mod status_tests;

/// A worktree branch to drain, as read from the `merge_queue` cache.
#[derive(Debug, Clone)]
pub(crate) struct QueueItem {
    pub worktree: String,
    pub branch: String,
    /// The worktree's `location` descriptor (from the queue row): empty = local,
    /// else an ssh/provider blob. Resolves the branch's `GitLoc` so the drain
    /// knows whether to fetch its tip into the target store (cross-host).
    pub location: String,
    /// Agent-dispatch cycles already spent on this row (from the queue row).
    /// The `agent_max_attempts` budget belongs to the BRANCH, not to one drain
    /// invocation — otherwise a `needs_human` row that had already exhausted it
    /// got the full budget again on every subsequent drain.
    pub agent_attempts: u32,
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
    drive_queue_with(
        cfg,
        full,
        repo_root,
        db,
        items,
        progress,
        DriveActions {
            attempt: |branch: &str, location: &thegn_core::remote::GitLoc| {
                if let thegn_core::remote::GitLoc::Local(worktree) = location {
                    let ref_name = format!("refs/heads/{branch}");
                    let key = worktree.to_string_lossy().into_owned();
                    if let Some(oid) = util::git_out(repo_root, &["rev-parse", &ref_name]) {
                        expected_oids.borrow_mut().insert(key, oid);
                    } else {
                        expected_oids.borrow_mut().remove(&key);
                    }
                }
                integrate::attempt_land(cfg, repo_root, branch, location)
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
                run_agent(cfg, template, worktree, branch, target, failure, sandbox)
            },
        },
    )
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
    let top_level = util::git_out(path, &["rev-parse", "--show-toplevel"])
        .ok_or_else(|| "path is not a Git worktree".to_string())?;
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
    let registered =
        util::git_out(repo_root, &["worktree", "list", "--porcelain"]).is_some_and(|list| {
            list.lines().any(|line| {
                line.strip_prefix("worktree ")
                    == Some(canonical_worktree.to_string_lossy().as_ref())
            })
        });
    if !registered {
        return Err("path is not a registered worktree of the queued repository".into());
    }
    let actual_ref = util::git_out(&canonical_worktree, &["symbolic-ref", "-q", "HEAD"])
        .ok_or_else(|| "worktree HEAD is detached or unavailable".to_string())?;
    let expected_ref = format!("refs/heads/{branch}");
    if actual_ref != expected_ref {
        return Err(format!("expected {expected_ref}, found {actual_ref}"));
    }
    if let Some(expected_oid) = expected_oid {
        let actual_oid = util::git_out(&canonical_worktree, &["rev-parse", "HEAD"])
            .ok_or_else(|| "worktree HEAD is unavailable".to_string())?;
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

fn drive_queue_with(
    cfg: &MergeQueueConfig,
    full: &Config,
    repo_root: &Path,
    db: &Db,
    items: Vec<QueueItem>,
    mut progress: impl FnMut(&DriveStep),
    mut actions: DriveActions<
        impl FnMut(&str, &thegn_core::remote::GitLoc) -> anyhow::Result<AttemptOutcome>,
        impl FnMut(&str) -> crate::agent_run::AgentDispatch,
        impl FnMut(&str, &str, &str, &str, &Failure, Option<thegn_core::sandbox::SandboxSpec>) -> bool,
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
        let write_error = std::cell::RefCell::new(None::<String>);
        let set = |db: &Db, status: MqStatus, oid: Option<&str>, detail: Option<&str>| {
            let fields = MergeStatusFields {
                result_oid: oid.map(str::to_owned),
                conflict_paths: None,
                error_detail: detail.map(str::to_owned),
            };
            if let Err(error) = replace_status(db, &item.worktree, status, &fields) {
                *write_error.borrow_mut() = Some(format!("{}: {error}", item.worktree));
            }
            fields
        };
        macro_rules! stop_on_write_error {
            () => {
                if let Some(error) = write_error.borrow_mut().take() {
                    tracing::error!(target: "thegn::merge", "queue state write failed: {error}");
                    out.persistence_errors.push(error);
                    break 'items;
                }
            };
        }
        // Sidebar-folder lifecycle: move the worktree on a settled transition
        // (landed ⇒ Merged/cleanup, failure ⇒ the failed folder). No-op unless
        // `[merge_queue] organize_folders` is on.
        let lifecycle = |db: &Db, event: thegn_core::merge_lifecycle::LifecycleEvent| {
            crate::merge_lifecycle::apply(cfg, db, repo_root, &item.worktree, &item.branch, event);
        };
        let fields = set(db, MqStatus::Folding, None, None);
        stop_on_write_error!();
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
        let mut agent_runs = item.agent_attempts;
        loop {
            let attempt = match (actions.attempt)(&item.branch, &branch_loc) {
                Ok(a) => a,
                Err(e) => {
                    let detail = format!("{e}");
                    let fields = set(db, MqStatus::NeedsHuman, None, Some(&detail));
                    stop_on_write_error!();
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
                    if let Some(error) = write_error.borrow_mut().take() {
                        out.bookkeeping_failed.push(RefAdvancedBookkeepingFailed {
                            branch: item.branch.clone(),
                            target: target.clone(),
                            old_oid,
                            new_oid: commit,
                            error,
                        });
                        out.landed.push(item.branch.clone());
                        break 'items;
                    }
                    crate::merge_lifecycle::apply_landed(
                        cfg,
                        db,
                        repo_root,
                        &item.worktree,
                        &item.branch,
                        &commit,
                    );
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
                    stop_on_write_error!();
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
                    stop_on_write_error!();
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
                    stop_on_write_error!();
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
                    stop_on_write_error!();
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
                        if let Err(error) =
                            replace_status(db, &item.worktree, MqStatus::NeedsHuman, &fields)
                        {
                            out.persistence_errors.push(format!(
                                "{}: could not persist driver refusal: {error}",
                                item.worktree
                            ));
                            break 'items;
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
                    stop_on_write_error!();
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
                // Opt-in isolation floor for the handoff (default: host + slice).
                // Gate BEFORE consuming an attempt: a fail-closed miss, or an
                // unbuildable sandbox under a demanded floor, is an INFRASTRUCTURE
                // failure — hold the entry and NEVER blame the branch.
                let dispatch = (actions.floor)(&item.worktree);
                let sandbox = match dispatch {
                    crate::agent_run::AgentDispatch::InfraHold(reason) => {
                        thegn_core::msg::warn(&reason);
                        let fields = set(db, MqStatus::AgentBlocked, None, Some(&reason));
                        stop_on_write_error!();
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
                agent_runs += 1;
                if let Err(error) = db.set_merge_agent_attempts(&item.worktree, agent_runs) {
                    let detail = format!("could not reserve agent attempt: {error}");
                    out.persistence_errors
                        .push(format!("{}: {detail}", item.worktree));
                    set(db, MqStatus::GateError, None, Some(&detail));
                    stop_on_write_error!();
                    break 'items;
                }
                let note = format!("agent fixing ({agent_runs}/{})", cfg.agent_max_attempts);
                let fields = set(db, MqStatus::AgentRunning, None, Some(&note));
                stop_on_write_error!();
                progress(&DriveStep {
                    fields: &fields,
                    worktree: &item.worktree,
                    branch: &item.branch,
                    status: MqStatus::AgentRunning.as_str(),
                    detail: &note,
                });
                // Run to completion; the re-attempt (top of loop) is the real
                // arbiter of whether the fix worked, so ignore the exit code.
                if let Some(template) = agent_cmd.as_deref() {
                    let _ = (actions.runner)(
                        // best-effort: the re-attempt at the top of the loop is the real arbiter (comment above)
                        template,
                        &item.worktree,
                        &item.branch,
                        &target,
                        &failure,
                        sandbox,
                    );
                }
                continue;
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
                    if let Err(error) = replace_status(db, &item.worktree, status, &fields) {
                        let detail = format!("could not persist conflict outcome: {error}");
                        out.persistence_errors
                            .push(format!("{}: {detail}", item.worktree));
                        set(db, MqStatus::GateError, None, Some(&detail));
                        stop_on_write_error!();
                        break 'items;
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
                    stop_on_write_error!();
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
fn run_agent(
    cfg: &MergeQueueConfig,
    command_template: &str,
    worktree: &str,
    branch: &str,
    target: &str,
    failure: &Failure,
    sandbox: Option<thegn_core::sandbox::SandboxSpec>,
) -> bool {
    let Some((kind, vars, prompt)) = compose(cfg, worktree, branch, target, failure) else {
        return false;
    };
    crate::agent_run::run(&crate::agent_run::AgentTaskRun {
        kind,
        worktree,
        prompt: &prompt,
        command_template,
        vars: &vars,
        timeout_secs: cfg.agent_timeout_secs,
        sandbox,
        credential_free: false,
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
        fn switched_queued_worktree_holds_without_dispatch_or_attempt_spend() {
            let (_fixture, root, feat_wt) = conflicting_repo("identity-hold");
            let sentinel = root.join("wrong-checkout-was-written");
            let db = Db::open_memory().unwrap();
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
                &Config::default(),
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
        fn agent_that_cannot_fix_marks_needs_human() {
            let (_fixture, root, feat_wt) = conflicting_repo("giveup");
            let before = out(&root, &["rev-parse", "main"]);
            // A no-op "agent" never resolves the conflict.
            let db = Db::open_memory().unwrap();
            let queued = item(&db, &feat_wt);
            let out_ = drive_queue(
                &cfg("true", 1),
                &Config::default(),
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
                agent_attempts: 0,
            }
        }

        #[test]
        fn cancelled_drain_leaves_rows_queued() {
            let (_fixture, root, feat_wt) = clean_repo("cancelled-drain");
            let db = Db::open_memory().unwrap();
            let first = item(&db, &feat_wt);
            db.enqueue_merge("/queued/second", "second", "main")
                .unwrap();
            let second = QueueItem {
                worktree: "/queued/second".into(),
                branch: "second".into(),
                location: String::new(),
                agent_attempts: 0,
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
            db.enqueue_merge("/queued/second", "second", "main")
                .unwrap();
            let later = QueueItem {
                worktree: "/queued/second".into(),
                branch: "second".into(),
                location: String::new(),
                agent_attempts: 0,
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
                &Config::default(),
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
                &Config::default(),
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
                &Config::default(),
                &root,
                &db,
                vec![item(&db, &feat_wt)],
                |_| {},
            );
            assert_eq!(out_.needs_human, ["feat"]);
            assert!(out_.landed.is_empty());
        }
    }
}
