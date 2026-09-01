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

use std::path::Path;

use thegn_core::config::{Config, ConflictHandoff, MergeQueueConfig};
use thegn_core::db::Db;
use thegn_core::store::{WorkspaceStore, WorktreeAuxStore};
// The real-git driver fixtures shell out to git and stamp queue rows.
#[cfg(test)]
use thegn_core::util;

use crate::integrate::{self, AttemptOutcome};

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
/// callback (the DB row is already written when this fires).
pub(crate) struct DriveStep<'a> {
    /// The queue row's key — lets the host patch its panel row in place.
    pub worktree: &'a str,
    pub branch: &'a str,
    pub status: &'a str,
    pub detail: &'a str,
}

/// Summary of a full drain.
#[derive(Debug, Default, Clone)]
pub(crate) struct DriveOutcome {
    pub landed: Vec<String>,
    pub ready: Vec<String>,
    pub deferred: Vec<String>,
    pub needs_human: Vec<String>,
    /// Branches whose gate could not RUN — an environment failure, reported
    /// separately from `deferred` so a caller never reads "the branch is bad"
    /// out of "the gate binary is missing".
    pub gate_error: Vec<String>,
    /// Live checkouts of the target branch and what the ref advance did to them.
    /// Advisory; the CLI warns about the ones it could not fast-forward.
    pub resyncs: Vec<thegn_core::util::CheckoutResync>,
    /// Non-fatal problems worth telling the user about — today, an agent handoff
    /// that was configured but could not be resolved. Surfaced rather than
    /// swallowed: silently degrading to "notify" looks identical to a queue that
    /// simply had nothing to fix.
    pub warnings: Vec<String>,
}

/// Why a branch didn't land — the material a fixing agent needs.
enum Failure {
    Conflict(Vec<String>),
    Gate(String),
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
pub(crate) fn rows_for_repo(db: &Db, root: &Path) -> Vec<thegn_core::db::MergeQueueRow> {
    let root_s = root.to_string_lossy();
    db.list_merge_queue()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| row_belongs_to_repo(db, &r.worktree, &root_s, root))
        .collect()
}

/// One membership test (see [`rows_for_repo`]): DB repo_path when known, else a
/// local `git worktree list` fallback for an unregistered worktree.
fn row_belongs_to_repo(db: &Db, worktree: &str, root_s: &str, root: &Path) -> bool {
    if let Ok(Some(rp)) = db.repo_root_for(worktree)
        && !rp.is_empty()
    {
        return rp == root_s;
    }
    integrate::main_checkout(Path::new(worktree)).as_deref() == Some(root)
}

