//! Read-time branch observations for registered worktrees. Registry branch
//! values are creation metadata; only this Git-backed snapshot is authoritative.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

/// Lexical normalisation only: drop `.` and resolve `..` without touching the
/// filesystem (so it can never hang on a dead network mount).
fn lexical_path(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn head_observation(head: &WorktreeHead) -> BranchObservation {
    match head {
        WorktreeHead::Branch(branch) => BranchObservation::Branch(branch.clone()),
        WorktreeHead::Detached => BranchObservation::Detached,
        WorktreeHead::Unborn => BranchObservation::Unborn,
    }
}

/// Map a `current_branch` answer (`"HEAD"`/empty = detached) to an observation.
fn from_branch_name(branch: &str) -> BranchObservation {
    if branch == "HEAD" || branch.is_empty() {
        BranchObservation::Detached
    } else {
        BranchObservation::Branch(branch.to_string())
    }
}

/// Join a `git worktree list` snapshot onto registry rows. Paths match by
/// exact string or lexical form first; only rows still unmatched fall back to
/// a canonical (symlink-resolving) comparison, so the steady state never
/// touches the filesystem per entry. Rows are local by contract.
pub(crate) fn join_snapshot(
    rows: &[WorktreeRow],
    git_worktrees: &[WorktreeInfo],
) -> HashMap<String, BranchObservation> {
    let by_path: HashMap<PathBuf, BranchObservation> = git_worktrees
        .iter()
        .map(|wt| (lexical_path(&wt.path), head_observation(&wt.head)))
        .collect();
    let mut canonical: Option<HashMap<PathBuf, BranchObservation>> = None;
    rows.iter()
        .map(|row| {
            let observed = by_path
                .get(&lexical_path(&row.worktree))
                .cloned()
                .or_else(|| {
                    let canon = canonical.get_or_insert_with(|| {
                        git_worktrees
                            .iter()
                            .filter_map(|wt| {
                                Some((
                                    std::fs::canonicalize(&wt.path).ok()?,
                                    head_observation(&wt.head),
                                ))
                            })
                            .collect()
                    });
                    canon
                        .get(&std::fs::canonicalize(&row.worktree).ok()?)
                        .cloned()
                })
                .unwrap_or(BranchObservation::Unavailable);
            (row.worktree.clone(), observed)
        })
        .collect()
}

/// How a remote row (ssh / provider sandbox) may be read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteRead {
    /// Hydration and other ambient paths: only through an already-live bridge.
    /// Never spawn ssh / provider exec, never wake a suspended sandbox.
    BridgedOnly,
    /// An explicit user command: one live read per row is acceptable.
    Live,
}

fn remote_observation(
    git: &(impl GitBackend + ?Sized),
    loc: &thegn_core::remote::GitLoc,
    policy: RemoteRead,
) -> BranchObservation {
    if policy == RemoteRead::BridgedOnly && thegn_svc::bridge::for_loc(loc).is_none() {
        return BranchObservation::Unavailable;
    }
    git.current_branch(loc)
        .ok()
        .map(|branch| from_branch_name(&branch))
        .unwrap_or(BranchObservation::Unavailable)
}

/// Spawn-free observation for ambient paths (hydration, the merged-PR sweep).
///
/// Per row: the branch the glyph scan already cached (`cached`, tiered by TTL
/// with an active-worktree floor), else an in-process gix read of a local HEAD
/// (no subprocess), else, for a remote row, a read through a live bridge only,
/// else `Unavailable`. The steady state therefore does no git subprocess, no
/// ssh, and no DB open per row.
pub(crate) fn observe_rows_cheap(
    rows: &[WorktreeRow],
    cached: impl Fn(&str) -> Option<String>,
) -> HashMap<String, BranchObservation> {
    // The read engine is config-selected (gix by default); never construct
    // one here — see the justfile `GixGit::new()` guard.
    let git = crate::git_handle::get();
    observe_rows_cheap_in(
        head_read_cache(),
        HEAD_READ_TTL,
        rows,
        cached,
        &*git,
        |loc| git.current_branch(loc),
    )
}

