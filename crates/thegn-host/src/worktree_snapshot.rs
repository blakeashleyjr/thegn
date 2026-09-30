//! Read-time branch observations for registered worktrees. Registry branch
//! values are creation metadata; only this Git-backed snapshot is authoritative.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use thegn_core::models::WorktreeRow;
use thegn_svc::git::{GitBackend, WorktreeHead, WorktreeInfo};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BranchObservation {
    Branch(String),
    Detached,
    Unborn,
    Unavailable,
}

impl BranchObservation {
    pub(crate) fn branch(&self) -> Option<&str> {
        match self {
            Self::Branch(branch) => Some(branch),
            Self::Detached | Self::Unborn | Self::Unavailable => None,
        }
    }

    pub(crate) fn display_branch<'a>(&'a self, fallback: &'a str) -> Option<&'a str> {
        match self {
            Self::Branch(branch) => Some(branch),
            Self::Detached | Self::Unborn => None,
            Self::Unavailable => (!fallback.is_empty()).then_some(fallback),
        }
    }
}

fn normalized_path(path: &str) -> PathBuf {
    let path = Path::new(path);
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        // Deleted paths cannot canonicalize. Preserve exact path components
        // while removing `.` and resolving lexical `..` for stable matching.
        let mut out = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    })
}

pub(crate) fn join_snapshot(
    rows: &[WorktreeRow],
    git_worktrees: &[WorktreeInfo],
) -> HashMap<String, BranchObservation> {
    let by_path: HashMap<PathBuf, BranchObservation> = git_worktrees
        .iter()
        .map(|wt| {
            let observation = match &wt.head {
                WorktreeHead::Branch(branch) => BranchObservation::Branch(branch.clone()),
                WorktreeHead::Detached => BranchObservation::Detached,
                WorktreeHead::Unborn => BranchObservation::Unborn,
            };
            (normalized_path(&wt.path), observation)
        })
        .collect();
    rows.iter()
        .map(|row| {
            (
                row.worktree.clone(),
                by_path
                    .get(&normalized_path(&row.worktree))
                    .cloned()
                    .unwrap_or(BranchObservation::Unavailable),
            )
        })
        .collect()
}

pub(crate) fn branch_for_path(row: &WorktreeRow, snapshot: &[WorktreeInfo]) -> Option<String> {
    join_snapshot(std::slice::from_ref(row), snapshot)
        .remove(&row.worktree)
        .and_then(|observation| observation.branch().map(str::to_owned))
}

/// Take one Git worktree-list snapshot per local repository, and one direct
/// branch read for each remote row. Called only from an existing worker.
pub(crate) fn observe_rows(
    git: &(impl GitBackend + ?Sized),
    rows: &[WorktreeRow],
) -> HashMap<String, BranchObservation> {
    let mut roots = HashSet::new();
    let mut observations = HashMap::new();
    let mut local_by_root: HashMap<String, Vec<WorktreeRow>> = HashMap::new();
    for row in rows {
        let loc = thegn_core::remote::GitLoc::for_worktree(Path::new(&row.worktree));
        if loc.is_remote() {
            let observed = git
                .current_branch(&loc)
                .ok()
                .map(|branch| {
                    if branch == "HEAD" || branch.is_empty() {
                        BranchObservation::Detached
                    } else {
                        BranchObservation::Branch(branch)
                    }
                })
                .unwrap_or(BranchObservation::Unavailable);
            observations.insert(row.worktree.clone(), observed);
        } else {
            local_by_root
                .entry(row.repo_root.clone())
                .or_default()
                .push(row.clone());
        }
    }
    for (root, repo_rows) in local_by_root {
        if root.is_empty() || !roots.insert(root.clone()) {
            continue;
        }
        match git.worktrees(Path::new(&root)) {
            Ok(snapshot) => observations.extend(join_snapshot(&repo_rows, &snapshot)),
            Err(_) => observations.extend(
                repo_rows
                    .into_iter()
                    .map(|row| (row.worktree, BranchObservation::Unavailable)),
            ),
        }
    }
    observations
}

