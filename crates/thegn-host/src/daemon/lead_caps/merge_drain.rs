//! `merge.drain` (THE-2172): start the merge-queue drain for a worktree's
//! repo. Spawns only on request.
//!
//! The drain itself is the CLI (`thegn merge drain --json`) run as a child of
//! the daemon, inside the `thegn.slice` CPU cap ([`wrap_background_argv`]): it
//! runs a full gate, so it must never run uncapped or on the event loop. One
//! named thread owns the child from spawn to `wait` (a dropped `Child` is a
//! zombie: THE-605/448/701), parses its JSON summary, and writes one
//! `queue_landed` / `queue_held` notification.
//!
//! Single-flight per repo: an in-process set of repos with a drain this daemon
//! started, plus a probe of the queue's own merge claims so a drain started
//! elsewhere (the CLI, another daemon) is refused too.
//!
//! The stage executor's unattended land (b155) starts the same drain through
//! [`start_for_repo`] with [`DrainEnv::Scrubbed`]: the gate runs
//! worker-written code, so agent sockets and every forge, tracker and model
//! credential ([`SCRUBBED_ENV`]) are removed from the child's environment.
//!
//! [`wrap_background_argv`]: thegn_core::sandbox_cpucap::wrap_background_argv

use super::{JsonResult, decode, encode};
use crate::daemon::service::DaemonService;
use serde_json::json;
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use thegn_core::db::Db;
use thegn_core::store::{MergeClaimView, NotificationStore, WorkspaceStore, WorktreeAuxStore};
use thegn_svc::control::{ControlError, ControlResult};

/// Repos with a drain this daemon started and has not yet reaped.
static DRAINS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

/// How much of the child's stdout is kept for parsing; the rest is drained to
/// keep the pipe from filling and wedging the child.
const MAX_REPORT_BYTES: u64 = 1 << 20;
/// The longest summary notification message (characters).
const MAX_MESSAGE: usize = 300;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DrainRequest {
    worktree: String,
}

/// RAII membership in [`DRAINS`]: removed on drop, so every return path and a
/// panicking owner thread release the repo.
#[derive(Debug)]
struct DrainGuard {
    set: &'static Mutex<HashSet<PathBuf>>,
    repo: PathBuf,
}

impl DrainGuard {
    fn claim(set: &'static Mutex<HashSet<PathBuf>>, repo: &Path) -> Result<Self, String> {
        // best-effort: a poisoned set only holds paths, so keep using it
        let mut held = set.lock().unwrap_or_else(|e| e.into_inner());
        if !held.insert(repo.to_path_buf()) {
            return Err(already_running(repo));
        }
        Ok(Self {
            set,
            repo: repo.to_path_buf(),
        })
    }
}

impl Drop for DrainGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.repo);
    }
}

fn already_running(repo: &Path) -> String {
    format!("a drain is already running for {}", repo.display())
}

/// Pure: is any queue row of the repo held by a live merge claim (a drain
/// running somewhere else)? `Some(refusal)` when so.
fn claim_refusal(repo: &Path, claims: &[MergeClaimView], now_ms: i64) -> Option<String> {
    claims
        .iter()
        .any(|view| {
            view.phase != "unclaimed" && view.lease_until_ms.is_some_and(|until| until > now_ms)
        })
        .then(|| already_running(repo))
}

/// Registry confinement for a LOCAL worktree, mirroring the daemon's
/// `confine_worktree`: an exact match, else a canonical-path match against
/// rows marked local. Remote rows never trigger host filesystem access.
fn registered_local<F>(
    rows: &[thegn_core::models::WorktreeRow],
    requested: &str,
    canonicalize: F,
) -> bool
where
    F: Fn(&str) -> PathBuf,
{
    let is_local = |row: &thegn_core::models::WorktreeRow| {
        let location = row.location.trim();
        location.is_empty() || location == "local"
    };
    if rows
        .iter()
        .any(|row| row.worktree == requested && is_local(row))
    {
        return true;
    }
    let want = canonicalize(requested);
    rows.iter()
        .filter(|row| is_local(row))
        .any(|row| canonicalize(&row.worktree) == want)
}

