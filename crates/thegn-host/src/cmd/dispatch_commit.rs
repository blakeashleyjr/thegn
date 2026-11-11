//! `thegn dispatch commit <row> --msg-file F [--trailer "Key: value"]...` (THE-1972).
//!
//! After a worker exits its changes sit uncommitted in the lane worktree. This
//! verb stages everything, commits with the given message plus trailers, and —
//! when a formatter pre-commit hook rewrote files and so failed the first
//! attempt — re-stages and retries exactly once. It never passes `--no-verify`.

use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use thegn_core::commit_msg::{validate_trailer, with_trailers};
use thegn_core::db::Db;
use thegn_core::git_run::{GitOutcome, GitPolicy, git_run};
use thegn_core::issue::AgentDispatchStatus;
use thegn_core::outln;
use thegn_core::store::NotificationStore;

/// Hooks (formatters) can be slow; bound the whole git call, not just reads.
const COMMIT_GIT_DEADLINE: Duration = Duration::from_secs(900);

static NO_CANCEL: AtomicBool = AtomicBool::new(false);

/// What one `dispatch commit` did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CommitOutcome {
    pub committed: bool,
    pub sha: Option<String>,
    pub retried: bool,
}

fn git(worktree: &Path, args: &[&str]) -> GitOutcome {
    git_run(
        worktree,
        args,
        &GitPolicy::read(COMMIT_GIT_DEADLINE),
        &NO_CANCEL,
    )
}

fn git_ok(worktree: &Path, args: &[&str]) -> Result<()> {
    let outcome = git(worktree, args);
    if outcome.is_success() {
        Ok(())
    } else {
        bail!("git {}: {outcome}", args.first().copied().unwrap_or(""))
    }
}

/// Pure row validation: the worktree must be set, the worker finished, and
/// the directory still present.
fn check_row(id: i64, worktree_path: &str, status: AgentDispatchStatus) -> Result<&Path> {
    if worktree_path.trim().is_empty() {
        bail!("dispatch {id} has no worktree");
    }
    if matches!(
        status,
        AgentDispatchStatus::Spawning | AgentDispatchStatus::Running
    ) {
        bail!(
            "dispatch {id} is still {}; wait for the worker to exit before committing",
            status.as_str()
        );
    }
    let worktree = Path::new(worktree_path);
    if !worktree.is_dir() {
        bail!("dispatch {id}: worktree {} is gone", worktree.display());
    }
    Ok(worktree)
}

pub(crate) fn run(id: i64, msg_file: &Path, trailers: &[String], json: bool) -> Result<()> {
    let db = Db::open()?;
    let row = db
        .get_dispatch(id)?
        .ok_or_else(|| anyhow::anyhow!("no dispatch with id {id}"))?;
    let worktree = check_row(id, &row.worktree_path, row.status)?;
    let outcome = commit_in(worktree, msg_file, trailers)?;
    if json {
        outln!(
            "{}",
            serde_json::json!({
                "committed": outcome.committed,
                "sha": outcome.sha,
                "retried": outcome.retried,
            })
        );
    } else if let Some(sha) = &outcome.sha {
        outln!("{sha}");
    } else {
        outln!("nothing to commit");
    }
    Ok(())
}

