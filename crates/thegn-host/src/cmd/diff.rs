//! `thegn diff` — syntax-highlighted (syntect) diff of a worktree against its
//! branch point. Range = everything since the merge-base with the resolved base
//! branch, so it shows "what this branch changes," including uncommitted work.

use anyhow::{Context, Result, bail};
use std::io::Write;
use thegn_core::config::{Config, StructuralDiff};
use thegn_core::diff_highlight;
use thegn_core::git_run::{GitOutcome, GitTextError};
use thegn_core::remote::GitLoc;

use crate::cmd::resolve_worktree;

pub fn run(
    cfg: &Config,
    worktree: Option<String>,
    base: Option<String>,
    stat: bool,
    file_path: Option<String>,
    structural: bool,
) -> Result<()> {
    let wt = resolve_worktree(worktree);
    // Route git through the worktree's location — local, or over ssh for a
    // remote worktree.
    let loc = GitLoc::for_worktree(&wt);

    let base = base.unwrap_or_else(|| default_branch(&loc));
    // Validate an explicitly-resolvable base ref up front. A typo'd `--base`
    // must NOT silently diff against HEAD: exit non-zero so scripts/CI notice
    // the mistake instead of trusting an empty/HEAD-only diff.
    let target = resolve_target(&loc, &base)?;

    // Structural (difftastic) read-only view. The `--structural` flag forces it;
    // otherwise `[git] structural_diff` may select it. Never for `--stat` (a
    // summary), and never fed to `git apply` — this surface is read-only.
    if !stat {
        let mode = if structural {
            StructuralDiff::Difft
        } else {
            // The repository's overlay, not one keyed by the worktree
            // directory's basename (THE-515). CLI: git is fine here.
            let root = thegn_core::repo::main_worktree(&wt).unwrap_or_else(|| wt.clone());
            cfg.repo_git(&root).structural_diff
        };
        if mode != StructuralDiff::Off {
            if let Some(difft) = crate::structural_diff::choose(cfg, mode) {
                return crate::structural_diff::run_cli(
                    &loc,
                    &target,
                    file_path.as_deref(),
                    &difft,
                );
            }
            // Explicit intent (flag or `= "difft"`) but nothing resolved: say so,
            // then fall through to the internal highlighter. `auto` is silent.
            if structural || mode == StructuralDiff::Difft {
                thegn_core::msg::warn(
                    "difft unavailable — falling back to the internal diff. \
                     Install difftastic or set [managed_tools.difft] path.",
                );
            }
        }
    }

    let emit_highlighted = |git_args: &[&str], file_path: Option<&str>| -> Result<()> {
        // CLI path: `thegn diff` runs synchronously, no event loop.
        #[expect(clippy::disallowed_methods)]
        let output = loc
            .git_command(git_args)
            .output()
            .context("failed to run git diff")?;
        // Surface git failures instead of swallowing them; the exit code must be
        // non-zero so scripts/CI don't mistake a git error for an empty diff.
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            bail!("git diff failed: {}", err.trim());
        }
        let raw = String::from_utf8_lossy(&output.stdout);
        let highlighted = diff_highlight::highlight_diff(&raw, file_path.unwrap_or(""));
        let _ = std::io::stdout().write_all(highlighted.as_bytes()); // best-effort: stdout write: EPIPE on a closed |head pipe is normal
        Ok(())
    };

    if let Some(fp) = file_path {
        return emit_highlighted(&["diff", "--no-color", &target, "--", &fp], Some(&fp));
    }

    if !stat {
        return emit_highlighted(&["diff", "--no-color", &target], None);
    }

    // --stat: stream straight through (colors / large diffs).
    // CLI path: `thegn diff` runs synchronously, no event loop.
    #[expect(clippy::disallowed_methods)]
    let status = loc
        .git_command(&["-c", "color.ui=always", "diff", "--stat", &target])
        .status()
        .context("failed to run git diff --stat")?;
    // Propagate git's failure: exit non-zero so scripts/CI notice.
    if !status.success() {
        bail!("git diff --stat failed");
    }
    Ok(())
}

fn resolve_target(loc: &GitLoc, base: &str) -> Result<String> {
    match loc.git_text(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{base}^{{commit}}"),
    ]) {
        Ok(_) => {}
        Err(error) if git_exit_is_one(&error) => bail!("unknown base ref '{base}'"),
        Err(error) => bail!("could not check base ref '{base}': {error}"),
    }

    // No common ancestor (exit 1) means the base is valid but unrelated.
    match loc.git_text(&["merge-base", base, "HEAD"]) {
        Ok(oid) if !oid.is_empty() => Ok(oid),
        Ok(_) => bail!("git merge-base returned an empty object id"),
        Err(error) if git_exit_is_one(&error) => Ok("HEAD".to_string()),
        Err(error) => bail!("git merge-base failed: {error}"),
    }
}

