//! `HouseGit` implementation — exposes superzej's git + semantic intelligence to
//! the embedded agent as MCP house tools. Lives in svc (where `GitBackend` is);
//! implements the `superzej_core::mcp::HouseGit` trait the core `McpRouter`
//! calls, inverting the core→svc layering boundary. Uses the CLI backend
//! (robust for on-demand agent calls; the gix fast path is for the hot loop).

use crate::git::{CliGit, GitBackend};
use std::path::Path;
use superzej_core::remote::GitLoc;
use superzej_core::{patch, semantic};

pub struct HouseGitImpl;

impl HouseGitImpl {
    fn loc(worktree: &str) -> GitLoc {
        GitLoc::for_worktree(Path::new(worktree))
    }
}

impl superzej_core::mcp::HouseGit for HouseGitImpl {
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
        let out = superzej_core::util::git_cmd(Path::new(worktree))
            .args(["diff", "--no-color", "HEAD"])
            .output()
            .map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use superzej_core::mcp::HouseGit;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
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
}
