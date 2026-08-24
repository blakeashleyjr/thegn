//! Resolve a *local* worktree's gitdir without spawning `git`.
//!
//! `GitLoc::resolve_git_path` shells out to `rev-parse --git-path <rel>` on
//! every call, including for local worktrees. That made the merge/rebase banner
//! probe in `merge_state` cost **five** subprocesses on a clean repo (three
//! `rev-parse --verify` pseudo-ref probes plus two `--git-path` resolutions for
//! the rebase state dir), and the hydration path runs it twice per refresh
//! cycle — ten spawns to answer "is a merge in progress?", which is almost
//! always "no".
//!
//! It does not need git at all. `gitrepository-layout(5)` specifies the layout:
//! `<worktree>/.git` is either the repository directory itself, or a **file**
//! containing `gitdir: <path>` — the form used by linked worktrees
//! (`git worktree add`) and by `--separate-git-dir`. A linked worktree's gitdir
//! additionally carries a `commondir` file pointing at the shared repository.
//!
//! Reading that is one `stat` plus at most one small file read, and the parsing
//! is pure — so the Windows arms are covered by the Linux coverage gate.
//!
//! Deliberately local-only. Remote and provider locs keep the subprocess path:
//! their gitdir lives on another machine, and the bridge already batches those
//! probes.

use std::path::{Path, PathBuf};

/// Parse the payload of a `.git` *file* into the gitdir it points at.
///
/// The documented form is a single `gitdir: <path>` line. The path may be
/// absolute or relative to the worktree. Returns `None` for anything else, so a
/// malformed pointer falls back to the subprocess path rather than guessing.
pub fn parse_dotgit_pointer(contents: &str) -> Option<&str> {
    contents
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

/// Join a (possibly relative) gitdir pointer against the worktree that holds
/// the `.git` file. Pure so the relative/absolute split is unit-tested on every
/// platform — note a Windows absolute path (`C:\…`) is not `starts_with('/')`,
/// which is exactly the bug an ad-hoc check would introduce.
pub fn resolve_pointer(worktree: &Path, pointer: &str) -> PathBuf {
    let p = Path::new(pointer);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        worktree.join(p)
    }
}

/// The gitdir for `worktree`, or `None` when no repository contains it.
///
/// Ascends like git's own discovery does, so a path *inside* a worktree
/// resolves the same as one at its root — `rev-parse` walks up, and a helper
/// that exists to replace `rev-parse` has to walk up too or it silently
/// disagrees on every subdirectory.
///
/// `None` is therefore a real answer — "no repository contains this path" —
/// and not merely "I could not tell". Callers can act on it instead of falling
/// back to a subprocess that will only reach the same conclusion more slowly.
pub fn local_git_dir(worktree: &Path) -> Option<PathBuf> {
    let mut cur = Some(worktree);
    while let Some(dir) = cur {
        if let Some(found) = git_dir_at(dir) {
            return Some(found);
        }
        cur = dir.parent();
    }
    None
}

/// The gitdir recorded by `<dir>/.git` exactly, without ascending.
fn git_dir_at(dir: &Path) -> Option<PathBuf> {
    let dot = dir.join(".git");
    let meta = std::fs::symlink_metadata(&dot).ok()?;
    if meta.is_dir() {
        return Some(dot);
    }
    // A file (linked worktree / --separate-git-dir), or a symlink to one.
    let contents = std::fs::read_to_string(&dot).ok()?;
    let pointer = parse_dotgit_pointer(&contents)?;
    Some(resolve_pointer(dir, pointer))
}

/// Is `path` inside a git worktree at all?
///
/// A stat-only answer to the question every git read implicitly asks first.
/// thegn's own default workspace is the user's home directory, which is
/// usually not a repository — and the hydration fan-out was firing seven git
/// subprocesses at it every refresh, each one failing with "not a git
/// repository" after paying full process-creation cost. On Linux that is a few
/// milliseconds of waste; on Windows, where a bare `git rev-parse` measures
/// ~170ms, it was ~950ms of spawning per cycle to learn nothing.
pub fn is_git_worktree(path: &Path) -> bool {
    local_git_dir(path).is_some()
}

/// The *common* gitdir for `worktree` — the shared repository directory that
/// linked worktrees hang off, i.e. what `rev-parse --git-common-dir` answers.
///
/// A linked worktree's gitdir carries a `commondir` file pointing at it
/// (usually the relative `../..`); a main worktree's gitdir *is* the common
/// dir. `None` when the layout is not recognised, so callers fall back.
pub fn local_git_common_dir(worktree: &Path) -> Option<PathBuf> {
    let dir = local_git_dir(worktree)?;
    match std::fs::read_to_string(dir.join("commondir")) {
        Ok(s) => {
            let rel = s.trim();
            if rel.is_empty() {
                return Some(dir);
            }
            let p = Path::new(rel);
            Some(if p.is_absolute() {
                p.to_path_buf()
            } else {
                dir.join(p)
            })
        }
        // No `commondir` => this gitdir is itself the common dir.
        Err(_) => Some(dir),
    }
}