/// How long an in-process HEAD read is trusted. Rows reach the gix read only
/// when the glyph cache has no entry (a workspace not visited this session, or
/// a glyph read that errored), and each read is a repository open (~0.2-1 ms
/// of CPU plus a dozen file reads) repeated on every 5 s idle hydration. A
/// minute bounds that to ~1/12 of the ticks; staleness is bounded by the TTL
/// and by [`invalidate_head_reads`], which the ref-move signal
/// ([`crate::branch_cache::invalidate_all`]) calls.
pub(crate) const HEAD_READ_TTL: Duration = Duration::from_secs(60);

/// Hard bound on the map; past it the whole map is dropped (it is a cache).
const HEAD_READ_CAP: usize = 4096;

/// Path-keyed memo of local `current_branch` answers, including failures and
/// detached/unborn results so a broken row does not re-read every tick.
#[derive(Default)]
pub(crate) struct HeadReadCache {
    map: Mutex<HashMap<String, (BranchObservation, Instant)>>,
}

impl HeadReadCache {
    fn get(&self, path: &str, ttl: Duration) -> Option<BranchObservation> {
        let map = self.map.lock().unwrap();
        map.get(path)
            .filter(|(_, at)| at.elapsed() < ttl)
            .map(|(obs, _)| obs.clone())
    }

    fn put(&self, path: &str, obs: BranchObservation, ttl: Duration) {
        let mut map = self.map.lock().unwrap();
        if map.len() >= HEAD_READ_CAP {
            map.clear();
        } else if map.len() >= 64 {
            // Amortised prune of expired entries, only once the map is non-tiny.
            map.retain(|_, (_, at)| at.elapsed() < ttl);
        }
        map.insert(path.to_string(), (obs, Instant::now()));
    }

    pub(crate) fn clear(&self) {
        self.map.lock().unwrap().clear();
    }
}

fn head_read_cache() -> &'static HeadReadCache {
    static CACHE: OnceLock<HeadReadCache> = OnceLock::new();
    CACHE.get_or_init(HeadReadCache::default)
}

/// Drop every memoised HEAD read (a ref moved). No I/O.
pub(crate) fn invalidate_head_reads() {
    head_read_cache().clear();
}

fn observe_rows_cheap_in(
    memo: &HeadReadCache,
    ttl: Duration,
    rows: &[WorktreeRow],
    cached: impl Fn(&str) -> Option<String>,
    gix: &(impl GitBackend + ?Sized),
    read_local: impl Fn(&thegn_core::remote::GitLoc) -> anyhow::Result<String>,
) -> HashMap<String, BranchObservation> {
    rows.iter()
        .map(|row| {
            let loc = thegn_core::remote::GitLoc::from_db(&row.worktree, Some(&row.location));
            let observed = if let Some(branch) = cached(&row.worktree) {
                from_branch_name(&branch)
            } else if loc.is_remote() {
                remote_observation(gix, &loc, RemoteRead::BridgedOnly)
            } else if let Some(hit) = memo.get(&row.worktree, ttl) {
                hit
            } else {
                let obs = read_local(&loc)
                    .ok()
                    .map(|branch| from_branch_name(&branch))
                    .unwrap_or(BranchObservation::Unavailable);
                memo.put(&row.worktree, obs.clone(), ttl);
                obs
            };
            (row.worktree.clone(), observed)
        })
        .collect()
}

