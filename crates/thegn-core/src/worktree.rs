//! Branch-name generation, base-branch resolution, and worktree add/remove.

use crate::config::{Config, NameScheme, WorktreeMode};
use crate::identity::{BranchRef, ExactPath, IdentityError};
use crate::msg;
use crate::repo;
use crate::util;
use std::path::{Path, PathBuf};

const MAX_GIT_IDENTITY_OUTPUT: usize = 64 * 1024;

/// Exact Git metadata for one registered worktree. The paths retain their
/// native bytes and the branch is kept separate from its display label. Git
/// remains authoritative; this is an inspection result, not a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeIdentity {
    pub common_dir: ExactPath,
    pub admin_dir: ExactPath,
    pub registered_path: ExactPath,
    pub branch: Option<BranchRef>,
}

impl GitWorktreeIdentity {
    /// Exact administrative path bytes used as the stable Git-instance input.
    pub fn admin_id(&self) -> &[u8] {
        self.admin_dir.as_bytes()
    }
}

/// Inspect the one Git worktree registered at `path` without converting
/// porcelain output through UTF-8. A relative or symlink alias is accepted for
/// lookup, but the returned path is Git's exact registered path.
pub fn inspect_registered(root: &Path, path: &Path) -> Result<GitWorktreeIdentity, IdentityError> {
    let requested = std::fs::canonicalize(path).map_err(|_| IdentityError::GitProbeFailed {
        operation: "canonicalize requested worktree",
    })?;
    let porcelain = util::git_stdout_bounded(
        root,
        &["worktree", "list", "--porcelain", "-z"],
        MAX_GIT_IDENTITY_OUTPUT,
    )
    .map_err(|_| IdentityError::GitProbeFailed {
        operation: "git worktree list --porcelain -z",
    })?;
    let entries = parse_worktree_identity_records(&porcelain)?;
    let mut matches = entries
        .into_iter()
        .filter(|entry| {
            std::fs::canonicalize(entry.path.as_path())
                .map(|candidate| candidate == requested)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(IdentityError::NotRegistered);
    }
    if matches.len() != 1 {
        return Err(IdentityError::Ambiguous {
            kind: "Git worktree registration",
        });
    }
    let entry = matches.pop().expect("one match checked");
    let common_dir = repo::canonical_common_dir(entry.path.as_path())?;
    let admin_dir = exact_git_path(
        &entry.path,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
        "rev-parse --git-dir",
    )?;
    Ok(GitWorktreeIdentity {
        common_dir,
        admin_dir,
        registered_path: entry.path,
        branch: entry.branch,
    })
}

#[derive(Debug)]
struct WorktreeIdentityRecord {
    path: ExactPath,
    branch: Option<BranchRef>,
}

fn exact_git_path(
    dir: &ExactPath,
    args: &[&str],
    operation: &'static str,
) -> Result<ExactPath, IdentityError> {
    let output = util::git_stdout_bounded(dir.as_path(), args, MAX_GIT_IDENTITY_OUTPUT)
        .map_err(|_| IdentityError::GitProbeFailed { operation })?;
    let raw = trim_git_line(&output).ok_or(IdentityError::GitProbeFailed { operation })?;
    ExactPath::from_git_bytes(raw)
}

fn trim_git_line(output: &[u8]) -> Option<&[u8]> {
    let mut end = output.len();
    if output.get(end.wrapping_sub(1)) == Some(&b'\n') {
        end -= 1;
    }
    if output.get(end.wrapping_sub(1)) == Some(&b'\r') {
        end -= 1;
    }
    (end > 0).then_some(&output[..end])
}

fn parse_worktree_identity_records(
    porcelain: &[u8],
) -> Result<Vec<WorktreeIdentityRecord>, IdentityError> {
    let mut records = Vec::new();
    let mut current_path: Option<ExactPath> = None;
    let mut current_branch: Option<BranchRef> = None;
    let mut finish = |records: &mut Vec<WorktreeIdentityRecord>,
                      current_path: &mut Option<ExactPath>,
                      current_branch: &mut Option<BranchRef>| {
        if let Some(path) = current_path.take() {
            records.push(WorktreeIdentityRecord {
                path,
                branch: current_branch.take(),
            });
        }
    };

    // Git's -z form terminates fields with NUL on supported versions. Only
    // fall back to line records when no NUL is present; otherwise a valid path
    // containing a newline would be silently split into a different claimant.
    let fields: Vec<&[u8]> = if porcelain.contains(&0) {
        porcelain.split(|byte| *byte == 0).collect()
    } else {
        porcelain.split(|byte| *byte == b'\n').collect()
    };
    for field in fields {
        if field.is_empty() {
            continue;
        }
        if let Some(raw) = field.strip_prefix(b"worktree ") {
            finish(&mut records, &mut current_path, &mut current_branch);
            current_path = Some(ExactPath::from_git_bytes(raw)?);
        } else if let Some(raw) = field.strip_prefix(b"branch refs/heads/") {
            current_branch = Some(BranchRef::from_bytes(raw)?);
        } else if field == b"detached" {
            current_branch = None;
        }
    }
    finish(&mut records, &mut current_path, &mut current_branch);
    Ok(records)
}

const ADJ: &[&str] = &[
    // Original small set + expansion
    "brisk", "calm", "clever", "bold", "swift", "quiet", "keen", "lucky", "nimble", "warm", "vivid",
    "amber", "cosmic", "dusty", "eager", "fancy", "gentle", "hardy", "ideal", "jolly", "merry",
    "noble", "proud", "brave", "bright", "chill", "crisp", "dandy", "dizzy", "fierce", "flaky",
    "fresh", "frosty", "grand", "great", "happy", "heavy", "jiffy", "juicy", "laser", "light",
    "lively", "lofty", "magic", "mighty", "neat", "nifty", "plump", "plush", "prime", "quick",
    "rad", "rapid", "sharp", "shiny", "sleek", "slick", "smart", "snug", "solid", "spark", "spicy",
    "stout", "sturdy", "sunny", "super", "sweet", "tough", "trusty", "valid", "vast", "wild",
    "witty", "zesty",
];
const NOUN: &[&str] = &[
    // Original small set + expansion
    "otter", "falcon", "maple", "cedar", "comet", "harbor", "meadow", "pebble", "willow", "ember",
    "lark", "quartz", "raven", "cobalt", "finch", "grove", "heron", "lotus", "marlin", "onyx",
    "pine", "reef", "sage", "acorn", "alpine", "anchor", "apple", "armor", "arrow", "badger",
    "bamboo", "basil", "beacon", "bear", "beech", "bison", "blade", "breeze", "brook", "canyon",
    "castle", "cherry", "cliff", "cloud", "clover", "coast", "copper", "coral", "crane", "crest",
    "crown", "crystal", "dagger", "dawn", "delta", "desert", "dragon", "eagle", "echo", "elm",
    "feather", "fern", "flame", "flint", "forest", "fox", "frost", "galaxy", "garden", "gecko",
    "glacier", "glade", "glen", "hawk", "hazel", "heart", "hedge", "hollow", "hound", "husky",
    "island", "ivy", "jade", "jaguar", "jewel", "jungle", "koala", "lake", "leaf", "lemon",
    "leopard", "lily", "lion", "lizard", "lynx", "mango", "marble", "marsh", "maze", "melon",
    "meteor", "moon", "moss", "mountain", "nebula", "nectar", "nest", "nova", "oak", "ocean",
    "olive", "opal", "orbit", "orchid", "owl", "panda", "panther", "parrot", "peak", "pearl",
    "petal", "pilot", "planet", "plum", "pony", "pool", "pulse", "puma", "radar", "rain", "rhino",
    "ridge", "river", "robin", "rocket", "rose", "ruby", "shadow", "shark", "shield", "sky",
    "slate", "snow", "solar", "spark", "sparrow", "sphere", "spider", "spire", "spring", "star",
    "stone", "storm", "stream", "summit", "sun", "swan", "sword", "tiger", "timber", "topaz",
    "tower", "trail", "tulip", "tundra", "valley", "velvet", "viper", "vision", "volcano",
    "walnut", "water", "wave", "whale", "wind", "wing", "wolf", "zebra", "zenith", "zephyr",
];

/// Whether an exact local branch `refs/heads/{branch}` exists in `root`. Exact
/// (not the hierarchical [`BranchSet::taken`] collision check): the batched
/// cross-repo create uses it to decide attach-vs-create per member, where
/// identity is literal branch-name equality.
pub fn branch_exists(root: &Path, branch: &str) -> bool {
    util::git_ok(
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// Every branch name a new branch must avoid: local heads plus branches
/// checked out in any worktree. Loaded with two git subprocesses total so
/// collision checks are pure lookups (the old per-candidate probing ran two
/// subprocesses per attempt).
pub struct BranchSet {
    taken: std::collections::HashSet<String>,
}

impl BranchSet {
    pub fn load(root: &Path) -> Self {
        let mut taken = std::collections::HashSet::new();
        if let Some(out) = util::git_out(
            root,
            &["for-each-ref", "refs/heads", "--format=%(refname:short)"],
        ) {
            taken.extend(out.lines().map(str::to_string).filter(|l| !l.is_empty()));
        }
        if let Some(out) = util::git_out(root, &["worktree", "list", "--porcelain"]) {
            taken.extend(
                out.lines()
                    .filter_map(|l| l.strip_prefix("branch refs/heads/"))
                    .map(str::to_string),
            );
        }
        Self { taken }
    }

    pub fn from_names<I: IntoIterator<Item = String>>(names: I) -> Self {
        Self {
            taken: names.into_iter().collect(),
        }
    }

    pub fn taken(&self, branch: &str) -> bool {
        if self.taken.contains(branch) {
            return true;
        }
        let b_slash = format!("{branch}/");
        self.taken
            .iter()
            .any(|t| t.starts_with(&b_slash) || branch.starts_with(&format!("{t}/")))
    }

    /// Drop a name from the set — used when renaming a worktree so its own
    /// current branch doesn't count as a collision against the new name.
    pub fn remove(&mut self, branch: &str) {
        self.taken.remove(branch);
    }
}

/// A random `adj-noun` slug (no branch prefix). Pure — seeded with the pid +
/// wall-clock so concurrent creates don't collide on the same candidate. Shared
/// by worktree branch naming and the new-terminal wizard.
pub fn random_pair() -> String {
    let seed = (std::process::id() as u64).wrapping_add(util::now() as u64);
    let adj = ADJ[(seed % ADJ.len() as u64) as usize];
    let noun = NOUN[((seed / 7 + 1) % NOUN.len() as u64) as usize];
    format!("{adj}-{noun}")
}

/// The branch-name candidate for an unnamed worktree: `{prefix}{adj}-{noun}`
/// (or `{prefix}pane` under the numbered scheme). Pure — no git, so a wizard
/// prefill can be computed synchronously on the UI loop.
pub fn candidate_name(cfg: &Config) -> String {
    let prefix = &cfg.branch_prefix;
    if cfg.name_scheme == NameScheme::Numbered {
        return format!("{prefix}pane");
    }
    format!("{prefix}{}", random_pair())
}

/// The branch-name base for a human-provided name: `{prefix}{slug}`.
pub fn human_base(human: &str, cfg: &Config) -> String {
    format!("{}{}", cfg.branch_prefix, util::slugify(human))
}

/// Suffix `base` with `-1`, `-2`, … until it avoids every taken name.
pub fn dedupe(base: &str, taken: &BranchSet) -> String {
    let mut candidate = base.to_string();
    let mut n = 0;
    while taken.taken(&candidate) {
        n += 1;
        candidate = format!("{base}-{n}");
    }
    candidate
}

/// Generate a collision-free branch name. `human` is an optional friendly name.
pub fn branch_name(root: &Path, human: Option<&str>, cfg: &Config) -> String {
    let base = match human {
        Some(h) => human_base(h, cfg),
        None => candidate_name(cfg),
    };
    dedupe(&base, &BranchSet::load(root))
}

/// Best-effort default branch: origin/HEAD, else main, else master, else HEAD.
pub fn default_branch(root: &Path) -> String {
    if let Some(r) = util::git_out(
        root,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        return r.strip_prefix("origin/").unwrap_or(&r).to_string();
    }
    for b in ["main", "master"] {
        if branch_exists(root, b) {
            return b.to_string();
        }
    }
    "HEAD".to_string()
}

/// Resolve the base ref to branch a new worktree off (honours `base_branch`).
pub fn resolve_base(root: &Path, cfg: &Config) -> String {
    if cfg.base_branch != "auto" {
        return cfg.base_branch.clone();
    }
    if repo::is_bare(root) {
        return default_branch(root);
    }
    if let Some(head) = util::git_out(root, &["symbolic-ref", "--quiet", "--short", "HEAD"]) {
        return head;
    }
    let def = default_branch(root);
    msg::warn(&format!(
        "main worktree is on a detached HEAD; basing new worktree off '{def}'"
    ));
    def
}

/// Compute the worktree directory path for a branch.
pub fn worktree_path(root: &Path, branch: &str, cfg: &Config) -> PathBuf {
    let slug = util::slugify(branch);
    if cfg.worktree_mode == WorktreeMode::InRepo {
        root.join(".worktrees").join(slug)
    } else {
        Path::new(&cfg.worktrees_dir)
            .join(repo::repo_name(root))
            .join(slug)
    }
}

/// Create a worktree. Returns false on failure (caller decides how to recover)
/// rather than killing the pane.
pub fn add(root: &Path, branch: &str, base: &str, path: &Path, cfg: &Config) -> bool {
    if let Err(e) = add_checked(root, branch, base, path, cfg) {
        msg::warn(&e);
        return false;
    }
    true
}

/// Details from a failed worktree add. `branch_created` is measured while the
/// repository mutation lock is held, so rollback can avoid deleting a branch
/// that pre-dated this attempt (including a concurrent add race).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddError {
    pub message: String,
    pub branch_created: bool,
}

/// [`add`] with the failure reason returned instead of warned, for callers
/// that surface errors in their own UI (the new-worktree progress overlay).
pub fn add_checked(
    root: &Path,
    branch: &str,
    base: &str,
    path: &Path,
    cfg: &Config,
) -> Result<(), String> {
    add_checked_with_state(root, branch, base, path, cfg).map_err(|error| error.message)
}

/// Like [`add_checked`], but preserves whether this failed attempt created the
/// branch. The check and `git worktree add` share one mutation lock.
pub fn add_checked_with_state(
    root: &Path,
    branch: &str,
    base: &str,
    path: &Path,
    cfg: &Config,
) -> Result<(), AddError> {
    if cfg.worktree_mode == WorktreeMode::InRepo {
        // Keep .worktrees out of git locally without touching tracked .gitignore.
        let excl = root.join(".git/info/exclude");
        if let Ok(contents) = std::fs::read_to_string(&excl)
            && !contents.lines().any(|l| l == ".worktrees/")
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&excl) {
                let _ = writeln!(f, ".worktrees/"); // best-effort: exclude-file append: advisory; worst case .worktrees/ shows in git status
            }
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent); // best-effort: dir prep: a later write reports the real failure
    }
    // Serialize against other thegn/agent git mutations on this repo's shared
    // `.git` (held until the subprocess returns).
    let _lock = util::lock_git_mutations(root);
    let branch_preexisted = branch_exists(root, branch);
    let out = util::git_cmd(root)
        .args(["worktree", "add", "--quiet", "-b", branch])
        .arg(path)
        .arg(base)
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            Err(AddError {
                message: format!(
                    "git worktree add failed (branch={branch} base={base}): {}",
                    stderr.trim()
                ),
                branch_created: !branch_preexisted && branch_exists(root, branch),
            })
        }
        Err(e) => Err(AddError {
            message: format!("could not run git worktree add: {e}"),
            branch_created: !branch_preexisted && branch_exists(root, branch),
        }),
    }
}