/// Whether a pseudo-ref file exists in `worktree`'s gitdir.
///
/// `MERGE_HEAD`, `CHERRY_PICK_HEAD` and `REVERT_HEAD` are pseudo-refs: git
/// writes them as plain files in the gitdir and never packs them into
/// `packed-refs`, so existence is equivalent to what
/// `rev-parse -q --verify <NAME>` answers — which is how git's own prompt
/// scripts detect an in-progress operation.
/// `Some(false)` when no repository contains `worktree` — that is an answer,
/// not a failure. `rev-parse -q --verify MERGE_HEAD` in a non-repository exits
/// non-zero with "not a git repository", which is the same "no" this reports
/// for the price of a few `stat`s instead of a process.
pub fn git_path_exists(worktree: &Path, rel: &str) -> bool {
    local_git_dir(worktree).is_some_and(|dir| dir.join(rel).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_pointer_form() {
        assert_eq!(
            parse_dotgit_pointer("gitdir: /repo/.git/worktrees/wt-1\n"),
            Some("/repo/.git/worktrees/wt-1")
        );
        // Windows spelling, and no trailing newline.
        assert_eq!(
            parse_dotgit_pointer(r"gitdir: C:\repo\.git\worktrees\wt-1"),
            Some(r"C:\repo\.git\worktrees\wt-1")
        );
        // Tolerate surrounding blank lines / whitespace.
        assert_eq!(parse_dotgit_pointer("\n  gitdir:   /a/b  \n"), Some("/a/b"));
    }

    #[test]
    fn rejects_anything_that_is_not_a_pointer() {
        assert_eq!(parse_dotgit_pointer(""), None);
        assert_eq!(parse_dotgit_pointer("ref: refs/heads/main\n"), None);
        // An empty target is malformed, not a pointer to the worktree root.
        assert_eq!(parse_dotgit_pointer("gitdir:\n"), None);
        assert_eq!(parse_dotgit_pointer("gitdir:   \n"), None);
    }

    #[test]
    fn resolves_absolute_pointers_as_is_on_this_platform() {
        // The platform's own notion of absolute — `C:\…` is absolute on
        // Windows but not on unix, and vice versa for `/…`.
        let abs = if cfg!(windows) {
            r"C:\repo\.git"
        } else {
            "/repo/.git"
        };
        assert_eq!(
            resolve_pointer(Path::new("/wt"), abs),
            PathBuf::from(abs),
            "an absolute pointer must not be joined onto the worktree"
        );
    }

    #[test]
    fn resolves_relative_pointers_against_the_worktree() {
        let got = resolve_pointer(Path::new("/wt"), "../.git/worktrees/w1");
        assert_eq!(got, Path::new("/wt").join("../.git/worktrees/w1"));
    }

    #[test]
    fn main_worktree_gitdir_is_the_dot_git_directory() {
        let tmp = std::env::temp_dir().join(format!("tg-gitdir-main-{}", std::process::id()));
        let dot = tmp.join(".git");
        std::fs::create_dir_all(&dot).unwrap();
        assert_eq!(local_git_dir(&tmp), Some(dot));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn linked_worktree_gitdir_follows_the_pointer_file() {
        let tmp = std::env::temp_dir().join(format!("tg-gitdir-linked-{}", std::process::id()));
        let real = tmp.join("repo/.git/worktrees/wt-1");
        std::fs::create_dir_all(&real).unwrap();
        let wt = tmp.join("wt-1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", real.to_string_lossy()),
        )
        .unwrap();

        assert_eq!(local_git_dir(&wt), Some(real.clone()));

        // The pseudo-ref probe reads through that pointer.
        assert!(!git_path_exists(&wt, "MERGE_HEAD"));
        std::fs::write(real.join("MERGE_HEAD"), "deadbeef\n").unwrap();
        assert!(git_path_exists(&wt, "MERGE_HEAD"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn common_dir_is_the_gitdir_itself_for_a_main_worktree() {
        let tmp = std::env::temp_dir().join(format!("tg-gitdir-common-{}", std::process::id()));
        let dot = tmp.join(".git");
        std::fs::create_dir_all(&dot).unwrap();
        // No `commondir` file => this gitdir IS the common dir.
        assert_eq!(local_git_common_dir(&tmp), Some(dot));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn common_dir_follows_a_linked_worktrees_commondir_pointer() {
        let tmp = std::env::temp_dir().join(format!("tg-gitdir-common2-{}", std::process::id()));
        let main_git = tmp.join("repo/.git");
        let wt_git = main_git.join("worktrees/wt-1");
        std::fs::create_dir_all(&wt_git).unwrap();
        let wt = tmp.join("wt-1");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_git.to_string_lossy()),
        )
        .unwrap();
        // git writes this relative, as `../..`.
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();

        let got = local_git_common_dir(&wt).unwrap();
        // Same directory, though spelled with `..` segments.
        assert_eq!(
            std::fs::canonicalize(&got).unwrap(),
            std::fs::canonicalize(&main_git).unwrap()
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_non_worktree_yields_none_so_the_caller_gets_a_definite_no() {
        // A temp dir with no repository above it. `None` here is an ANSWER —
        // callers skip the git read entirely rather than spawning a subprocess
        // that would reach the same conclusion for the price of a process.
        let tmp = std::env::temp_dir().join(format!("tg-gitdir-none-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(local_git_dir(&tmp), None);
        assert!(!is_git_worktree(&tmp));
        assert!(!git_path_exists(&tmp, "MERGE_HEAD"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn discovery_ascends_like_git_does() {
        // `rev-parse` answers for any path INSIDE a worktree, not just its
        // root — so a helper written to replace it has to ascend too, or it
        // silently disagrees on every subdirectory and the caller falls back
        // to the subprocess it was meant to avoid.
        let tmp = std::env::temp_dir().join(format!("tg-gitdir-asc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dot = tmp.join(".git");
        let deep = tmp.join("crates").join("thing").join("src");
        std::fs::create_dir_all(&dot).unwrap();
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(local_git_dir(&deep), Some(dot.clone()));
        assert!(is_git_worktree(&deep));
        // And the pseudo-ref probe reads the repo's gitdir from down there.
        assert!(!git_path_exists(&deep, "MERGE_HEAD"));
        std::fs::write(dot.join("MERGE_HEAD"), "deadbeef\n").unwrap();
        assert!(git_path_exists(&deep, "MERGE_HEAD"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