fn git_exit_is_one(error: &GitTextError) -> bool {
    matches!(
        error,
        GitTextError::Failed(GitOutcome::Exit {
            code: Some(1),
            stderr_tail,
        }) if stderr_tail.is_empty()
    )
}

/// The repo's default branch (origin/HEAD, else main/master, else HEAD), probed
/// through the location so it works for remote worktrees too. Shared with the
/// in-app diff viewer (`diff_view`).
pub(crate) fn default_branch(loc: &GitLoc) -> String {
    if let Some(r) = loc.git_out(&[
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ]) {
        return r.strip_prefix("origin/").unwrap_or(&r).to_string();
    }
    for b in ["main", "master"] {
        if loc.git_ok(&[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{b}"),
        ]) {
            return b.to_string();
        }
    }
    "HEAD".to_string()
}

#[cfg(test)]
mod tests {
    use super::resolve_target;
    use std::path::Path;
    use thegn_core::remote::GitLoc;

    #[expect(clippy::disallowed_methods, reason = "test fixture git command")]
    fn git(dir: &Path, args: &[&str]) {
        let output = thegn_core::util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
            ])
            .args(args)
            .output()
            .expect("git fixture command starts");
        assert!(
            output.status.success(),
            "git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn committed_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("fixture"), "first\n").expect("write fixture");
        git(dir, &["add", "fixture"]);
        git(dir, &["commit", "-q", "-m", "initial"]);
    }

    fn failing_provider(wrapper_dir: &Path, path: &Path, needle: &str, exit_code: u8) -> GitLoc {
        let wrapper = wrapper_dir.join("git-wrapper.sh");
        std::fs::write(
            &wrapper,
            format!(
                "case \"$3\" in *'{needle}'*) echo 'ssh: connect to host fixture: Connection refused' >&2; exit {exit_code};; esac\nexec \"$@\"\n"
            ),
        )
        .expect("write provider wrapper");
        GitLoc::Provider {
            control_prefix: vec!["sh".into(), wrapper.to_string_lossy().into_owned()],
            path: path.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn transport_failure_is_not_an_unknown_base() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path());
        let wrapper_dir = tempfile::tempdir().expect("wrapper dir");
        let location = failing_provider(
            wrapper_dir.path(),
            repo.path(),
            "rev-parse --verify --quiet",
            255,
        );

        let result = resolve_target(&location, "main");
        assert!(matches!(result, Err(error)
            if !error.to_string().starts_with("unknown base ref")
                && error.to_string().contains("Connection refused")));
    }

    #[test]
    fn merge_base_failure_is_not_head() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path());
        let wrapper_dir = tempfile::tempdir().expect("wrapper dir");
        let location = failing_provider(wrapper_dir.path(), repo.path(), "merge-base", 255);

        let result = resolve_target(&location, "main");
        assert!(matches!(result, Err(error)
            if error.to_string().contains("merge-base")
                && error.to_string().contains("Connection refused")));
    }

    #[test]
    fn unrelated_history_falls_back_to_head() {
        let repo = tempfile::tempdir().expect("local repo");
        committed_repo(repo.path());
        git(repo.path(), &["checkout", "--orphan", "other"]);
        std::fs::write(repo.path().join("other"), "second\n").expect("write orphan file");
        git(repo.path(), &["add", "other"]);
        git(repo.path(), &["commit", "-q", "-m", "orphan"]);

        let result = resolve_target(&GitLoc::Local(repo.path().to_path_buf()), "main")
            .expect("unrelated valid base uses HEAD");
        assert_eq!(result, "HEAD");
    }

    #[test]
    fn base_probe_exit_one_with_stderr_is_not_unknown_base() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path());
        let wrapper_dir = tempfile::tempdir().expect("wrapper dir");
        let location = failing_provider(
            wrapper_dir.path(),
            repo.path(),
            "rev-parse --verify --quiet",
            1,
        );

        let result = resolve_target(&location, "main");
        assert!(matches!(result, Err(error)
            if !error.to_string().starts_with("unknown base ref")
                && error.to_string().contains("Connection refused")));
    }

    #[test]
    fn merge_base_exit_one_with_stderr_is_not_head() {
        if !crate::platform::sound_process::test_support::UNIX {
            return;
        }
        let repo = tempfile::tempdir().expect("provider repo");
        committed_repo(repo.path());
        let wrapper_dir = tempfile::tempdir().expect("wrapper dir");
        let location = failing_provider(wrapper_dir.path(), repo.path(), "merge-base", 1);

        let result = resolve_target(&location, "main");
        assert!(matches!(result, Err(error)
            if error.to_string().contains("merge-base")
                && error.to_string().contains("Connection refused")));
    }

    #[test]
    fn unknown_base_is_reported() {
        let repo = tempfile::tempdir().expect("local repo");
        committed_repo(repo.path());

        let result = resolve_target(&GitLoc::Local(repo.path().to_path_buf()), "nope");
        assert!(matches!(result, Err(error)
            if error.to_string() == "unknown base ref 'nope'"));
    }
}