/// Take one Git worktree-list snapshot per local repository and read remote
/// rows per `remote`. For explicit commands / control calls, never the
/// hydration tick (see [`observe_rows_cheap`]).
pub(crate) fn observe_rows(
    git: &(impl GitBackend + ?Sized),
    rows: &[WorktreeRow],
    remote: RemoteRead,
) -> HashMap<String, BranchObservation> {
    let mut observations = HashMap::new();
    let mut local_by_root: HashMap<String, Vec<WorktreeRow>> = HashMap::new();
    for row in rows {
        let loc = thegn_core::remote::GitLoc::from_db(&row.worktree, Some(&row.location));
        if loc.is_remote() {
            observations.insert(row.worktree.clone(), remote_observation(git, &loc, remote));
        } else {
            local_by_root
                .entry(row.repo_root.clone())
                .or_default()
                .push(row.clone());
        }
    }
    for (root, repo_rows) in local_by_root {
        if root.is_empty() {
            observations.extend(
                repo_rows
                    .into_iter()
                    .map(|row| (row.worktree, BranchObservation::Unavailable)),
            );
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

    // `Command::status` is a blocking child wait, disallowed so it can never
    // reach the render/input loop. A `#[cfg(test)]` fixture building a throwaway
    // repo is the sanctioned off-loop case.
    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture: blocking git in a temp repo, never on the event loop"
    )]
    #[test]
    fn external_checkout_reconciles_against_git_worktree_list() {
        let temp = tempfile::TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        std::fs::create_dir_all(&repo).unwrap();
        // `git_cmd` scrubs GIT_DIR/GIT_WORK_TREE/GIT_INDEX_FILE, so this stays
        // in the temp repo even when run under an exporting git hook.
        let git = |dir: &Path, args: &[&str]| {
            let status = thegn_core::util::git_cmd(dir).args(args).status().unwrap();
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
        let joined = join_snapshot(std::slice::from_ref(&row), &snapshot);
        assert_eq!(
            joined[linked.to_str().unwrap()],
            BranchObservation::Branch("outside-change".into()),
            "the registry's creation-time branch must not survive an external checkout",
        );
        // The cheap (hydration) observer agrees, with no git subprocess.
        let before = thegn_svc::git::git_spawn_count();
        let cheap = observe_rows_cheap(std::slice::from_ref(&row), |_| None);
        assert_eq!(
            cheap[linked.to_str().unwrap()],
            BranchObservation::Branch("outside-change".into()),
        );
        assert_eq!(thegn_svc::git::git_spawn_count(), before);
    }

    #[test]
    fn cheap_observation_prefers_the_glyph_cache_and_never_execs_remote() {
        let mut local = row("/definitely/not/on/disk", "registry");
        local.location = String::new();
        let mut remote = row("/srv/wt", "registry");
        remote.location = thegn_core::remote::GitLoc::remote_db_string(
            "nonexistent.invalid",
            22,
            false,
            "/srv/wt",
        );
        let cached = observe_rows_cheap(&[local.clone(), remote.clone()], |p| {
            (p == "/definitely/not/on/disk").then(|| "from-cache".to_string())
        });
        assert_eq!(
            cached["/definitely/not/on/disk"],
            BranchObservation::Branch("from-cache".into())
        );
        // No cache entry, no bridge: Unavailable, not an ssh exec.
        assert_eq!(cached["/srv/wt"], BranchObservation::Unavailable);
        // A cached "HEAD" is a detached checkout.
        let detached = observe_rows_cheap(&[local], |_| Some("HEAD".into()));
        assert_eq!(
            detached["/definitely/not/on/disk"],
            BranchObservation::Detached
        );
    }

    #[test]
    fn head_read_is_memoised_within_ttl_and_reread_after() {
        use std::cell::Cell;
        let memo = HeadReadCache::default();
        let gix = crate::git_handle::get();
        let rows = [row("/not/on/disk", "registry")];
        let reads = Cell::new(0);
        let read = |_: &thegn_core::remote::GitLoc| {
            reads.set(reads.get() + 1);
            Ok("feature/x".to_string())
        };
        let ttl = Duration::from_secs(60);
        let a = observe_rows_cheap_in(&memo, ttl, &rows, |_| None, &*gix, read);
        let b = observe_rows_cheap_in(&memo, ttl, &rows, |_| None, &*gix, read);
        assert_eq!(a, b);
        assert_eq!(reads.get(), 1, "second observation inside the TTL re-read");
        // An expired entry re-reads.
        observe_rows_cheap_in(&memo, Duration::ZERO, &rows, |_| None, &*gix, read);
        assert_eq!(reads.get(), 2);
        // Invalidation re-reads.
        memo.clear();
        observe_rows_cheap_in(&memo, ttl, &rows, |_| None, &*gix, read);
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn failed_reads_are_memoised_too() {
        use std::cell::Cell;
        let memo = HeadReadCache::default();
        let gix = crate::git_handle::get();
        let rows = [row("/not/on/disk", "registry")];
        let reads = Cell::new(0);
        let read = |_: &thegn_core::remote::GitLoc| {
            reads.set(reads.get() + 1);
            Err(anyhow::anyhow!("broken"))
        };
        let ttl = Duration::from_secs(60);
        for _ in 0..3 {
            let got = observe_rows_cheap_in(&memo, ttl, &rows, |_| None, &*gix, read);
            assert_eq!(got["/not/on/disk"], BranchObservation::Unavailable);
        }
        assert_eq!(reads.get(), 1);
    }
}