/// Resolve one worktree's branch for command context. Failure, detached HEAD,
/// or unborn HEAD intentionally yields no branch; the registry is not fallback
/// authority for agent or dispatch context.
pub(crate) fn current_branch(path: &Path) -> Option<String> {
    let loc = thegn_core::remote::GitLoc::for_worktree(path);
    let branch = crate::git_handle::get().current_branch(&loc).ok()?;
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(path: &str, branch: &str) -> WorktreeRow {
        WorktreeRow {
            worktree: path.into(),
            branch: branch.into(),
            agent: String::new(),
            created_at: 0,
            repo_root: "/repo".into(),
            tab_name: String::new(),
            session_name: String::new(),
            location: String::new(),
            position: 0,
            sandbox_backend: None,
            observed_backend: None,
            folder_id: None,
            env_name: None,
        }
    }

    #[test]
    fn snapshot_matches_paths_and_never_stale_branch_names() {
        let rows = vec![
            row("/repo/wt-a", "collision"),
            row("/repo/wt-b", "collision"),
        ];
        let snapshot = vec![
            WorktreeInfo {
                path: "/repo/wt-a".into(),
                head: WorktreeHead::Branch("feature/a".into()),
            },
            WorktreeInfo {
                path: "/repo/wt-b".into(),
                head: WorktreeHead::Branch("feature/b".into()),
            },
        ];
        let joined = join_snapshot(&rows, &snapshot);
        assert_eq!(
            joined["/repo/wt-a"],
            BranchObservation::Branch("feature/a".into())
        );
        assert_eq!(
            joined["/repo/wt-b"],
            BranchObservation::Branch("feature/b".into())
        );
    }

    #[test]
    fn snapshot_preserves_detached_unborn_and_missing_states() {
        let rows = vec![
            row("/repo/detached", "old"),
            row("/repo/unborn", "old"),
            row("/repo/deleted", "old"),
        ];
        let snapshot = vec![
            WorktreeInfo {
                path: "/repo/detached".into(),
                head: WorktreeHead::Detached,
            },
            WorktreeInfo {
                path: "/repo/unborn".into(),
                head: WorktreeHead::Unborn,
            },
        ];
        let joined = join_snapshot(&rows, &snapshot);
        assert_eq!(joined["/repo/detached"], BranchObservation::Detached);
        assert_eq!(joined["/repo/unborn"], BranchObservation::Unborn);
        assert_eq!(joined["/repo/deleted"], BranchObservation::Unavailable);
        assert_eq!(
            BranchObservation::Unavailable.display_branch("cached"),
            Some("cached")
        );
        assert_eq!(BranchObservation::Detached.display_branch("cached"), None);
        assert_eq!(BranchObservation::Unborn.display_branch("cached"), None);
    }

    #[test]
    fn external_checkout_reconciles_against_git_worktree_list() {
        use std::process::Command;
        let temp = std::env::temp_dir().join(format!(
            "thegn-the706-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let repo = temp.join("repo");
        let linked = temp.join("linked");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let status = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {:?} failed", args);
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(repo.join("file"), "initial\n").unwrap();
        git(&repo, &["add", "file"]);
        git(
            &repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", "initial"],
        );
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "created",
                linked.to_str().unwrap(),
            ],
        );
        git(&linked, &["checkout", "-q", "-b", "outside-change"]);
        let row = row(linked.to_str().unwrap(), "created");
        let snapshot = thegn_svc::git::CliGit.worktrees(&repo).unwrap();
        let joined = join_snapshot(&[row], &snapshot);
        assert_eq!(
            joined[linked.to_str().unwrap()],
            BranchObservation::Branch("outside-change".into()),
            "the registry's creation-time branch must not survive an external checkout",
        );
        std::fs::remove_dir_all(temp).unwrap();
    }
}