/// Delete a removed remote/provider worktree's checkout on the box over ssh.
/// The local directory has already been removed by Git before this is called;
/// deleting it again would introduce a pathname-reuse race in which unrelated
/// replacement data could be recursively removed. The shared base image and
/// warm volumes stay. Call before the DB's `worktrees.location` row is
/// forgotten, otherwise the remote target cannot be resolved.
pub fn purge_worktree_files(path: &Path) {
    crate::remote::GitLoc::for_worktree(path).remove_remote_dir();
}

/// Remove a worktree and optionally delete its branch. Returns whether the
/// worktree directory was actually removed. Callers that cache worktree state
/// (e.g. the merge lifecycle) MUST NOT drop their rows when this returns `false`
/// — a read-only mount or uncommitted changes can leave the worktree on disk,
/// and dropping its row would orphan it out of its sidebar folder (under home).
///
/// A directory can survive after an older cleanup removed its Git worktree
/// registration (or after that registration was lost during a partial
/// operation). Git can no longer authenticate such a directory as ours, so an
/// existing unregistered path is retained for manual inspection. Its pathname
/// may have been reused for unrelated user data since the registration was
/// lost; absence of a `.git` marker is not proof of ownership. An already
/// absent path still counts as removed.
pub fn remove(root: &Path, path: &Path, branch: &str, delete_branch: bool) -> bool {
    let _lock = util::lock_git_mutations(root);
    let path_s = path.to_string_lossy();
    let mut removed = util::git_ok(root, &["worktree", "remove", &path_s])
        || util::git_ok(root, &["worktree", "remove", "--force", &path_s]);
    if !removed {
        match git_worktree_registration(root, path) {
            Some(false) if !path.exists() && !worktree_paths_equal(root, path) => {
                removed = true;
                msg::info(&format!(
                    "Git-unregistered worktree directory at {} was already absent",
                    path.display()
                ));
            }
            Some(false) => msg::warn(&format!(
                "kept Git-unregistered path at {}: Git can no longer prove that this directory belongs to the worktree",
                path.display()
            )),
            Some(true) => msg::warn(&format!(
                "could not remove registered worktree at {} (uncommitted changes or read-only mount?)",
                path.display()
            )),
            None => msg::warn(&format!(
                "could not remove worktree at {} (could not verify Git worktree registry)",
                path.display()
            )),
        }
    }
    // The branch delete is gated on the removal having ACTUALLY happened. It
    // used to run unconditionally, so a failed removal (a read-only mount is the
    // common one) left the worktree sitting on disk with its branch ref deleted
    // out from under it — the worst of both outcomes, and `git branch -D` is
    // force, so the ref went without a merged-ness check.
    if delete_branch && !branch.is_empty() {
        if !removed {
            msg::warn(&format!(
                "kept branch {branch}: its worktree at {} is still on disk",
                path.display()
            ));
        } else if !util::git_ok(root, &["branch", "-D", branch]) {
            msg::warn(&format!("could not delete branch {branch}"));
        }
    }
    removed
}