/// Drain `items` one at a time, landing clean branches and dispatching the agent
/// on the rest. `progress` is invoked after each status write. Best-effort DB
/// writes (the DB is a cache; the git refs are the source of truth).
pub(crate) fn drive_queue(
    cfg: &MergeQueueConfig,
    full: &Config,
    repo_root: &Path,
    db: &Db,
    items: Vec<QueueItem>,
    mut progress: impl FnMut(&DriveStep),
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

    for item in items {
        let set = |db: &Db, status: &str, oid: Option<&str>, detail: Option<&str>| {
            let _ = db.update_merge_status(&item.worktree, status, oid, detail, None); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        };
        // Sidebar-folder lifecycle: move the worktree on a settled transition
        // (landed ⇒ Merged/cleanup, failure ⇒ the failed folder). No-op unless
        // `[merge_queue] organize_folders` is on.
        let lifecycle = |db: &Db, event: thegn_core::merge_lifecycle::LifecycleEvent| {
            crate::merge_lifecycle::apply(cfg, db, repo_root, &item.worktree, &item.branch, event);
        };
        set(db, "folding", None, None);
        progress(&DriveStep {
            worktree: &item.worktree,
            branch: &item.branch,
            status: "folding",
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
            let attempt = match integrate::attempt_land(cfg, repo_root, &item.branch, &branch_loc) {
                Ok(a) => a,
                Err(e) => {
                    let detail = format!("{e}");
                    set(db, "needs_human", None, Some(&detail));
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: "needs_human",
                        detail: &detail,
                    });
                    out.needs_human.push(item.branch.clone());
                    break;
                }
            };

            let failure = match attempt {
                AttemptOutcome::Landed { commit, resyncs } => {
                    // Carried out to the caller so the CLI can warn about any
                    // live checkout of the target left stale by the ref move.
                    out.resyncs.extend(resyncs);
                    set(db, "landed", Some(&commit), None);
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Landed);
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: "landed",
                        detail: &commit[..commit.len().min(12)],
                    });
                    out.landed.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::UpToDate => {
                    set(db, "landed", None, Some("already merged"));
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Landed);
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: "landed",
                        detail: "already merged",
                    });
                    out.landed.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::Ready { tip } => {
                    set(db, "ready", Some(&tip), Some("gated green — awaiting land"));
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: "ready",
                        detail: "gated green — awaiting land",
                    });
                    out.ready.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::Unreachable { detail } => {
                    // Branch host unreachable / tip couldn't be fetched in. Hold
                    // (deferred) with the reason — a transient blip is retried on
                    // the next drain; never silently drop the row.
                    set(db, "deferred", None, Some(&detail));
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: "deferred",
                        detail: &detail,
                    });
                    out.deferred.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::GateError { reason, log } => {
                    // The gate could not RUN. This is a fact about the
                    // environment, not a verdict about the branch, so it must
                    // NOT become a `Failure`: handing it to the fixing agent
                    // would set a coding model loose on source code in response
                    // to `command not found`. Record it as its own state and
                    // stop; the row is retried on the next drain, by which time
                    // the environment may be fixed.
                    let detail = detail_with_log(&reason, &log);
                    set(db, "gate_error", None, Some(&detail));
                    lifecycle(db, thegn_core::merge_lifecycle::LifecycleEvent::Failed);
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status: "gate_error",
                        detail: &detail,
                    });
                    out.gate_error.push(item.branch.clone());
                    break;
                }
                AttemptOutcome::Conflict { paths } => Failure::Conflict(paths),
                AttemptOutcome::GateFailed { log } => Failure::Gate(log),
            };

            // A land failure. Dispatch the agent to fix it, if we still can.
            if use_agent && agent_runs < cfg.agent_max_attempts {
                // Opt-in isolation floor for the handoff (default: host + slice).
                // Gate BEFORE consuming an attempt: a fail-closed miss, or an
                // unbuildable sandbox under a demanded floor, is an INFRASTRUCTURE
                // failure — hold the entry and NEVER blame the branch.
                let dispatch = crate::agent_run::agent_floor_gate(
                    full,
                    &item.worktree,
                    cfg.agent_sandbox,
                    cfg.agent_isolation_floor,
                    cfg.agent_on_floor_miss,
                );
                let sandbox = match dispatch {
                    crate::agent_run::AgentDispatch::InfraHold(reason) => {
                        thegn_core::msg::warn(&reason);
                        set(db, "agent_blocked", None, Some(&reason));
                        progress(&DriveStep {
                            worktree: &item.worktree,
                            branch: &item.branch,
                            status: "agent_blocked",
                            detail: &reason,
                        });
                        // Held for a later drain; NOT a branch/gate failure.
                        out.deferred.push(item.branch.clone());
                        continue;
                    }
                    crate::agent_run::AgentDispatch::RunDegraded(spec, warning) => {
                        thegn_core::msg::warn(&warning);
                        spec
                    }
                    crate::agent_run::AgentDispatch::Run(spec) => spec,
                };
                agent_runs += 1;
                let _ = db.set_merge_agent_attempts(&item.worktree, agent_runs); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                let note = format!("agent fixing ({agent_runs}/{})", cfg.agent_max_attempts);
                set(db, "agent_running", None, Some(&note));
                progress(&DriveStep {
                    worktree: &item.worktree,
                    branch: &item.branch,
                    status: "agent_running",
                    detail: &note,
                });
                // Run to completion; the re-attempt (top of loop) is the real
                // arbiter of whether the fix worked, so ignore the exit code.
                if let Some(template) = agent_cmd.as_deref() {
                    let _ = run_agent(
                        // best-effort: the re-attempt at the top of the loop is the real arbiter (comment above)
                        cfg,
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
                Failure::Conflict(paths) => {
                    let detail = paths.join("\n");
                    let status = if agent_runs > 0 {
                        "needs_human"
                    } else {
                        "deferred"
                    };
                    set(
                        db,
                        status,
                        None,
                        (!detail.is_empty()).then_some(&detail).map(|s| s.as_str()),
                    );
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status,
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
                        "needs_human"
                    } else {
                        "gate_failed"
                    };
                    // Persist the actual gate output, not a fixed two-word
                    // string: "breaks build" told the user nothing about WHY,
                    // and the log was otherwise discarded entirely.
                    let detail = detail_with_log("breaks build", &log);
                    set(db, status, None, Some(&detail));
                    progress(&DriveStep {
                        worktree: &item.worktree,
                        branch: &item.branch,
                        status,
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
    use thegn_core::agent_task::{TaskKind, TaskVars, format_paths, render_prompt};

    let (kind, vars) = match failure {
        Failure::Conflict(paths) => (
            TaskKind::MergeConflict,
            TaskVars::new().set("paths", format_paths(paths)),
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

    match render_prompt(cfg.prompts.resolve(kind), &vars) {
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
    const MAX_LOG: usize = 2000;
    let log = log.trim();
    if log.is_empty() {
        return headline.to_string();
    }
    let mut cut = log.len().saturating_sub(MAX_LOG);
    while cut < log.len() && !log.is_char_boundary(cut) {
        cut += 1;
    }
    let body = if cut > 0 { &log[cut..] } else { log };
    format!("{headline}\n{body}")
}

/// The last non-empty line of a log (for a one-line status detail), falling back
/// to a headline when the command produced no output at all — a bare `exit 1`
/// gate would otherwise render as "needs a human — " with nothing after it.
fn tail_line(log: &str) -> String {
    log.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("breaks build (gate exited non-zero, no output)")
        .to_string()
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
            &Failure::Conflict(vec!["src/a.rs".into(), "src/b.rs".into()]),
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
            &Failure::Conflict(vec!["src/a.rs".into()]),
        )
        .unwrap();
        assert_eq!(p, "fix feat-x vs main, see:\n  - src/a.rs\n");
        // The built-in rules are gone precisely because the user replaced them.
        assert!(!p.contains("Do NOT push"));
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
            compose(&cfg, "/w/x", "b", "main", &Failure::Conflict(vec![])).is_none(),
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
        /// commit conflicts with `main` on `base.txt`. Returns (repo_root, feat_wt).
        fn conflicting_repo(tag: &str) -> (PathBuf, PathBuf) {
            let root = std::env::temp_dir().join(format!(
                "tg-drive-{tag}-{}-{}",
                std::process::id(),
                util::now()
            ));
            let feat_wt = root.with_extension("feat");
            let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            let _ = std::fs::remove_dir_all(&feat_wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            std::fs::create_dir_all(&root).unwrap();
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
            (root, feat_wt)
        }

        fn cfg(agent_command: &str, max: u32) -> MergeQueueConfig {
            // Hermetic shell for run_agent's `$SHELL -lc` wrapper (nextest isolates
            // env per test process).
            unsafe {
                std::env::set_var("SHELL", "/bin/sh");
            }
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
            let (root, feat_wt) = conflicting_repo("resolve");
            let before = out(&root, &["rev-parse", "main"]);
            // The "agent": rebase feat onto main as a disjoint change so it folds clean.
            let agent = "git reset --hard main -q && echo feat > feat.txt && \
                         git add -A && git commit -q -m resolved";
            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &cfg(agent, 2),
                &Config::default(),
                &root,
                &db,
                vec![QueueItem {
                    worktree: feat_wt.to_string_lossy().into(),
                    branch: "feat".into(),
                    location: String::new(),
                    agent_attempts: 0,
                }],
                |_| {},
            );
            assert_eq!(
                out_.landed,
                ["feat"],
                "branch should land after the agent fix"
            );
            assert!(out_.needs_human.is_empty());
            assert_ne!(out(&root, &["rev-parse", "main"]), before, "main advanced");
            let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            let _ = std::fs::remove_dir_all(&feat_wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }

        #[test]
        fn agent_that_cannot_fix_marks_needs_human() {
            let (root, feat_wt) = conflicting_repo("giveup");
            let before = out(&root, &["rev-parse", "main"]);
            // A no-op "agent" never resolves the conflict.
            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &cfg("true", 1),
                &Config::default(),
                &root,
                &db,
                vec![QueueItem {
                    worktree: feat_wt.to_string_lossy().into(),
                    branch: "feat".into(),
                    location: String::new(),
                    agent_attempts: 0,
                }],
                |_| {},
            );
            assert_eq!(out_.needs_human, ["feat"]);
            assert!(out_.landed.is_empty());
            assert_eq!(out(&root, &["rev-parse", "main"]), before, "main held");
            let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            let _ = std::fs::remove_dir_all(&feat_wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }

        /// The fixing script, as a `sh` one-liner. Ends in `&& true` so the
        /// unknown-provider fallback (`<command> {prompt}`) can append the
        /// prompt harmlessly — `true` ignores its arguments.
        const FIXER: &str = "git reset --hard main -q && echo feat > feat.txt && \
                             git add -A && git commit -q -m resolved && true";

        fn item(feat_wt: &std::path::Path) -> QueueItem {
            QueueItem {
                worktree: feat_wt.to_string_lossy().into(),
                branch: "feat".into(),
                location: String::new(),
                agent_attempts: 0,
            }
        }

        #[test]
        fn a_named_agent_entry_is_dispatched_without_an_agent_command() {
            let (root, feat_wt) = conflicting_repo("byname");
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
            let out_ = drive_queue(&mq, &full, &root, &db, vec![item(&feat_wt)], |_| {});
            assert_eq!(
                out_.landed,
                ["feat"],
                "a named [[agents]] entry should have been dispatched"
            );
            assert!(out_.warnings.is_empty());
            let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            let _ = std::fs::remove_dir_all(&feat_wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }

        #[test]
        fn a_named_agent_that_resolves_to_nothing_warns_instead_of_going_quiet() {
            let (root, feat_wt) = conflicting_repo("noagent");
            let mut mq = cfg("", 2);
            mq.agent = "not-configured".into();

            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &mq,
                &Config::default(),
                &root,
                &db,
                vec![item(&feat_wt)],
                |_| {},
            );
            // No agent ran, so the branch takes the classic deferred path...
            assert_eq!(out_.deferred, ["feat"]);
            assert!(out_.needs_human.is_empty());
            // ...but the reason is reported rather than looking like a clean no-op.
            assert_eq!(out_.warnings.len(), 1, "{:?}", out_.warnings);
            assert!(out_.warnings[0].contains("not-configured"));
            let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            let _ = std::fs::remove_dir_all(&feat_wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }

        #[test]
        fn a_custom_prompt_template_reaches_the_agent() {
            let (root, feat_wt) = conflicting_repo("prompt");
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
                vec![item(&feat_wt)],
                |_| {},
            );
            assert_eq!(
                out_.landed,
                ["feat"],
                "the configured prompt template should have reached the agent"
            );
            let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            let _ = std::fs::remove_dir_all(&feat_wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }

        #[test]
        fn the_builtin_prompt_carries_no_sentinel() {
            // The negative control for the test above: with no configured
            // template the same agent finds no sentinel and never fixes anything,
            // so `landed` there really was caused by the override.
            let (root, feat_wt) = conflicting_repo("nosentinel");
            let agent = "printf '%s' \"$THEGN_TASK_PROMPT\" | grep -q MAGIC-SENTINEL && ";
            let mq = cfg(&format!("{agent}{FIXER}"), 1);

            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &mq,
                &Config::default(),
                &root,
                &db,
                vec![item(&feat_wt)],
                |_| {},
            );
            assert_eq!(out_.needs_human, ["feat"]);
            assert!(out_.landed.is_empty());
            let _ = std::fs::remove_dir_all(&root); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
            let _ = std::fs::remove_dir_all(&feat_wt); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        }
    }
}
