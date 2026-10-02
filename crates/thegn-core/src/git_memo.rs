//! Fingerprint-keyed memo for git answers that are a pure function of a few
//! small files (THE-718).
//!
//! The idle hydration loop used to re-derive "which repo is this worktree in"
//! with a `git rev-parse` subprocess on every tick (91 forks/min on a
//! one-worktree session). The answer only changes when the worktree's `.git`
//! entry changes, so it is memoised under a [`Fingerprint`] of exactly those
//! inputs, read in-process.
//!
//! **Fail toward today's behaviour, never toward staleness**: when a complete
//! fingerprint cannot be computed (no `.git` entry at the path, unreadable
//! files) [`fingerprint`] returns `None` and the caller runs the subprocess
//! every time, exactly as before.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// mtime + length of one file or directory; `None` stands for "absent".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp(Option<(Option<SystemTime>, u64, bool)>);

fn stamp(path: &Path) -> Stamp {
    Stamp(
        std::fs::metadata(path)
            .ok()
            .map(|m| (m.modified().ok(), m.len(), m.is_dir())),
    )
}

/// What a worktree's repo-identity answer was derived from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fingerprint {
    /// The path itself does not exist (a failure result is memoised under
    /// this, and retried the moment the path appears).
    Missing,
    /// `<path>/.git` is a directory (main checkout): its own stamp and HEAD's.
    DotGitDir { dot_git: Stamp, head: Stamp },
    /// `<path>/.git` is a file (linked worktree): the file's stamp and
    /// content, plus the stamp and content of the `commondir` file it points
    /// through (absent when the per-worktree gitdir is gone).
    DotGitFile {
        dot_git: Stamp,
        content: String,
        commondir: Stamp,
        commondir_content: Option<String>,
    },
}

/// Per-worktree git dir named by a `.git` file's `gitdir:` line.
pub(crate) fn gitdir_from_dot_git_file(worktree: &Path, content: &str) -> Option<PathBuf> {
    let p = content
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|p| !p.is_empty())?;
    Some(if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        worktree.join(p)
    })
}

/// The per-worktree git directory of a local worktree root, resolved without
/// a subprocess: `<dir>/.git` itself when it is a directory, else the target of
/// its `gitdir:` line. `None` when `<dir>/.git` is absent/unparseable or the
/// resolved directory does not exist, so callers fall back to the `git` CLI
/// (which also honours `GIT_DIR` and discovery from a subdirectory).
pub fn git_dir(dir: &Path) -> Option<PathBuf> {
    let dot_git = dir.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    let content = std::fs::read_to_string(&dot_git).ok()?;
    gitdir_from_dot_git_file(dir, &content).filter(|g| g.is_dir())
}

/// Compute the fingerprint for `dir`, or `None` when it cannot be done
/// soundly (callers then bypass the memo).
pub fn fingerprint(dir: &Path) -> Option<Fingerprint> {
    match std::fs::metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(Fingerprint::Missing),
        Err(_) => return None,
        Ok(m) if !m.is_dir() => return None,
        Ok(_) => {}
    }
    let dot_git = dir.join(".git");
    let meta = std::fs::metadata(&dot_git).ok()?;
    if meta.is_dir() {
        return Some(Fingerprint::DotGitDir {
            dot_git: stamp(&dot_git),
            head: stamp(&dot_git.join("HEAD")),
        });
    }
    let content = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = gitdir_from_dot_git_file(dir, &content)?;
    let commondir_path = gitdir.join("commondir");
    Some(Fingerprint::DotGitFile {
        dot_git: stamp(&dot_git),
        commondir: stamp(&commondir_path),
        commondir_content: std::fs::read_to_string(&commondir_path).ok(),
        content,
    })
}

/// A bounded path -> (fingerprint, value) memo; the oldest entry is evicted.
pub struct Memo<V: Clone> {
    cap: usize,
    map: HashMap<PathBuf, (Fingerprint, V)>,
    order: VecDeque<PathBuf>,
}

