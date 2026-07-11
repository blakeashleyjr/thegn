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
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thegn_core::config::{ConflictHandoff, MergeQueueConfig};
use thegn_core::db::Db;
use thegn_core::store::WorktreeAuxStore;
use thegn_core::util;

use crate::integrate::{self, AttemptOutcome};

/// A worktree branch to drain, as read from the `merge_queue` cache.
#[derive(Debug, Clone)]
pub(crate) struct QueueItem {
    pub worktree: String,
    pub branch: String,
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
}

/// Why a branch didn't land — the material a fixing agent needs.
enum Failure {
    Conflict(Vec<String>),
    Gate(String),
}

/// Queue rows belonging to `root`'s repo (the queue is global; a drain is
/// per-repo because the target ref is). Shared by the CLI (`merge` namespace)
/// and the host's in-app drain so both see exactly one membership rule.
pub(crate) fn rows_for_repo(db: &Db, root: &Path) -> Vec<thegn_core::db::MergeQueueRow> {
    db.list_merge_queue()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| integrate::main_checkout(Path::new(&r.worktree)).as_deref() == Some(root))
        .collect()
}

/// Drain `items` one at a time, landing clean branches and dispatching the agent
/// on the rest. `progress` is invoked after each status write. Best-effort DB
/// writes (the DB is a cache; the git refs are the source of truth).
pub(crate) fn drive_queue(
    cfg: &MergeQueueConfig,
    repo_root: &Path,
    db: &Db,
    items: Vec<QueueItem>,
    mut progress: impl FnMut(&DriveStep),
) -> DriveOutcome {
    let mut out = DriveOutcome::default();
    let use_agent = cfg.conflict_handoff == ConflictHandoff::Agent && !cfg.agent_command.is_empty();
    let target = integrate::resolve_target(cfg, repo_root);

    for item in items {
        let set = |db: &Db, status: &str, oid: Option<&str>, detail: Option<&str>| {
            let _ = db.update_merge_status(&item.worktree, status, oid, detail, None);
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

        let mut agent_runs = 0u32;
        loop {
            let attempt = match integrate::attempt_land(cfg, repo_root, &item.branch) {
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
                AttemptOutcome::Landed { commit } => {
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
                AttemptOutcome::Conflict { paths } => Failure::Conflict(paths),
                AttemptOutcome::GateFailed { log } => Failure::Gate(log),
            };

            // A land failure. Dispatch the agent to fix it, if we still can.
            if use_agent && agent_runs < cfg.agent_max_attempts {
                agent_runs += 1;
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
                let _ = run_agent(cfg, &item.worktree, &item.branch, &target, &failure);
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
                    set(db, status, None, Some("breaks build"));
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

/// Compose the task prompt handed to the fixing agent. Kept pure (and unit-tested)
/// so the instructions the agent gets are stable and reviewable.
fn build_prompt(branch: &str, target: &str, failure: &Failure) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "You are resolving a merge-queue blocker for the git branch `{branch}`, \
         which must land onto `{target}`. You are already checked out in this \
         branch's worktree.\n\n"
    ));
    match failure {
        Failure::Conflict(paths) => {
            p.push_str(&format!(
                "Rebasing `{branch}` onto `{target}` produces merge conflicts in:\n"
            ));
            for path in paths {
                p.push_str(&format!("  - {path}\n"));
            }
            p.push_str(&format!(
                "\nRebase this branch onto the latest `{target}` and resolve every \
                 conflict, preserving the intent of both sides.\n"
            ));
        }
        Failure::Gate(log) => {
            p.push_str(&format!(
                "Merging `{branch}` onto `{target}` is clean, but the merged result \
                 fails the test gate. Gate output (tail):\n\n{}\n\n\
                 Fix the branch so the gate passes.\n",
                tail_line(log)
            ));
        }
    }
    p.push_str(
        "\nRules:\n\
         - Work only in this worktree; commit your fix on this branch.\n\
         - Do NOT push, and do NOT merge into or check out the target branch — \
           the merge queue lands it for you once this branch is clean.\n\
         - When done, ensure `git status` is clean (everything committed).\n",
    );
    p
}

/// Substitute the `{prompt}`/`{branch}`/`{target}` placeholders in the command
/// template, shell-quoting each so a prompt full of quotes/newlines is safe. The
/// template should use bare placeholders (`claude -p {prompt}`), not quoted ones.
fn substitute(template: &str, prompt: &str, branch: &str, target: &str) -> String {
    template
        .replace("{prompt}", &util::sh_quote(prompt))
        .replace("{branch}", &util::sh_quote(branch))
        .replace("{target}", &util::sh_quote(target))
}

/// Run the configured headless agent in the branch's worktree, to completion,
/// under a watchdog. Returns whether it exited zero. Output is captured (never
/// written to the terminal — this runs off the compositor loop). The git
/// environment is scrubbed so the agent's `git` operates on its own worktree.
// off-loop: the driver runs from the CLI (`merge queue drain`) or from
// spawn_drive's spawn_blocking — never on the event loop.
#[cfg(unix)]
#[expect(clippy::disallowed_methods)]
fn run_agent(
    cfg: &MergeQueueConfig,
    worktree: &str,
    branch: &str,
    target: &str,
    failure: &Failure,
) -> bool {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let prompt = build_prompt(branch, target, failure);
    let command = substitute(&cfg.agent_command, &prompt, branch, target);

    // Login shell so the agent (e.g. an npm-global `claude`) is on PATH with the
    // user's credentials, like an interactive agent pane.
    let mut cmd = Command::new(util::shell());
    cmd.arg("-lc")
        .arg(&command)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("THEGN_WORKTREE", worktree)
        .env("THEGN_BRANCH", branch)
        .env("THEGN_MERGE_PROMPT", &prompt)
        .env("THEGN_MERGE_TARGET", target)
        .process_group(0);
    // Defense in depth: the agent's git must target its cwd, not an inherited
    // GIT_DIR/GIT_INDEX_FILE (mirrors task.rs::build_capped_command).
    for var in util::GIT_ENV_VARS {
        cmd.env_remove(var);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "thegn::merge", error = %e, "merge queue: agent failed to spawn");
            return false;
        }
    };
    let pgid = child.id() as i32;

    // Watchdog: kill the process group if the agent overruns its deadline.
    let done = Arc::new(AtomicBool::new(false));
    let timed_out = Arc::new(AtomicBool::new(false));
    let watchdog = (cfg.agent_timeout_secs > 0).then(|| {
        let done = done.clone();
        let timed_out = timed_out.clone();
        let deadline = Duration::from_secs(cfg.agent_timeout_secs);
        std::thread::spawn(move || {
            let end = Instant::now() + deadline;
            while Instant::now() < end {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            if !done.load(Ordering::Relaxed) {
                timed_out.store(true, Ordering::Relaxed);
                nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pgid),
                    nix::sys::signal::Signal::SIGTERM,
                )
                .ok();
            }
        })
    });

    // Drain the pipes so a chatty agent can't deadlock on a full buffer.
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        if let Some(o) = stdout {
            let mut b = Vec::new();
            let _ = o.take(1 << 20).read_to_end(&mut b);
        }
    });
    let err_h = std::thread::spawn(move || {
        if let Some(e) = stderr {
            let mut b = Vec::new();
            let _ = e.take(1 << 20).read_to_end(&mut b);
        }
    });

    let status = child.wait();
    done.store(true, Ordering::Relaxed);
    if let Some(w) = watchdog {
        let _ = w.join();
    }
    let _ = out_h.join();
    let _ = err_h.join();

    if timed_out.load(Ordering::Relaxed) {
        tracing::warn!(target: "thegn::merge", branch, "merge queue: agent timed out");
        return false;
    }
    status.map(|s| s.success()).unwrap_or(false)
}

