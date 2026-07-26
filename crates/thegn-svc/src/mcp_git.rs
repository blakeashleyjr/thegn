//! `HouseGit` implementation — exposes thegn's git + semantic intelligence to
//! the embedded agent as MCP house tools. Lives in svc (where `GitBackend` is);
//! implements the `thegn_core::mcp::HouseGit` trait the core `McpRouter`
//! calls, inverting the core→svc layering boundary. Uses the CLI backend
//! (robust for on-demand agent calls; the gix fast path is for the hot loop).

use crate::git::{BranchOps, CliGit, CommitOps, GitBackend};
use std::path::Path;
use std::time::Duration;
use thegn_core::remote::GitLoc;
use thegn_core::{patch, semantic};

/// Wall-clock cap for a `gh` network read (pr/ci status). `gh` can hang for
/// minutes on a stalled HTTPS call — bounded here so a blackholed network can't
/// wedge the agent's MCP tool turn indefinitely.
const GH_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap for the raw `git diff` in `semantic_diff` (local, but a network-mounted
/// or wedged worktree could still stall it).
const GIT_DIFF_TIMEOUT: Duration = Duration::from_secs(60);

pub struct HouseGitImpl;

impl HouseGitImpl {
    fn loc(worktree: &str) -> GitLoc {
        GitLoc::for_worktree(Path::new(worktree))
    }

    /// Run the `gh` CLI in the worktree, returning stdout (text) or stderr (err).
    /// Bounded by [`GH_TIMEOUT`] so a hung network call maps to an `Err` the
    /// agent can see rather than an indefinitely-stalled tool turn.
    fn gh(worktree: &str, args: &[&str]) -> Result<String, String> {
        let mut cmd = std::process::Command::new("gh");
        cmd.args(args).current_dir(worktree);
        let out = output_deadline(cmd, GH_TIMEOUT).map_err(|e| format!("gh: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }
}

/// Run `cmd` to completion capturing full output, killing the child if it
/// outruns `timeout`. Both pipes drain on threads so a large output can't
/// deadlock the wait; the exit is polled with a short adaptive backoff. These
/// are on-demand MCP tool calls off the compositor loop, so the poll costs
/// nothing the UI can feel. (Local mirror of `git::output_bounded_with`, which
/// is private to that module.)
fn output_deadline(
    mut cmd: std::process::Command,
    timeout: Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut so = child.stdout.take().expect("piped stdout");
    let mut se = child.stderr.take().expect("piped stderr");
    let so_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let se_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let deadline = Instant::now() + timeout;
    let mut backoff = Duration::from_millis(1);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out after {}s (killed)", timeout.as_secs()),
            ));
        }
        std::thread::sleep(backoff.min(remaining));
        backoff = (backoff * 2).min(Duration::from_millis(25));
    };
    let stdout = so_h.join().unwrap_or_default();
    let stderr = se_h.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

impl thegn_core::mcp::HouseGit for HouseGitImpl {
    fn status(&self, worktree: &str) -> Result<String, String> {
        let files = CliGit
            .status(&Self::loc(worktree))
            .map_err(|e| e.to_string())?;
        if files.is_empty() {
            return Ok("working tree clean".to_string());
        }
        let mut s = String::new();
        for f in &files {
            // git porcelain-style XY columns (space = unmodified).
            s.push_str(&format!("{}{} {}\n", f.staged, f.unstaged, f.path));
        }
        Ok(s)
    }

    fn diff(&self, worktree: &str) -> Result<String, String> {
        let entries = CliGit
            .diff_files(&Self::loc(worktree), "HEAD")
            .map_err(|e| e.to_string())?;
        if entries.is_empty() {
            return Ok("no changes vs HEAD".to_string());
        }
        let mut s = String::new();
        for e in &entries {
            s.push_str(&format!("+{:<5} -{:<5} {}\n", e.added, e.deleted, e.path));
        }
        Ok(s)
    }

    fn branches(&self, worktree: &str) -> Result<String, String> {
        let branches = CliGit
            .branches(&Self::loc(worktree))
            .map_err(|e| e.to_string())?;
        let mut s = String::new();
        for b in &branches {
            s.push_str(&format!(
                "{} {}\n",
                if b.is_head { "*" } else { " " },
                b.name
            ));
        }
        Ok(s)
    }

    fn semantic_diff(&self, worktree: &str) -> Result<String, String> {
        // Raw unified diff vs HEAD → core::patch parse → per-file entity changes
        // → impact summary + suggested commit message (core::semantic).
        let mut cmd = thegn_core::util::git_cmd(Path::new(worktree));
        cmd.args(["diff", "--no-color", "HEAD"]);
        let out = output_deadline(cmd, GIT_DIFF_TIMEOUT).map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        let diff = String::from_utf8_lossy(&out.stdout);
        let files = patch::parse_patch(&diff);
        if files.is_empty() {
            return Ok("no changes vs HEAD".to_string());
        }
        let mut per_file: Vec<(String, Vec<semantic::EntityChange>)> = Vec::new();
        for pf in &files {
            let Some(lang) = semantic::Lang::from_path(&pf.new_path) else {
                continue; // unsupported language — skip from the semantic view
            };
            let Ok(src) = std::fs::read_to_string(Path::new(worktree).join(&pf.new_path)) else {
                continue; // deleted/binary/unreadable
            };
            let changes = semantic::entities_for_diff(&src, lang, &pf.hunks);
            if !changes.is_empty() {
                per_file.push((pf.new_path.clone(), changes));
            }
        }
        if per_file.is_empty() {
            return Ok("changes vs HEAD touch no recognizable code entities".to_string());
        }
        let impact = semantic::impact_summary(&per_file);
        let commit = semantic::derive_commit_message(&per_file);
        let mut s = format!("{}\n", impact.summary);
        for (file, changes) in &per_file {
            s.push_str(&format!("\n{file}:\n"));
            for c in changes {
                s.push_str(&format!(
                    "  {} {} (+{} -{})\n",
                    c.kind.label(),
                    c.name,
                    c.added,
                    c.deleted
                ));
            }
        }
        s.push_str(&format!("\nsuggested commit message:\n{commit}\n"));
        Ok(s)
    }
}

impl thegn_core::mcp::HouseForge for HouseGitImpl {
    fn pr_status(&self, worktree: &str) -> Result<String, String> {
        Self::gh(worktree, &["pr", "status"])
    }

    fn pr_list(&self, worktree: &str) -> Result<String, String> {
        Self::gh(worktree, &["pr", "list", "--limit", "30"])
    }

    fn ci_runs(&self, worktree: &str) -> Result<String, String> {
        Self::gh(worktree, &["run", "list", "--limit", "10"])
    }

    fn create_branch(&self, worktree: &str, name: &str, base: &str) -> Result<String, String> {
        CliGit
            .create_branch(&Self::loc(worktree), name, base)
            .map(|_| format!("created branch {name} from {base}"))
            .map_err(|e| e.to_string())
    }

    fn commit(&self, worktree: &str, message: &str) -> Result<String, String> {
        CliGit
            .commit(&Self::loc(worktree), message, false, None)
            .map(|_| format!("committed: {message}"))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::mcp::{HouseForge, HouseGit};

    #[test]
    fn output_deadline_kills_a_hung_child() {
        // `sleep 30` would block the tool call indefinitely without the
        // deadline; the helper must SIGKILL it and return a TimedOut error long
        // before the sleep elapses.
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        let start = std::time::Instant::now();
        let r = output_deadline(cmd, Duration::from_millis(200));
        let err = r.expect_err("expected a timeout error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "helper waited far longer than the deadline: {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn output_deadline_returns_output_when_the_child_finishes() {
        let mut cmd = std::process::Command::new("printf");
        cmd.arg("hello");
        let out = output_deadline(cmd, Duration::from_secs(10)).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    }

    // Route test git through the scrubbing helper rather than a raw git command,
    // matching the repo invariant the lint guardrail enforces.
    fn git(dir: &Path, args: &[&str]) {
        let ok = thegn_core::util::git_cmd(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn house_git_reports_status_diff_and_semantic() {
        let dir = std::env::temp_dir().join(format!("sz-housegit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wt = dir.to_str().unwrap();

        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("lib.rs"), "fn alpha() {}\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);
        // Unstaged change: add a function.
        std::fs::write(
            dir.join("lib.rs"),
            "fn alpha() {}\nfn beta() {\n    let x = 1;\n}\n",
        )
        .unwrap();

        let h = HouseGitImpl;
        let st = h.status(wt).unwrap();
        assert!(st.contains("lib.rs"), "status missing file: {st}");
        let d = h.diff(wt).unwrap();
        assert!(d.contains("lib.rs"), "diff missing file: {d}");
        // Semantic view names the newly-added entity + suggests a commit message.
        let s = h.semantic_diff(wt).unwrap();
        assert!(s.contains("beta"), "semantic missing new entity: {s}");
        assert!(
            s.to_lowercase().contains("commit message"),
            "semantic missing commit msg: {s}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn house_forge_branch_and_commit() {
        let dir = std::env::temp_dir().join(format!("sz-houseforge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wt = dir.to_str().unwrap();
        git(&dir, &["init", "-q"]);
        git(&dir, &["config", "user.email", "t@t"]);
        git(&dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "init"]);

        let h = HouseGitImpl;
        // create_branch off HEAD, then commit a staged change.
        h.create_branch(wt, "feature/x", "HEAD").unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        git(&dir, &["add", "."]);
        let out = h.commit(wt, "add line two").unwrap();
        assert!(out.contains("add line two"), "commit out: {out}");
        // The commit landed.
        let log = thegn_core::util::git_cmd(&dir)
            .args(["log", "--oneline", "-1"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).contains("add line two"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