/// Stage everything in `worktree` and commit it; see the module docs.
pub(crate) fn commit_in(
    worktree: &Path,
    msg_file: &Path,
    trailers: &[String],
) -> Result<CommitOutcome> {
    let raw = std::fs::read_to_string(msg_file)
        .with_context(|| format!("reading message file {}", msg_file.display()))?;
    if raw.trim().is_empty() {
        bail!("message file {} is empty", msg_file.display());
    }
    let trailers = trailers
        .iter()
        .map(|t| validate_trailer(t).map_err(anyhow::Error::msg))
        .collect::<Result<Vec<_>>>()?;
    let message = with_trailers(&raw, &trailers);
    let mut tmp = tempfile::NamedTempFile::new().context("creating temp message file")?;
    tmp.write_all(message.as_bytes())
        .context("writing temp message file")?;
    tmp.flush().context("writing temp message file")?;
    let tmp_path = tmp.path().to_string_lossy().into_owned();

    let _scope = thegn_core::util::GitWriteScope::begin();
    git_ok(worktree, &["add", "-A"])?;
    // `diff --cached --quiet` exits 1 when something is staged.
    match git(worktree, &["diff", "--cached", "--quiet"]) {
        GitOutcome::Exit { code: Some(1), .. } => {}
        o if o.is_success() => {
            return Ok(CommitOutcome {
                committed: false,
                sha: None,
                retried: false,
            });
        }
        other => bail!("git diff --cached: {other}"),
    }

    let commit_args = ["commit", "-q", "-F", tmp_path.as_str()];
    let mut retried = false;
    let first = git(worktree, &commit_args);
    if !first.is_success() {
        // `add -A` just staged everything, so unstaged tracked changes now
        // mean a hook rewrote files.
        let rewritten = matches!(git(worktree, &["diff", "--name-only"]), GitOutcome::Ok(_));
        if !rewritten {
            bail!("git commit failed: {first}");
        }
        retried = true;
        git_ok(worktree, &["add", "-A"])?;
        let second = git(worktree, &commit_args);
        if !second.is_success() {
            bail!("git commit failed after re-staging hook changes: {second}");
        }
    }
    let sha = git(worktree, &["rev-parse", "--short", "HEAD"])
        .into_text()
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow::anyhow!("git rev-parse: {e}"))?;
    Ok(CommitOutcome {
        committed: true,
        sha: Some(sha),
        retried,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_git(dir: &Path, args: &[&str]) -> String {
        let out = thegn_core::util::git_cmd(dir)
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        sh_git(&root, &["init", "-q"]);
        sh_git(&root, &["config", "user.email", "t@t"]);
        sh_git(&root, &["config", "user.name", "t"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        sh_git(&root, &["add", "-A"]);
        sh_git(&root, &["commit", "-q", "-m", "init"]);
        (dir, root)
    }

    fn msg(dir: &Path, text: &str) -> std::path::PathBuf {
        let p = dir.join("msg.txt");
        std::fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn dispatch_commit_check_row_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().to_string_lossy().into_owned();
        let e = check_row(1, "  ", AgentDispatchStatus::Spawning).unwrap_err();
        assert!(e.to_string().contains("no worktree"), "{e}");
        for st in [AgentDispatchStatus::Spawning, AgentDispatchStatus::Running] {
            let e = check_row(2, &p, st).unwrap_err();
            assert!(e.to_string().contains("still"), "{e}");
        }
        let gone = dir.path().join("missing").to_string_lossy().into_owned();
        let e = check_row(3, &gone, AgentDispatchStatus::Spawning).unwrap_err();
        assert!(e.to_string().contains("still"), "{e}");
        let st = AgentDispatchStatus::Done;
        let e = check_row(4, &gone, st).unwrap_err();
        assert!(e.to_string().contains("gone"), "{e}");
        assert_eq!(check_row(5, &p, st).unwrap(), dir.path());
    }

    #[test]
    fn dispatch_commit_nothing_to_commit() {
        let _state = crate::testenv::IsolatedState::new();
        let (dir, root) = repo();
        let m = msg(dir.path(), "feat: x\n");
        let out = commit_in(&root, &m, &[]).unwrap();
        assert_eq!(
            out,
            CommitOutcome {
                committed: false,
                sha: None,
                retried: false
            }
        );
    }

    #[test]
    fn dispatch_commit_plain_with_trailer() {
        let _state = crate::testenv::IsolatedState::new();
        let (dir, root) = repo();
        std::fs::write(root.join("b.txt"), "new\n").unwrap();
        let m = msg(dir.path(), "feat: x\n");
        let out = commit_in(&root, &m, &["Claude-Session: s1".into()]).unwrap();
        assert!(out.committed && !out.retried && out.sha.is_some());
        let body = sh_git(&root, &["log", "-1", "--format=%B"]);
        assert!(body.contains("feat: x") && body.contains("Claude-Session: s1"));
    }

    #[test]
    fn dispatch_commit_retries_once_after_hook_rewrite() {
        let _state = crate::testenv::IsolatedState::new();
        let (dir, root) = repo();
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            "#!/bin/sh\nif [ ! -f .git/hook-ran ]; then\n  touch .git/hook-ran\n  echo formatted >> a.txt\n  exit 1\nfi\nexit 0\n",
        )
        .unwrap();
        crate::managed_tool::make_executable(&hook).unwrap();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        let m = msg(dir.path(), "feat: y\n");
        let out = commit_in(&root, &m, &["Claude-Session: s2".into()]).unwrap();
        assert!(out.committed && out.retried);
        let body = sh_git(&root, &["log", "-1", "--format=%B"]);
        assert!(body.contains("Claude-Session: s2"));
        assert!(sh_git(&root, &["status", "--porcelain"]).trim().is_empty());
    }

    #[test]
    fn dispatch_commit_second_failure_surfaces() {
        let _state = crate::testenv::IsolatedState::new();
        let (dir, root) = repo();
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\necho x >> a.txt\nexit 1\n").unwrap();
        crate::managed_tool::make_executable(&hook).unwrap();
        std::fs::write(root.join("a.txt"), "changed\n").unwrap();
        let m = msg(dir.path(), "feat: z\n");
        assert!(commit_in(&root, &m, &[]).is_err());
    }

    #[test]
    fn dispatch_commit_refuses_empty_message_and_bad_trailer() {
        let _state = crate::testenv::IsolatedState::new();
        let (dir, root) = repo();
        let empty = msg(dir.path(), "  \n");
        assert!(commit_in(&root, &empty, &[]).is_err());
        let m = msg(dir.path(), "feat: x\n");
        assert!(commit_in(&root, &m, &["bad trailer".into()]).is_err());
    }
}