/// Answer `merge.drain`: `{started: true, repo, pid}` once the drain child is
/// running; refusal when the worktree is unregistered or remote, or a drain is
/// already running for its repo.
pub(crate) async fn drain(svc: &DaemonService, req: serde_json::Value) -> JsonResult {
    let DrainRequest { worktree } = decode(req)?;
    if worktree.trim().is_empty() {
        return Err(ControlError::InvalidArgument("worktree is empty".into()));
    }
    let lookup = worktree.clone();
    let (remote, known) = svc
        .with_db(move |db| {
            let remote = crate::merge_ops::registered_remote_worktree(db, &lookup)?.is_some();
            let rows = db.worktrees()?;
            Ok((
                remote,
                registered_local(&rows, &lookup, |path| {
                    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
                }),
            ))
        })
        .await?;
    if remote {
        return Err(ControlError::FailedPrecondition(
            "merge.drain runs on the host that owns the queue; this worktree is remote".into(),
        ));
    }
    if !known {
        return Err(ControlError::NotFound(format!(
            "worktree not registered with thegn: {}",
            worktree.chars().take(256).collect::<String>()
        )));
    }

    let db = svc.db.clone();
    let events = svc.events.clone();
    let (repo, pid) = tokio::task::spawn_blocking(move || {
        let repo = crate::merge_ops::repo_root_of(Path::new(&worktree)).ok_or_else(|| {
            ControlError::FailedPrecondition(format!("{worktree}: not inside a git repository"))
        })?;
        let pid = start(db, events, &repo, worktree, DrainEnv::Inherit)?;
        Ok::<_, ControlError>((repo, pid))
    })
    .await
    .map_err(|e| ControlError::Internal(anyhow::anyhow!("merge.drain task join: {e}")))??;
    encode(&json!({ "started": true, "repo": repo.to_string_lossy(), "pid": pid }))
}

/// The drain child's environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainEnv {
    /// The daemon's environment, as `merge.drain` always ran.
    Inherit,
    /// The daemon's environment minus [`SCRUBBED_ENV`]: the stage executor's
    /// unattended land (b155), whose gate runs worker-written code.
    Scrubbed,
}

/// What a scrubbed drain child never sees: agent/keyring sockets, forge and
/// tracker tokens, and model keys. The land is local only (it never pushes),
/// so none of these is needed.
pub(crate) const SCRUBBED_ENV: &[&str] = &[
    "SSH_AUTH_SOCK",
    "DBUS_SESSION_BUS_ADDRESS",
    "GPG_AGENT_INFO",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GITLAB_TOKEN",
    "GLAB_TOKEN",
    "LINEAR_API_KEY",
    "JIRA_API_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
];

/// Start the drain for `repo_root` from inside the daemon (the stage
/// executor's land). Blocking (it probes the queue's claims, which may run
/// git): call it off the event loop. Same single-flight as `merge.drain`.
pub(crate) fn start_for_repo(
    svc: &DaemonService,
    repo_root: &Path,
    env: DrainEnv,
) -> ControlResult<u32> {
    start(
        svc.db.clone(),
        svc.events.clone(),
        repo_root,
        repo_root.to_string_lossy().into_owned(),
        env,
    )
}