/// Ask Git whether `path` is one of the repository's registered worktrees.
/// `None` is deliberately fail-closed: a broken/unreadable repository is not
/// permission to recursively delete a directory.
fn git_worktree_registration(root: &Path, path: &Path) -> Option<bool> {
    let output = util::git_cmd(root)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let porcelain = String::from_utf8_lossy(&output.stdout);
    Some(
        util::parse_worktree_branches(&porcelain)
            .iter()
            .any(|(listed, _)| worktree_paths_equal(Path::new(listed), path)),
    )
}

fn worktree_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
        || (left.exists()
            && right.exists()
            && std::fs::canonicalize(left).ok() == std::fs::canonicalize(right).ok())
}

/// Reclaim a worktree's build artifacts (`target/`) while keeping the checkout
/// intact. Distinct from [`remove`], which deletes the whole worktree via `git
/// worktree remove`. Returns the bytes reclaimed.
///
/// Prefers `cargo clean` when a `Cargo.toml` and `cargo` are present: it
/// acquires cargo's build-directory lock, so a concurrent build *serializes*
/// (blocks) rather than racing a half-deleted tree. Falls back to removing
/// `target/` directly for non-cargo projects or when `cargo` is absent. A
/// missing `target/` is a no-op returning 0.
///
/// Callers are responsible for the safety guards (never the active worktree,
/// never one with a running build) — this function only does the reclaim.
pub fn clean_target(path: &Path) -> std::io::Result<u64> {
    let target = path.join("target");
    if !target.is_dir() {
        return Ok(0);
    }
    let before = crate::disk::measure_worktree(path).target_bytes;

    let has_cargo = path.join("Cargo.toml").is_file();
    let cleaned = if has_cargo && util::have("cargo") {
        // `cargo clean` takes the build lock, so it can't corrupt a concurrent
        // build — it waits. Fall back to rm if cargo errors (e.g. no toolchain).
        std::process::Command::new("cargo")
            .arg("clean")
            .current_dir(path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        false
    };

    if !cleaned && target.is_dir() {
        std::fs::remove_dir_all(&target)?;
    }
    Ok(before)
}

/// Rename a worktree's branch (`git branch -m`) and move its checkout to the
/// path implied by the new branch name (`git worktree move`). Returns the new
/// on-disk path on success, or the reason it failed (the caller keeps the old
/// name and surfaces the message). Both steps must succeed; a failed move after
/// a successful branch rename leaves the branch renamed but the checkout in
/// place — reported so the caller can warn.
///
/// Shared by the new-worktree wizard's finalize-name step and the sidebar's
/// post-creation "rename worktree" action.
pub fn rename(
    root: &Path,
    old_path: &Path,
    old_branch: &str,
    new_branch: &str,
    cfg: &Config,
) -> Result<PathBuf, String> {
    if new_branch.is_empty() {
        return Err("new branch name is empty".into());
    }
    if new_branch == old_branch {
        return Ok(old_path.to_path_buf());
    }
    let new_path = worktree_path(root, new_branch, cfg);
    if let Some(parent) = new_path.parent() {
        let _ = std::fs::create_dir_all(parent); // best-effort: dir prep: a later write reports the real failure
    }
    if !util::git_ok(root, &["branch", "-m", old_branch, new_branch]) {
        return Err(format!(
            "could not rename branch {old_branch} → {new_branch}"
        ));
    }
    if !util::git_ok(
        root,
        &[
            "worktree",
            "move",
            &old_path.to_string_lossy(),
            &new_path.to_string_lossy(),
        ],
    ) {
        return Err(format!(
            "branch renamed to {new_branch}, but moving the worktree to {} failed",
            new_path.display()
        ));
    }
    Ok(new_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tg-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test setup: fresh scratch dir
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t.t"],
            &["config", "user.name", "t"],
            // Hermetic against the developer's global signing setup: a
            // `commit.gpgsign = true` in ~/.gitconfig would make every test
            // commit require a live gpg agent.
            &["config", "commit.gpgsign", "false"],
            &["commit", "--allow-empty", "-q", "-m", "init"],
        ] {
            assert!(util::git_cmd(&dir).args(args).status().unwrap().success());
        }
        dir
    }

    #[test]
    fn candidate_name_uses_prefix_and_scheme() {
        let mut cfg = Config {
            branch_prefix: "x/".into(),
            ..Default::default()
        };
        let name = candidate_name(&cfg);
        let tail = name.strip_prefix("x/").expect("prefix");
        let (adj, noun) = tail.split_once('-').expect("adj-noun");
        assert!(ADJ.contains(&adj));
        assert!(NOUN.contains(&noun));

        cfg.name_scheme = NameScheme::Numbered;
        assert_eq!(candidate_name(&cfg), "x/pane");
    }

    #[test]
    fn human_base_slugifies() {
        let cfg = Config::default();
        assert_eq!(
            human_base("My Fix!", &cfg),
            format!("{}my-fix", cfg.branch_prefix)
        );
    }

    #[test]
    fn dedupe_suffixes_until_free() {
        let taken = BranchSet::from_names(["a".into(), "a-1".into()]);
        assert_eq!(dedupe("a", &taken), "a-2");
        assert_eq!(dedupe("b", &taken), "b");
    }

    #[test]
    fn branch_set_load_sees_heads_and_worktree_branches() {
        let repo = temp_repo("set");
        assert!(util::git_ok(&repo, &["branch", "feature"]));
        let wt = repo.join(".wt-other");
        assert!(util::git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "other",
                &wt.to_string_lossy(),
                "main",
            ]
        ));
        let set = BranchSet::load(&repo);
        assert!(set.taken("main"));
        assert!(set.taken("feature"));
        assert!(set.taken("other"));
        assert!(!set.taken("free"));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_name_dedupes_against_repo() {
        let repo = temp_repo("name");
        let cfg = Config::default();
        let first = branch_name(&repo, Some("dup"), &cfg);
        assert_eq!(first, format!("{}dup", cfg.branch_prefix));
        assert!(util::git_ok(&repo, &["branch", &first]));
        assert_eq!(branch_name(&repo, Some("dup"), &cfg), format!("{first}-1"));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn failed_add_reports_preexisting_branch_without_claiming_it() {
        let repo = temp_repo("add-state");
        let path = repo.join(".wt-existing-branch");
        let error = add_checked_with_state(&repo, "main", "main", &path, &Config::default())
            .expect_err("git must reject creating an existing branch");
        assert!(!error.branch_created);
        assert!(branch_exists(&repo, "main"));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn remove_keeps_a_git_unregistered_directory_without_proof_of_ownership() {
        let repo = temp_repo("remove-orphan");
        let path = repo.join(".wt-orphan");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("leftover.txt"), "stale").unwrap();
        assert!(util::git_ok(&repo, &["branch", "orphan"]));

        assert!(!remove(&repo, &path, "orphan", true));
        assert!(
            path.exists(),
            "an unregistered path may contain replacement data"
        );
        assert!(
            branch_exists(&repo, "orphan"),
            "branch deletion is refused while the path remains"
        );
        assert!(repo.exists(), "the repository root is preserved");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn remove_keeps_an_unregistered_directory_for_a_bare_repo() {
        let repo = std::env::temp_dir().join(format!("tg-wt-bare-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["init", "--bare", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let path = repo.join(".wt-orphan");
        std::fs::create_dir_all(&path).unwrap();

        assert!(!remove(&repo, &path, "", false));
        assert!(path.exists(), "the unauthenticated directory is retained");
        assert!(repo.exists(), "the bare repository root is preserved");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn remove_accepts_an_already_absent_unregistered_path() {
        let repo = temp_repo("remove-absent");
        let path = repo.join(".wt-gone");
        assert!(util::git_ok(&repo, &["branch", "gone"]));

        assert!(remove(&repo, &path, "gone", true));
        assert!(!branch_exists(&repo, "gone"));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn remove_keeps_an_unregistered_directory_with_a_git_marker() {
        let repo = temp_repo("remove-unregistered-checkout");
        let path = repo.join(".wt-marker");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(".git"), "gitdir: /somewhere").unwrap();

        assert!(!remove(&repo, &path, "", false));
        assert!(path.exists(), "a possible checkout is retained");
        assert!(
            path.join(".git").exists(),
            "the checkout marker is retained"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn clean_target_removes_artifacts_keeps_source() {
        // A non-cargo dir so clean_target takes the rm fallback (no toolchain
        // dependency in the test). cargo-clean path is exercised by smoke/CI.
        let dir = std::env::temp_dir().join(format!("tg-clean-{}", std::process::id()));
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::write(dir.join("src/main.rs"), b"fn main(){}").unwrap();
        std::fs::write(dir.join("target/debug/bin"), vec![0u8; 4096]).unwrap();

        let reclaimed = clean_target(&dir).unwrap();
        assert!(reclaimed >= 4096, "reports bytes reclaimed");
        assert!(!dir.join("target").exists(), "target/ removed");
        assert!(dir.join("src/main.rs").exists(), "source kept");

        // No target/ → no-op, returns 0.
        assert_eq!(clean_target(&dir).unwrap(), 0);
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_moves_branch_and_checkout() {
        let repo = temp_repo("rename");
        // Keep worktrees inside the temp repo so nothing leaks into the real
        // `~/.thegn/worktrees` (the default `worktrees_dir`).
        let cfg = Config {
            worktrees_dir: repo.join(".wt").to_string_lossy().into_owned(),
            ..Default::default()
        };
        let old_branch = "old-feat";
        let path = worktree_path(&repo, old_branch, &cfg);
        add_checked(&repo, old_branch, "main", &path, &cfg).unwrap();
        assert!(path.is_dir());

        let new_branch = "new-feat";
        let new_path = rename(&repo, &path, old_branch, new_branch, &cfg).unwrap();
        // New checkout exists; the branch was renamed (old gone, new present).
        assert!(new_path.is_dir(), "moved checkout exists");
        assert_ne!(new_path, path, "path changed with the branch name");
        let set = BranchSet::load(&repo);
        assert!(set.taken(new_branch), "new branch present");
        assert!(!set.taken(old_branch), "old branch gone");
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn rename_to_same_name_is_a_noop() {
        let repo = temp_repo("rename-noop");
        // Keep worktrees inside the temp repo (see rename_moves_branch_and_checkout).
        let cfg = Config {
            worktrees_dir: repo.join(".wt").to_string_lossy().into_owned(),
            ..Default::default()
        };
        let path = worktree_path(&repo, "keep", &cfg);
        add_checked(&repo, "keep", "main", &path, &cfg).unwrap();
        let same = rename(&repo, &path, "keep", "keep", &cfg).unwrap();
        assert_eq!(same, path);
        // Empty new name is rejected.
        assert!(rename(&repo, &path, "keep", "", &cfg).is_err());
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn porcelain_identity_parser_keeps_unix_raw_path_and_ref_bytes() {
        let porcelain = b"worktree /tmp/raw-path\0HEAD deadbeef\0branch refs/heads/feat/\xff\0";
        let records = parse_worktree_identity_records(porcelain).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path.as_bytes(), b"/tmp/raw-path");
        assert_eq!(records[0].branch.as_ref().unwrap().raw(), b"feat/\xff");
    }

    #[test]
    fn inspect_registered_returns_exact_common_admin_and_branch_identity() {
        let repo = temp_repo("identity-inspect");
        let cfg = Config {
            worktrees_dir: repo.join(".external").to_string_lossy().into_owned(),
            ..Default::default()
        };
        let path = worktree_path(&repo, "feat/a", &cfg);
        add_checked(&repo, "feat/a", "main", &path, &cfg).unwrap();
        let inspected = inspect_registered(&repo, &path).unwrap();
        assert_eq!(inspected.registered_path.as_path(), path);
        assert_eq!(inspected.branch.as_ref().unwrap().raw(), b"feat/a");
        assert!(inspected.common_dir.as_path().is_absolute());
        assert!(inspected.admin_dir.as_path().is_absolute());
        assert!(!inspected.admin_id().is_empty());
        let _ = std::fs::remove_dir_all(&repo);
    }
}
