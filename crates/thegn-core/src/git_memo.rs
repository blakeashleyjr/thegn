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
    /// `git remote get-url origin`: the worktree's `.git` print plus every
    /// config input (see [`OriginInputs`]).
    Origin {
        base: Box<Fingerprint>,
        inputs: OriginInputs,
    },
    /// A forge checkout scope: everything the origin print covers, plus what
    /// `rev-parse --abbrev-ref HEAD` reads (see [`HeadInputs`]).
    Checkout {
        origin: Box<Fingerprint>,
        head: HeadInputs,
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

// ---------------------------------------------------------------------------
// `git remote get-url origin` memo (THE-718 chunk 3)
// ---------------------------------------------------------------------------

/// Upper bound on a config file we are willing to fold into a fingerprint.
const MAX_CONFIG_BYTES: usize = 1 << 20;
/// How many `include.path` hops are followed before giving up.
const MAX_INCLUDE_DEPTH: usize = 8;

/// The inputs `git remote get-url origin` is derived from, besides the
/// worktree's `.git` entry: every config file git would read (system, global,
/// the repo's shared config, a per-worktree config), every file those pull in
/// through `include`/`includeIf` (over-approximated: ALL referenced files are
/// folded in, whether or not the condition holds, so a change to any of them
/// invalidates), the legacy `remotes/origin` file, and the config-bearing
/// environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginInputs {
    files: Vec<(PathBuf, Option<String>)>,
    env: Vec<(String, String)>,
}

fn read_config(path: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() > MAX_CONFIG_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `path = <value>` lines of `[include]` / `[includeIf "..."]` sections.
fn include_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_include = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            let head = l.trim_start_matches('[').to_ascii_lowercase();
            in_include = head.starts_with("include");
            // `[include] path = x` on one line.
            if in_include
                && let Some((_, rest)) = l.split_once(']')
                && let Some(v) = key_value(rest, "path")
            {
                out.push(v);
            }
        } else if in_include && let Some(v) = key_value(l, "path") {
            out.push(v);
        }
    }
    out
}

fn key_value(line: &str, key: &str) -> Option<String> {
    let l = line.trim();
    if l.len() < key.len() || !l[..key.len()].eq_ignore_ascii_case(key) {
        return None;
    }
    let rest = l[key.len()..].trim_start();
    let v = rest.strip_prefix('=')?.trim();
    let v = v.trim_matches('"');
    (!v.is_empty()).then(|| v.to_string())
}

fn push_config(
    path: PathBuf,
    home: Option<&Path>,
    depth: usize,
    files: &mut Vec<(PathBuf, Option<String>)>,
) -> Option<()> {
    if files.iter().any(|(p, _)| *p == path) {
        return Some(());
    }
    let content = read_config(&path);
    // Present but unreadable/oversized is not a complete print.
    if content.is_none() && path.exists() {
        return None;
    }
    let includes = content.as_deref().map(include_paths).unwrap_or_default();
    files.push((path.clone(), content));
    if includes.is_empty() {
        return Some(());
    }
    if depth >= MAX_INCLUDE_DEPTH {
        return None;
    }
    for inc in includes {
        let target = if let Some(rest) = inc.strip_prefix("~/") {
            home?.join(rest)
        } else if inc.starts_with('~') {
            return None; // `~user/…`: not modelled
        } else if Path::new(&inc).is_absolute() {
            PathBuf::from(&inc)
        } else {
            path.parent()?.join(&inc)
        };
        push_config(target, home, depth + 1, files)?;
    }
    Some(())
}