#[cfg(not(unix))]
fn run_agent(
    _cfg: &MergeQueueConfig,
    _worktree: &str,
    _branch: &str,
    _target: &str,
    _failure: &Failure,
) -> bool {
    false
}

/// The last non-empty line of a log (for a one-line status detail).
fn tail_line(log: &str) -> String {
    log.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_quotes_prompt_safely() {
        let cmd = substitute("claude -p {prompt}", "it's \"tricky\"\nline2", "b", "main");
        // Single-quoted; embedded single quotes are escaped, so the whole prompt
        // is one shell word regardless of quotes/newlines.
        assert!(cmd.starts_with("claude -p '"));
        assert!(
            cmd.contains("it'\\''s"),
            "single quote must be escaped: {cmd}"
        );
        assert!(cmd.contains("line2"));
    }

    #[test]
    fn substitute_fills_branch_and_target() {
        // Bare-word branch/target pass through unquoted (sh_quote readability).
        let cmd = substitute("run {branch} {target}", "p", "feat-x", "main");
        assert_eq!(cmd, "run feat-x main");
    }

    #[test]
    fn conflict_prompt_lists_paths_and_rules() {
        let p = build_prompt(
            "feat-x",
            "main",
            &Failure::Conflict(vec!["src/a.rs".into(), "src/b.rs".into()]),
        );
        assert!(p.contains("feat-x") && p.contains("main"));
        assert!(p.contains("src/a.rs") && p.contains("src/b.rs"));
        assert!(p.contains("Do NOT push"));
        assert!(p.contains("Rebase"));
    }

    #[test]
    fn gate_prompt_includes_log_tail_and_rules() {
        let p = build_prompt("feat-x", "main", &Failure::Gate("error: boom\n".into()));
        assert!(p.contains("fails the test gate"));
        assert!(p.contains("boom"));
        assert!(p.contains("Do NOT push"));
    }

    #[test]
    fn tail_line_picks_last_nonempty() {
        assert_eq!(tail_line("a\nb\n\n"), "b");
        assert_eq!(tail_line(""), "");
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
                "sz-drive-{tag}-{}-{}",
                std::process::id(),
                util::now()
            ));
            let feat_wt = root.with_extension("feat");
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&feat_wt);
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
                &root,
                &db,
                vec![QueueItem {
                    worktree: feat_wt.to_string_lossy().into(),
                    branch: "feat".into(),
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
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&feat_wt);
        }

        #[test]
        fn agent_that_cannot_fix_marks_needs_human() {
            let (root, feat_wt) = conflicting_repo("giveup");
            let before = out(&root, &["rev-parse", "main"]);
            // A no-op "agent" never resolves the conflict.
            let db = Db::open_memory().unwrap();
            let out_ = drive_queue(
                &cfg("true", 1),
                &root,
                &db,
                vec![QueueItem {
                    worktree: feat_wt.to_string_lossy().into(),
                    branch: "feat".into(),
                }],
                |_| {},
            );
            assert_eq!(out_.needs_human, ["feat"]);
            assert!(out_.landed.is_empty());
            assert_eq!(out(&root, &["rev-parse", "main"]), before, "main held");
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&feat_wt);
        }
    }
}