/// Off the event loop: take the repo's single-flight slot, probe foreign
/// claims, and hand the slot to the owner thread that spawns the child.
fn start(
    db: crate::daemon::service::SharedDb,
    events: super::feed::EventsTx,
    repo: &Path,
    worktree: String,
    env: DrainEnv,
) -> Result<u32, ControlError> {
    let repo = repo.to_path_buf();
    let guard = DrainGuard::claim(&DRAINS, &repo).map_err(ControlError::Conflict)?;
    // A fresh handle (like `merge_add`): `rows_for_repo` may run git, which
    // must not happen under the daemon's shared DB lock.
    let claims = {
        let fresh = Db::open().map_err(ControlError::Internal)?;
        let rows =
            crate::merge_driver::rows_for_repo(&fresh, &repo).map_err(ControlError::Internal)?;
        let mut claims = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(view) = fresh
                .observe_merge_claim(&row.worktree)
                .map_err(ControlError::Internal)?
            {
                claims.push(view);
            }
        }
        claims
    };
    if let Some(refusal) = claim_refusal(&repo, &claims, thegn_core::util::now_ms()) {
        return Err(ControlError::Conflict(refusal));
    }

    let exe = std::env::current_exe()
        .map_err(|e| ControlError::Internal(anyhow::anyhow!("current_exe: {e}")))?;
    let argv = vec![
        exe.to_string_lossy().into_owned(),
        "merge".into(),
        "drain".into(),
        "--json".into(),
    ];
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<u32, String>>(1);
    let owner_repo = repo.clone();
    thegn_core::thread_name::try_spawn_named("thegn-mqdrain", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
        // Held for the whole run; released on every path, panic included.
        let _guard = guard;
        own_drain(&db, &events, &owner_repo, &worktree, argv, env, &tx);
    })
    .map_err(|e| ControlError::Unavailable(format!("merge.drain thread: {e}")))?;
    match rx.recv() {
        Ok(Ok(pid)) => Ok(pid),
        Ok(Err(error)) => Err(ControlError::Internal(anyhow::anyhow!(
            "merge drain failed to start: {error}"
        ))),
        Err(_) => Err(ControlError::Internal(anyhow::anyhow!(
            "merge drain owner thread exited before starting the child"
        ))),
    }
}

/// The drain child's command: the (already capped) argv, run in the repo,
/// stdin closed, stdout piped for the report, with [`SCRUBBED_ENV`] removed
/// when `env` is [`DrainEnv::Scrubbed`].
fn drain_command(argv: &[String], repo: &Path, env: DrainEnv) -> std::process::Command {
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir(repo)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if env == DrainEnv::Scrubbed {
        for var in SCRUBBED_ENV {
            command.env_remove(var);
        }
    }
    command
}

/// The owner thread: spawn the capped child, report its pid, read its report,
/// ALWAYS `wait` it, then record one summary notification.
fn own_drain(
    db: &crate::daemon::service::SharedDb,
    events: &super::feed::EventsTx,
    repo: &Path,
    worktree: &str,
    argv: Vec<String>,
    env: DrainEnv,
    tx: &std::sync::mpsc::SyncSender<Result<u32, String>>,
) {
    let argv = thegn_core::sandbox_cpucap::wrap_background_argv(argv);
    let mut command = drain_command(&argv, repo, env);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if tx.send(Err(error.to_string())).is_err() {
                tracing::debug!(target: "thegn::daemon", "merge.drain requester gone before spawn failure was reported");
            }
            return;
        }
    };
    if tx.send(Ok(child.id())).is_err() {
        tracing::debug!(target: "thegn::daemon", "merge.drain requester gone before the pid was reported");
    }

    let mut report = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        if let Err(error) = (&mut stdout)
            .take(MAX_REPORT_BYTES)
            .read_to_end(&mut report)
        {
            tracing::debug!(target: "thegn::daemon", "merge.drain report read failed: {error}");
        }
        // Keep reading past the cap so a chatty child never blocks on a full
        // pipe (and so never reaches the `wait` below).
        if let Err(error) = std::io::copy(&mut stdout, &mut std::io::sink()) {
            tracing::debug!(target: "thegn::daemon", "merge.drain stdout drain failed: {error}");
        }
    }
    #[expect(
        clippy::disallowed_methods,
        reason = "the dedicated thegn-mqdrain owner thread reaps its own child; off the event loop"
    )]
    let status = child.wait();
    let exit = match &status {
        Ok(status) => status
            .code()
            .map_or_else(|| "signal".to_string(), |c| c.to_string()),
        Err(error) => format!("wait failed: {error}"),
    };
    let summary = summarize(&String::from_utf8_lossy(&report), &exit);
    let kind = if summary.landed > 0 {
        "queue_landed"
    } else {
        "queue_held"
    };
    let message = summary.message(repo);
    let source = format!("merge-drain:{}", repo.display());
    let id = {
        let db = db.lock().unwrap_or_else(|e| e.into_inner());
        db.put_notification(kind, &source, &message, worktree)
    };
    match id {
        Ok(id) => super::feed::notification(events, id),
        Err(error) => {
            tracing::warn!(target: "thegn::daemon", "merge.drain summary notification failed: {error:#}");
        }
    }
}