/// Compute the [`OriginInputs`] for `dir`, reading the environment through
/// `env` (injectable for tests). `None` = cannot be fingerprinted completely.
pub fn origin_inputs(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<OriginInputs> {
    // These redirect git's own discovery; we cannot model them.
    for k in ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_CONFIG"] {
        if env(k).is_some() {
            return None;
        }
    }
    let home = env("HOME").map(PathBuf::from);
    let common = crate::util::git_common_dir(dir);
    let mut files = Vec::new();
    // system
    if env("GIT_CONFIG_NOSYSTEM").is_none() {
        let sys = env("GIT_CONFIG_SYSTEM")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/gitconfig"));
        push_config(sys, home.as_deref(), 0, &mut files)?;
    }
    // global
    match env("GIT_CONFIG_GLOBAL") {
        Some(g) => push_config(PathBuf::from(g), home.as_deref(), 0, &mut files)?,
        None => {
            if let Some(h) = &home {
                push_config(h.join(".gitconfig"), home.as_deref(), 0, &mut files)?;
            }
            let xdg = env("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(".config")));
            if let Some(x) = xdg {
                push_config(x.join("git/config"), home.as_deref(), 0, &mut files)?;
            }
        }
    }
    // repo: shared config, per-worktree config, legacy remotes file
    push_config(common.join("config"), home.as_deref(), 0, &mut files)?;
    push_config(
        common.join("config.worktree"),
        home.as_deref(),
        0,
        &mut files,
    )?;
    if let Some(gd) = git_dir(dir) {
        push_config(gd.join("config.worktree"), home.as_deref(), 0, &mut files)?;
    }
    push_config(
        common.join("remotes/origin"),
        home.as_deref(),
        0,
        &mut files,
    )?;
    let mut envs: Vec<(String, String)> = [
        "HOME",
        "XDG_CONFIG_HOME",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ]
    .into_iter()
    .filter_map(|k| env(k).map(|v| (k.to_string(), v)))
    .collect();
    // GIT_CONFIG_KEY_<n> / GIT_CONFIG_VALUE_<n> are open-ended: fold in as many
    // as COUNT announces.
    if let Some(n) = env("GIT_CONFIG_COUNT").and_then(|c| c.parse::<usize>().ok()) {
        for i in 0..n.min(256) {
            for p in ["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"] {
                let k = format!("{p}{i}");
                if let Some(v) = env(&k) {
                    envs.push((k, v));
                }
            }
        }
    }
    Some(OriginInputs { files, env: envs })
}

/// What `git rev-parse --abbrev-ref HEAD` is derived from: the worktree's HEAD,
/// and -- for a symbolic HEAD -- the existence of every ref its short name could
/// be ambiguous with (git prints `heads/x` when `x` is also a tag, a remote, ...)
/// or that must exist for it to resolve at all. Loose refs by stamp, packed ones
/// through the `packed-refs` stamp. The reftable backend keeps refs outside
/// files, so it is not modelled (no print).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadInputs {
    head: String,
    loose: Vec<(PathBuf, Stamp)>,
    packed: Stamp,
}

/// Compute [`HeadInputs`] for `dir`; `None` = cannot be modelled completely.
pub fn head_inputs(dir: &Path) -> Option<HeadInputs> {
    let gitdir = git_dir(dir)?;
    let common = crate::util::git_common_dir(dir);
    if common.join("reftable").exists() {
        return None;
    }
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let mut loose = Vec::new();
    if let Some(name) = head.trim().strip_prefix("ref: refs/heads/") {
        if name.is_empty() || name.split('/').any(|c| c == ".." || c == ".") {
            return None;
        }
        // git's ref_rev_parse_rules, minus the rule that names the ref itself
        // (which must exist for HEAD to resolve), plus that ref.
        for base in [&gitdir, &common] {
            loose.push((base.join(name), stamp(&base.join(name))));
        }
        for rel in [
            format!("refs/{name}"),
            format!("refs/tags/{name}"),
            format!("refs/heads/{name}"),
            format!("refs/remotes/{name}"),
            format!("refs/remotes/{name}/HEAD"),
        ] {
            let p = common.join(&rel);
            let s = stamp(&p);
            loose.push((p, s));
        }
    }
    Some(HeadInputs {
        head,
        loose,
        packed: stamp(&common.join("packed-refs")),
    })
}