impl<V: Clone> Memo<V> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub fn get(&self, path: &Path, fp: &Fingerprint) -> Option<V> {
        self.map
            .get(path)
            .filter(|(f, _)| f == fp)
            .map(|(_, v)| v.clone())
    }

    pub fn put(&mut self, path: &Path, fp: Fingerprint, v: V) {
        if self.map.insert(path.to_path_buf(), (fp, v)).is_none() {
            self.order.push_back(path.to_path_buf());
            while self.map.len() > self.cap {
                match self.order.pop_front() {
                    Some(old) => {
                        self.map.remove(&old);
                    }
                    None => break,
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Memoise `compute()` for `dir` under its [`fingerprint`]. The fingerprint is
/// taken BEFORE `compute` runs, so a change racing the computation is stored
/// under the old fingerprint and re-derived on the next call (never stale).
pub fn memoised<V: Clone>(
    memo: &Mutex<Option<Memo<V>>>,
    cap: usize,
    dir: &Path,
    compute: impl FnOnce() -> V,
) -> V {
    let Some(fp) = fingerprint(dir) else {
        return compute();
    };
    {
        let guard = memo.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = guard.as_ref().and_then(|m| m.get(dir, &fp)) {
            return v;
        }
    }
    let v = compute();
    memo.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(|| Memo::new(cap))
        .put(dir, fp, v.clone());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tg-gmemo-{}-{}", std::process::id(), name));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn missing_path_fingerprints_as_missing_then_changes_when_it_appears() {
        let base = tmp("missing");
        let p = base.join("later");
        assert_eq!(fingerprint(&p), Some(Fingerprint::Missing));
        std::fs::create_dir_all(p.join(".git")).unwrap();
        assert!(matches!(
            fingerprint(&p),
            Some(Fingerprint::DotGitDir { .. })
        ));
    }

    #[test]
    fn dir_without_dot_git_is_unfingerprintable() {
        let base = tmp("nodotgit");
        assert_eq!(fingerprint(&base), None);
        let file = base.join("f");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(fingerprint(&file), None);
    }

    #[test]
    fn dot_git_file_fingerprint_follows_commondir() {
        let base = tmp("dotgitfile");
        let wt = base.join("wt");
        let gitdir = base.join("main/.git/worktrees/wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        let a = fingerprint(&wt).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        let b = fingerprint(&wt).unwrap();
        assert_ne!(a, b, "commondir appearing must change the fingerprint");
        assert_eq!(b, fingerprint(&wt).unwrap(), "untouched means same print");
        std::fs::write(gitdir.join("commondir"), "/elsewhere\n").unwrap();
        assert_ne!(b, fingerprint(&wt).unwrap());
    }

    #[test]
    fn relative_gitdir_resolves_against_the_worktree() {
        let wt = Path::new("/a/wt");
        assert_eq!(
            gitdir_from_dot_git_file(wt, "gitdir: ../m/.git/worktrees/wt\n"),
            Some(PathBuf::from("/a/wt/../m/.git/worktrees/wt"))
        );
        assert_eq!(
            gitdir_from_dot_git_file(wt, "gitdir: /abs/x\n"),
            Some(PathBuf::from("/abs/x"))
        );
        assert_eq!(gitdir_from_dot_git_file(wt, "garbage"), None);
        assert_eq!(gitdir_from_dot_git_file(wt, "gitdir:  \n"), None);
    }

    #[test]
    fn git_dir_resolves_dir_file_and_rejects_dangling() {
        let base = tmp("gitdir");
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        assert_eq!(git_dir(&main), Some(main.join(".git")));
        let wt = base.join("wt");
        let gd = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gd.display())).unwrap();
        assert_eq!(git_dir(&wt), Some(gd.clone()));
        std::fs::remove_dir_all(&gd).unwrap();
        assert_eq!(git_dir(&wt), None, "pruned gitdir falls back to the CLI");
        assert_eq!(git_dir(&base.join("nope")), None);
        std::fs::write(wt.join(".git"), "junk").unwrap();
        assert_eq!(git_dir(&wt), None);
    }

    #[test]
    fn memo_is_bounded_and_evicts_oldest() {
        let mut m: Memo<u32> = Memo::new(2);
        let fp = Fingerprint::Missing;
        m.put(Path::new("/a"), fp.clone(), 1);
        m.put(Path::new("/b"), fp.clone(), 2);
        m.put(Path::new("/c"), fp.clone(), 3);
        assert_eq!(m.len(), 2);
        assert!(!m.is_empty());
        assert_eq!(m.get(Path::new("/a"), &fp), None);
        assert_eq!(m.get(Path::new("/c"), &fp), Some(3));
        // Overwriting an existing key does not grow the queue.
        m.put(Path::new("/c"), fp.clone(), 4);
        assert_eq!(m.len(), 2);
        assert_eq!(m.get(Path::new("/c"), &fp), Some(4));
        assert_eq!(
            m.get(
                Path::new("/b"),
                &Fingerprint::DotGitDir {
                    dot_git: Stamp(None),
                    head: Stamp(None)
                }
            ),
            None
        );
    }

    #[test]
    fn memoised_computes_once_per_fingerprint_and_bypasses_when_unprintable() {
        let base = tmp("memoised");
        let memo: Mutex<Option<Memo<u32>>> = Mutex::new(None);
        let calls = std::cell::Cell::new(0);
        let run = |dir: &Path| {
            memoised(&memo, 4, dir, || {
                calls.set(calls.get() + 1);
                calls.get()
            })
        };
        let gone = base.join("gone");
        assert_eq!(run(&gone), 1);
        assert_eq!(run(&gone), 1, "failure memoised while the path is missing");
        std::fs::create_dir_all(gone.join(".git")).unwrap();
        assert_eq!(run(&gone), 2, "retried the moment the path appears");
        assert_eq!(run(&gone), 2);
        // A dir with no .git cannot be fingerprinted: always recomputed.
        assert_eq!(run(&base), 3);
        assert_eq!(run(&base), 4);
    }
}
