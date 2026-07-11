//! Branch-name generation, base-branch resolution, and worktree add/remove.

use crate::config::{Config, NameScheme, WorktreeMode};
use crate::msg;
use crate::repo;
use crate::util;
use std::path::{Path, PathBuf};

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

fn branch_exists(root: &Path, branch: &str) -> bool {
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

/// [`add`] with the failure reason returned instead of warned, for callers
/// that surface errors in their own UI (the new-worktree progress overlay).
pub fn add_checked(
    root: &Path,
    branch: &str,
    base: &str,
    path: &Path,
    cfg: &Config,
) -> Result<(), String> {
    if cfg.worktree_mode == WorktreeMode::InRepo {
        // Keep .worktrees out of git locally without touching tracked .gitignore.
        let excl = root.join(".git/info/exclude");
        if let Ok(contents) = std::fs::read_to_string(&excl)
            && !contents.lines().any(|l| l == ".worktrees/")
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&excl) {
                let _ = writeln!(f, ".worktrees/");
            }
        }
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Serialize against other thegn/agent git mutations on this repo's shared
    // `.git` (held until the subprocess returns).
    let _lock = util::lock_git_mutations(root);
    let out = util::git_cmd(root)
        .args(["worktree", "add", "--quiet", "-b", branch])
        .arg(path)
        .arg(base)
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            Err(format!(
                "git worktree add failed (branch={branch} base={base}): {}",
                stderr.trim()
            ))
        }
        Err(e) => Err(format!("could not run git worktree add: {e}")),
    }
}

/// Delete a removed worktree's files from wherever they actually live: the
/// local dir AND — for a remote/provider worktree — the checkout on the box
/// over ssh (the local remove never reaches it, so the remote dir would
/// otherwise leak under the host's `remote_dir`). ONLY the worktree dir is
/// removed; the shared base image + warm volumes stay. Best-effort: git is the
/// source of truth. Call BEFORE the DB's `worktrees.location` row is forgotten
/// (else the remote target can't be resolved and only the local dir is purged).
pub fn purge_worktree_files(path: &Path) {
    crate::remote::GitLoc::for_worktree(path).remove_remote_dir();
    let _ = std::fs::remove_dir_all(path);
}

/// Remove a worktree and optionally delete its branch.
pub fn remove(root: &Path, path: &Path, branch: &str, delete_branch: bool) {
    let _lock = util::lock_git_mutations(root);
    let removed = util::git_ok(root, &["worktree", "remove", &path.to_string_lossy()])
        || util::git_ok(
            root,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        );
    if !removed {
        msg::warn(&format!(
            "could not remove worktree at {} (uncommitted changes?)",
            path.display()
        ));
    }
    if delete_branch && !branch.is_empty() && !util::git_ok(root, &["branch", "-D", branch]) {
        msg::warn(&format!("could not delete branch {branch}"));
    }
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
        let _ = std::fs::create_dir_all(parent);
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
        let dir = std::env::temp_dir().join(format!("sz-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t.t"],
            &["config", "user.name", "t"],
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
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn clean_target_removes_artifacts_keeps_source() {
        // A non-cargo dir so clean_target takes the rm fallback (no toolchain
        // dependency in the test). cargo-clean path is exercised by smoke/CI.
        let dir = std::env::temp_dir().join(format!("sz-clean-{}", std::process::id()));
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
        let _ = std::fs::remove_dir_all(&repo);
    }
}