/// Fingerprint for a forge checkout scope (branch + origin/base/push remote
/// identities) of a local worktree; `None` = bypass the memo.
pub fn checkout_fingerprint(
    dir: &Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<Fingerprint> {
    let origin = origin_fingerprint(dir, env)?;
    if origin == Fingerprint::Missing {
        return Some(origin);
    }
    Some(Fingerprint::Checkout {
        origin: Box::new(origin),
        head: head_inputs(dir)?,
    })
}

/// Inputs of the default-base probe (`symbolic-ref origin/HEAD`, then
/// `show-ref --verify refs/heads/<b>` for the candidates): the `origin/HEAD`
/// symref file by content, the loose heads that could be the answer, and the
/// packed-refs stamp. Cheap stats; equal prints mean the probe answers the same.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseRefsPrint {
    origin_head: Option<String>,
    loose: Vec<(PathBuf, Stamp)>,
    packed: Stamp,
}

/// Compute [`BaseRefsPrint`] for a local worktree, or `None` when it cannot be
/// modelled (no resolvable git dir, reftable).
pub fn base_refs_print(dir: &Path) -> Option<BaseRefsPrint> {
    git_dir(dir)?;
    let common = crate::util::git_common_dir(dir);
    if common.join("reftable").exists() {
        return None;
    }
    // `None` for an absent symref (no origin/HEAD) and for an oversized/unreadable one.
    let origin_head = read_config(&common.join("refs/remotes/origin/HEAD"));
    let mut names: Vec<String> = vec!["main".into(), "master".into()];
    if let Some(target) = origin_head
        .as_deref()
        .and_then(|c| c.trim().strip_prefix("ref: refs/remotes/origin/"))
        && !target.is_empty()
        && !target.split('/').any(|c| c == ".." || c == ".")
    {
        names.push(target.to_string());
    }
    let loose = names
        .into_iter()
        .map(|n| {
            let p = common.join("refs/heads").join(n);
            let s = stamp(&p);
            (p, s)
        })
        .collect();
    Some(BaseRefsPrint {
        origin_head,
        loose,
        packed: stamp(&common.join("packed-refs")),
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

/// Memoise `compute()` for `dir` under its [`fingerprint`] (see [`memoised_with`]). The fingerprint is
/// taken BEFORE `compute` runs, so a change racing the computation is stored
/// under the old fingerprint and re-derived on the next call (never stale).
pub fn memoised<V: Clone>(
    memo: &Mutex<Option<Memo<V>>>,
    cap: usize,
    dir: &Path,
    compute: impl FnOnce() -> V,
) -> V {
    memoised_with(memo, cap, dir, fingerprint(dir), compute)
}

/// [`memoised`] with a caller-computed fingerprint (`None` = bypass).
pub fn memoised_with<V: Clone>(
    memo: &Mutex<Option<Memo<V>>>,
    cap: usize,
    dir: &Path,
    fp: Option<Fingerprint>,
    compute: impl FnOnce() -> V,
) -> V {
    memoised_checked(memo, cap, dir, fp, || (compute(), true))
}

/// [`memoised_with`] for a computation that can tell a deterministic answer
/// from a transient failure: `compute` returns `(value, cacheable)`, and an
/// uncacheable value (a `git` that could not even be spawned -- EAGAIN, EMFILE
/// under load) is returned but never stored, so it cannot outlive the moment.
/// A non-zero exit from git is deterministic given the fingerprinted inputs and
/// IS cacheable; only "could not run it at all" is not.
pub fn memoised_checked<V: Clone>(
    memo: &Mutex<Option<Memo<V>>>,
    cap: usize,
    dir: &Path,
    fp: Option<Fingerprint>,
    compute: impl FnOnce() -> (V, bool),
) -> V {
    let Some(fp) = fp else {
        return compute().0;
    };
    {
        let guard = memo.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = guard.as_ref().and_then(|m| m.get(dir, &fp)) {
            return v;
        }
    }
    let (v, cacheable) = compute();
    if cacheable {
        memo.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(|| Memo::new(cap))
            .put(dir, fp, v.clone());
    }
    v
}

/// The fingerprint for the origin URL of `dir`; `None` = bypass the memo.
pub fn origin_fingerprint(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Option<Fingerprint> {
    let base = fingerprint(dir)?;
    // A missing path has no config to read; the Missing print alone keys it.
    if base == Fingerprint::Missing {
        return Some(base);
    }
    let inputs = origin_inputs(dir, env)?;
    Some(Fingerprint::Origin {
        base: Box::new(base),
        inputs,
    })
}

const ORIGIN_MEMO_CAP: usize = 512;

/// `git remote get-url origin` for a LOCAL worktree, memoised under
/// [`origin_fingerprint`] (THE-718: 45 forks/min on an idle one-repo session).
/// `compute` is the subprocess; it runs only on a miss or when the inputs
/// cannot be fingerprinted completely.
pub fn origin_url(dir: &Path, compute: impl FnOnce() -> (Option<String>, bool)) -> Option<String> {
    static MEMO: Mutex<Option<Memo<Option<String>>>> = Mutex::new(None);
    let fp = origin_fingerprint(dir, &|k| std::env::var(k).ok());
    memoised_checked(&MEMO, ORIGIN_MEMO_CAP, dir, fp, compute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tg-gmemo-{}-{}", std::process::id(), name));
        if p.exists() {
            std::fs::remove_dir_all(&p).unwrap();
        }
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

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let m: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn repo_with_config(base: &Path, config: &str) -> PathBuf {
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(repo.join(".git/config"), config).unwrap();
        repo
    }

    #[test]
    fn include_paths_reads_include_and_includeif_only() {
        let text = "[user]\n\tpath = no\n[include]\n\tpath = ~/a.cfg\n\
                    [includeIf \"gitdir:~/w/\"]\n\tPath = \"b.cfg\"\n[core]\n\tpath = no\n";
        assert_eq!(include_paths(text), vec!["~/a.cfg", "b.cfg"]);
        assert_eq!(include_paths("[include] path = c.cfg\n"), vec!["c.cfg"]);
        assert!(include_paths("[remote \"origin\"]\n\turl = x\n").is_empty());
    }

    #[test]
    fn origin_inputs_change_with_repo_global_and_included_config() {
        let base = tmp("origin-inputs");
        let home = base.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".gitconfig"), "[include]\n\tpath = extra.cfg\n").unwrap();
        std::fs::write(
            home.join("extra.cfg"),
            "[url \"ssh://x/\"]\n\tinsteadOf = https://x/\n",
        )
        .unwrap();
        let repo = repo_with_config(&base, "[remote \"origin\"]\n\turl = https://x/r\n");
        let h = home.to_str().unwrap();
        let env = env_of(&[("HOME", h), ("GIT_CONFIG_NOSYSTEM", "1")]);
        let a = origin_fingerprint(&repo, &env).expect("complete print");
        assert_eq!(a, origin_fingerprint(&repo, &env).unwrap());

        // Repo config edit.
        std::fs::write(
            repo.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://y/r\n",
        )
        .unwrap();
        let b = origin_fingerprint(&repo, &env).unwrap();
        assert_ne!(a, b);
        // A file pulled in by the global config's include (the insteadOf rewrite).
        std::fs::write(
            home.join("extra.cfg"),
            "[url \"ssh://z/\"]\n\tinsteadOf = https://y/\n",
        )
        .unwrap();
        let c = origin_fingerprint(&repo, &env).unwrap();
        assert_ne!(b, c);
        // Global config edit.
        std::fs::write(
            home.join(".gitconfig"),
            "[include]\n\tpath = extra.cfg\n# x\n",
        )
        .unwrap();
        assert_ne!(c, origin_fingerprint(&repo, &env).unwrap());
        // Per-worktree config appearing.
        let d = origin_fingerprint(&repo, &env).unwrap();
        std::fs::write(repo.join(".git/config.worktree"), "[remote \"origin\"]\n").unwrap();
        assert_ne!(d, origin_fingerprint(&repo, &env).unwrap());
        // Env knobs that change what git reads change the print too.
        let env2 = env_of(&[
            ("HOME", h),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_COUNT", "0"),
        ]);
        assert_ne!(
            origin_fingerprint(&repo, &env).unwrap(),
            origin_fingerprint(&repo, &env2).unwrap()
        );
    }

    #[test]
    fn origin_inputs_bypass_when_not_modelled() {
        let base = tmp("origin-bypass");
        let repo = repo_with_config(&base, "");
        let h = base.to_str().unwrap();
        for k in ["GIT_DIR", "GIT_COMMON_DIR", "GIT_WORK_TREE", "GIT_CONFIG"] {
            let env = env_of(&[("HOME", h), (k, "/x")]);
            assert_eq!(origin_fingerprint(&repo, &env), None, "{k}");
        }
        // `~user/` include, an include cycle past the depth cap, and a config
        // that is present but unreadable as a file are not complete prints.
        std::fs::write(base.join(".gitconfig"), "[include]\n\tpath = ~bob/x\n").unwrap();
        let env = env_of(&[("HOME", h), ("GIT_CONFIG_NOSYSTEM", "1")]);
        assert_eq!(origin_fingerprint(&repo, &env), None);
        std::fs::write(base.join(".gitconfig"), "[include]\n\tpath = loop.cfg\n").unwrap();
        std::fs::write(base.join("loop.cfg"), "[include]\n\tpath = loop.cfg\n").unwrap();
        // Self-include is deduplicated, so it terminates with a complete print.
        assert!(origin_fingerprint(&repo, &env).is_some());
        // Not a worktree at all.
        assert_eq!(
            origin_fingerprint(&base.join("nothing-here"), &env),
            Some(Fingerprint::Missing)
        );
        assert_eq!(origin_fingerprint(&base, &env), None);
    }

    #[test]
    fn origin_url_memoises_and_follows_set_url() {
        let base = tmp("origin-url");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            assert!(
                crate::util::git_cmd(&repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        git(&["init", "-q"]);
        git(&["remote", "add", "origin", "https://example.test/a/b.git"]);
        let spawns = std::cell::Cell::new(0);
        let run = || {
            origin_url(&repo, || {
                spawns.set(spawns.get() + 1);
                (
                    crate::util::git_out(&repo, &["remote", "get-url", "origin"]),
                    true,
                )
            })
        };
        assert_eq!(run().as_deref(), Some("https://example.test/a/b.git"));
        assert_eq!(run().as_deref(), Some("https://example.test/a/b.git"));
        if origin_fingerprint(&repo, &|k| std::env::var(k).ok()).is_some() {
            assert_eq!(spawns.get(), 1, "second call is a memo hit");
        }
        git(&[
            "remote",
            "set-url",
            "origin",
            "https://example.test/c/d.git",
        ]);
        assert_eq!(run().as_deref(), Some("https://example.test/c/d.git"));
        git(&["remote", "remove", "origin"]);
        assert_eq!(run(), None, "removal is seen, not served from the memo");
    }

    #[test]
    fn head_inputs_see_ambiguity_and_head_moves() {
        let base = tmp("head-inputs");
        let repo = repo_with_config(&base, "");
        let a = head_inputs(&repo).expect("modelled");
        assert_eq!(a, head_inputs(&repo).unwrap());
        // A tag appearing with the branch's short name makes `--abbrev-ref`
        // print `heads/main`: the print must move.
        std::fs::create_dir_all(repo.join(".git/refs/tags")).unwrap();
        std::fs::write(repo.join(".git/refs/tags/main"), "0\n").unwrap();
        let b = head_inputs(&repo).unwrap();
        assert_ne!(a, b);
        // HEAD pointing elsewhere.
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/other\n").unwrap();
        assert_ne!(b, head_inputs(&repo).unwrap());
        // Detached HEAD carries no ref candidates, just the content.
        std::fs::write(
            repo.join(".git/HEAD"),
            "0123456789012345678901234567890123456789\n",
        )
        .unwrap();
        assert!(head_inputs(&repo).unwrap().loose.is_empty());
        // packed-refs appearing moves it.
        let c = head_inputs(&repo).unwrap();
        std::fs::write(repo.join(".git/packed-refs"), "# pack-refs\n").unwrap();
        assert_ne!(c, head_inputs(&repo).unwrap());
        // Not modelled: reftable, a path traversal in the branch name, no gitdir.
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/../x\n").unwrap();
        assert!(head_inputs(&repo).is_none());
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(repo.join(".git/reftable")).unwrap();
        assert!(head_inputs(&repo).is_none());
        assert!(head_inputs(&base.join("nothing")).is_none());
    }

    #[test]
    fn checkout_fingerprint_combines_config_and_head_inputs() {
        let base = tmp("checkout-fp");
        let repo = repo_with_config(&base, "[remote \"origin\"]\n\turl = https://x/r\n");
        let h = base.to_str().unwrap();
        let env = env_of(&[("HOME", h), ("GIT_CONFIG_NOSYSTEM", "1")]);
        let a = checkout_fingerprint(&repo, &env).expect("complete");
        assert!(matches!(a, Fingerprint::Checkout { .. }));
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/side\n").unwrap();
        assert_ne!(
            a,
            checkout_fingerprint(&repo, &env).unwrap(),
            "branch switch"
        );
        let b = checkout_fingerprint(&repo, &env).unwrap();
        std::fs::write(
            repo.join(".git/config"),
            "[remote \"origin\"]\n\turl = https://y/r\n",
        )
        .unwrap();
        assert_ne!(
            b,
            checkout_fingerprint(&repo, &env).unwrap(),
            "remote retarget"
        );
        assert_eq!(
            checkout_fingerprint(&base.join("gone"), &env),
            Some(Fingerprint::Missing)
        );
        let bypass = env_of(&[("HOME", h), ("GIT_DIR", "/x")]);
        assert_eq!(checkout_fingerprint(&repo, &bypass), None);
    }

    #[test]
    fn base_refs_print_tracks_origin_head_and_candidate_heads() {
        let base = tmp("base-refs");
        let repo = repo_with_config(&base, "");
        let a = base_refs_print(&repo).expect("modelled");
        assert_eq!(a, base_refs_print(&repo).unwrap());
        std::fs::create_dir_all(repo.join(".git/refs/remotes/origin")).unwrap();
        std::fs::write(
            repo.join(".git/refs/remotes/origin/HEAD"),
            "ref: refs/remotes/origin/trunk\n",
        )
        .unwrap();
        let b = base_refs_print(&repo).unwrap();
        assert_ne!(a, b, "origin/HEAD appearing");
        // The symref's target is now a candidate: a local `trunk` branch moves it.
        std::fs::create_dir_all(repo.join(".git/refs/heads")).unwrap();
        std::fs::write(repo.join(".git/refs/heads/trunk"), "0\n").unwrap();
        let c = base_refs_print(&repo).unwrap();
        assert_ne!(b, c);
        std::fs::write(repo.join(".git/refs/heads/main"), "0\n").unwrap();
        assert_ne!(c, base_refs_print(&repo).unwrap(), "main appearing");
        std::fs::write(repo.join(".git/packed-refs"), "# pack-refs\n").unwrap();
        let d = base_refs_print(&repo).unwrap();
        std::fs::write(
            repo.join(".git/packed-refs"),
            "# pack-refs\n0 refs/heads/master\n",
        )
        .unwrap();
        assert_ne!(d, base_refs_print(&repo).unwrap(), "packed-refs rewrite");
        std::fs::create_dir_all(repo.join(".git/reftable")).unwrap();
        assert!(base_refs_print(&repo).is_none());
        assert!(base_refs_print(&base.join("nothing")).is_none());
    }

    #[test]
    fn memoised_checked_never_stores_an_uncacheable_answer() {
        let base = tmp("checked");
        let dir = base.join("gone");
        let memo: Mutex<Option<Memo<u32>>> = Mutex::new(None);
        let calls = std::cell::Cell::new(0);
        let run = |cacheable: bool| {
            memoised_checked(&memo, 4, &dir, fingerprint(&dir), || {
                calls.set(calls.get() + 1);
                (calls.get(), cacheable)
            })
        };
        assert_eq!(run(false), 1);
        assert_eq!(run(true), 2, "the transient answer was not remembered");
        assert_eq!(run(true), 2, "a cacheable one is");
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