/// What a finished drain reported.
#[derive(Debug, Default, PartialEq, Eq)]
struct DrainSummary {
    /// Whether a JSON report could be read at all.
    parsed: bool,
    landed: usize,
    deferred: usize,
    needs_human: usize,
    exit: String,
}

impl DrainSummary {
    fn message(&self, repo: &Path) -> String {
        let name = repo.file_name().map_or_else(
            || repo.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let text = if self.parsed {
            format!(
                "merge drain {name}: {} landed, {} deferred, {} need a human",
                self.landed, self.deferred, self.needs_human
            )
        } else {
            format!("merge drain {name}: no report (exit {})", self.exit)
        };
        text.chars()
            .filter(|c| !c.is_control())
            .take(MAX_MESSAGE)
            .collect()
    }
}

/// Pure: read the drain's `--json` document (the last parseable object line,
/// or the whole output) into counts.
fn summarize(stdout: &str, exit: &str) -> DrainSummary {
    let doc = serde_json::from_str::<serde_json::Value>(stdout.trim())
        .ok()
        .filter(serde_json::Value::is_object)
        .or_else(|| {
            stdout.lines().rev().find_map(|line| {
                serde_json::from_str::<serde_json::Value>(line.trim())
                    .ok()
                    .filter(serde_json::Value::is_object)
            })
        });
    let count = |doc: &serde_json::Value, key: &str| {
        doc.get(key)
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len)
    };
    match doc {
        Some(doc) => DrainSummary {
            parsed: true,
            landed: count(&doc, "landed"),
            deferred: count(&doc, "deferred"),
            needs_human: count(&doc, "needs_human"),
            exit: exit.to_string(),
        },
        None => DrainSummary {
            exit: exit.to_string(),
            ..DrainSummary::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_SET: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

    #[test]
    fn a_second_drain_for_the_same_repo_is_refused_until_the_first_ends() {
        let repo = Path::new("/repo/single-flight");
        let first = DrainGuard::claim(&TEST_SET, repo).expect("first drain claims");
        let refused = DrainGuard::claim(&TEST_SET, repo).expect_err("second refused");
        assert_eq!(
            refused,
            "a drain is already running for /repo/single-flight"
        );
        // Another repo is independent.
        let other = DrainGuard::claim(&TEST_SET, Path::new("/repo/other")).expect("other repo");
        drop(first);
        let again = DrainGuard::claim(&TEST_SET, repo).expect("released on drop");
        drop((again, other));
    }

    #[test]
    fn the_guard_is_released_when_its_owner_panics() {
        let repo = Path::new("/repo/panicking-owner");
        let guard = DrainGuard::claim(&TEST_SET, repo).unwrap();
        let joined = std::thread::spawn(move || {
            let _guard = guard;
            panic!("owner thread died");
        })
        .join();
        assert!(joined.is_err());
        assert!(DrainGuard::claim(&TEST_SET, repo).is_ok());
    }

    fn view(phase: &str, lease_until_ms: Option<i64>) -> MergeClaimView {
        MergeClaimView {
            worktree: "/repo/wt".into(),
            generation: 1,
            phase: phase.into(),
            lease_until_ms,
            owner_hint: None,
        }
    }

    #[test]
    fn a_live_foreign_claim_refuses_the_drain() {
        let repo = Path::new("/repo");
        assert_eq!(claim_refusal(repo, &[], 1_000), None);
        assert_eq!(
            claim_refusal(repo, &[view("unclaimed", Some(9_999))], 1_000),
            None
        );
        assert_eq!(
            claim_refusal(repo, &[view("folding", Some(500))], 1_000),
            None
        );
        assert_eq!(claim_refusal(repo, &[view("folding", None)], 1_000), None);
        assert_eq!(
            claim_refusal(repo, &[view("folding", Some(2_000))], 1_000),
            Some("a drain is already running for /repo".into())
        );
    }

    fn row(worktree: &str, location: &str) -> thegn_core::models::WorktreeRow {
        let db = Db::open_memory().unwrap();
        db.put_worktree("t", "/repo", worktree, "b", Some(location), None)
            .unwrap();
        db.worktrees().unwrap().remove(0)
    }

    #[test]
    fn an_unregistered_worktree_is_refused() {
        let rows = vec![
            row("/repo/wt", "local"),
            row("/remote/wt", r#"{"kind":"provider"}"#),
        ];
        let identity = |p: &str| PathBuf::from(p.trim_end_matches('/'));
        assert!(registered_local(&rows, "/repo/wt", identity));
        assert!(registered_local(&rows, "/repo/wt/", identity));
        assert!(!registered_local(&rows, "/elsewhere", identity));
        // A remote row never confines a local path, even on an exact match.
        assert!(!registered_local(&rows, "/remote/wt", identity));
    }

    #[test]
    fn a_scrubbed_drain_removes_every_listed_variable() {
        let argv = vec!["thegn".to_string(), "merge".into(), "drain".into()];
        let scrubbed = drain_command(&argv, Path::new("/repo"), DrainEnv::Scrubbed);
        let removed: Vec<_> = scrubbed
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        for var in SCRUBBED_ENV {
            assert!(removed.iter().any(|k| k == var), "{var} not removed");
        }
        for must in [
            "SSH_AUTH_SOCK",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "LINEAR_API_KEY",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
        ] {
            assert!(SCRUBBED_ENV.contains(&must), "{must}");
        }
        assert_eq!(scrubbed.get_program(), "thegn");
        assert_eq!(scrubbed.get_current_dir(), Some(Path::new("/repo")));

        let inherit = drain_command(&argv, Path::new("/repo"), DrainEnv::Inherit);
        assert_eq!(inherit.get_envs().count(), 0, "inherit touches nothing");
    }

    #[test]
    fn summarize_counts_the_report_lists() {
        let out = r#"{"target":"main","landed":["a","b"],"deferred":["c"],"needs_human":[]}"#;
        let summary = summarize(out, "0");
        assert!(summary.parsed);
        assert_eq!(
            (summary.landed, summary.deferred, summary.needs_human),
            (2, 1, 0)
        );
        assert_eq!(
            summary.message(Path::new("/code/thegn")),
            "merge drain thegn: 2 landed, 1 deferred, 0 need a human"
        );
        let noisy = format!("Warning: x\n{out}\n");
        assert_eq!(summarize(&noisy, "0").landed, 2);
    }

    #[test]
    fn summarize_without_a_report_is_held() {
        let summary = summarize("boom", "1");
        assert!(!summary.parsed);
        assert_eq!(summary.landed, 0);
        assert_eq!(
            summary.message(Path::new("/r/x")),
            "merge drain x: no report (exit 1)"
        );
        let long = DrainSummary {
            exit: "e".repeat(1_000),
            ..DrainSummary::default()
        };
        assert_eq!(long.message(Path::new("/r/x")).chars().count(), MAX_MESSAGE);
    }
}
