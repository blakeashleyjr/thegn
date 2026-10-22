//! Branch-name generation, base-branch resolution, and worktree add/remove.

use crate::config::{Config, NameScheme, WorktreeMode};
use crate::identity::{BranchRef, BytePart, ExactPath, IdentityError, RepositoryId};
use crate::msg;
use crate::repo;
use crate::util;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_GIT_IDENTITY_OUTPUT: usize = 64 * 1024;
const WORKTREE_MUTATION_LOCK_WAIT: Duration = Duration::from_secs(120);

#[cfg(test)]
thread_local! {
    static WORKTREE_GIT_DEADLINE_OVERRIDE: std::cell::Cell<Option<Duration>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
thread_local! {
    static WORKTREE_NEXT_DESTRUCTIVE_TIMEOUT: std::cell::Cell<bool> = const {
        std::cell::Cell::new(false)
    };
}

#[cfg(test)]
thread_local! {
    static WORKTREE_MUTATION_LOCK_WAIT_OVERRIDE: std::cell::Cell<Option<Duration>> = const {
        std::cell::Cell::new(None)
    };
}

fn worktree_mutation_lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(wait) = WORKTREE_MUTATION_LOCK_WAIT_OVERRIDE.get() {
        return wait;
    }
    WORKTREE_MUTATION_LOCK_WAIT
}

fn worktree_git_deadline() -> Duration {
    #[cfg(test)]
    if let Some(deadline) = WORKTREE_GIT_DEADLINE_OVERRIDE.get() {
        return deadline;
    }
    WORKTREE_MUTATION_LOCK_WAIT
}

fn destructive_policy() -> crate::git_run::GitPolicy {
    crate::git_run::GitPolicy::read(worktree_git_deadline())
}

fn acquire_mutation_lock(root: &Path) -> std::io::Result<util::GitLock> {
    util::try_lock_git_mutations_within(root, worktree_mutation_lock_wait())
}

/// Exact Git metadata for one registered worktree. The paths retain their
/// native bytes and the branch is kept separate from its display label. Git
/// remains authoritative; this is an inspection result, not a claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeIdentity {
    common_dir: ExactPath,
    admin_dir: ExactPath,
    registered_path: ExactPath,
    branch: Option<BranchRef>,
    generation: crate::identity::WorktreeGeneration,
}

impl GitWorktreeIdentity {
    /// Exact administrative path bytes used as the stable Git-instance input.
    pub fn admin_id(&self) -> &[u8] {
        self.admin_dir.as_bytes()
    }

    pub fn common_dir(&self) -> &ExactPath {
        &self.common_dir
    }

    pub fn admin_dir(&self) -> &ExactPath {
        &self.admin_dir
    }

    pub fn registered_path(&self) -> &ExactPath {
        &self.registered_path
    }

    pub fn branch(&self) -> Option<&BranchRef> {
        self.branch.as_ref()
    }

    pub fn generation(&self) -> crate::identity::WorktreeGeneration {
        self.generation
    }
}

/// Inspect the one Git worktree registered at `path` without converting
/// porcelain output through UTF-8. A relative or symlink alias is accepted for
/// lookup, but the returned path is Git's exact registered path.
pub fn inspect_registered(root: &Path, path: &Path) -> Result<GitWorktreeIdentity, IdentityError> {
    let inspected_common = repo::canonical_common_dir(root)?;
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
    let entry = registered_entry(&parse_worktree_identity_records(&porcelain)?, &requested)?;
    let common_dir = repo::canonical_common_dir(entry.path.as_path())?;
    if common_dir != inspected_common {
        return Err(IdentityError::GitProbeFailed {
            operation: "repository changed during worktree inspection",
        });
    }
    let admin_dir = exact_git_path(
        &entry.path,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
        "rev-parse --git-dir",
    )?;
    let stamp_before = util::git_admin_instance_stamp(admin_dir.as_path()).ok_or(
        IdentityError::UnsupportedEncoding {
            kind: "Git admin instance",
        },
    )?;
    let confirmed_common = repo::canonical_common_dir(root)?;
    if confirmed_common != inspected_common {
        return Err(IdentityError::GitProbeFailed {
            operation: "repository registration changed during worktree inspection",
        });
    }
    let confirmed_porcelain = util::git_stdout_bounded(
        root,
        &["worktree", "list", "--porcelain", "-z"],
        MAX_GIT_IDENTITY_OUTPUT,
    )
    .map_err(|_| IdentityError::GitProbeFailed {
        operation: "git worktree list --porcelain -z",
    })?;
    let confirmed_entry = registered_entry(
        &parse_worktree_identity_records(&confirmed_porcelain)?,
        &requested,
    )?;
    registration_snapshot_consistent(&entry, &confirmed_entry)?;
    let confirmed_admin = exact_git_path(
        &entry.path,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
        "rev-parse --git-dir",
    )?;
    if confirmed_admin != admin_dir {
        return Err(IdentityError::GitProbeFailed {
            operation: "worktree admin identity changed during inspection",
        });
    }
    let stamp_after = util::git_admin_instance_stamp(admin_dir.as_path()).ok_or(
        IdentityError::UnsupportedEncoding {
            kind: "Git admin instance",
        },
    )?;
    if stamp_after.as_bytes() != stamp_before.as_bytes() {
        return Err(IdentityError::GitProbeFailed {
            operation: "Git admin instance changed during inspection",
        });
    }
    let generation =
        crate::identity::WorktreeGeneration::from_captured_instance_stamp(stamp_after.as_bytes())?;
    Ok(GitWorktreeIdentity {
        common_dir,
        admin_dir,
        registered_path: entry.path,
        branch: entry.branch,
        generation,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreeIdentityRecord {
    path: ExactPath,
    branch: Option<BranchRef>,
}

fn registered_entry(
    entries: &[WorktreeIdentityRecord],
    requested: &Path,
) -> Result<WorktreeIdentityRecord, IdentityError> {
    let mut matches = entries.iter().filter(|entry| {
        std::fs::canonicalize(entry.path.as_path())
            .map(|candidate| candidate == requested)
            .unwrap_or(false)
    });
    let entry = matches.next().ok_or(IdentityError::NotRegistered)?;
    if matches.next().is_some() {
        return Err(IdentityError::Ambiguous {
            kind: "Git worktree registration",
        });
    }
    Ok(entry.clone())
}

fn registration_snapshot_consistent(
    before: &WorktreeIdentityRecord,
    after: &WorktreeIdentityRecord,
) -> Result<(), IdentityError> {
    if before != after {
        return Err(IdentityError::GitProbeFailed {
            operation: "worktree branch or registration changed during inspection",
        });
    }
    Ok(())
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
    let end = output
        .len()
        .checked_sub(1)
        .filter(|&end| output[end] == b'\n')?;
    (end > 0).then_some(&output[..end])
}

fn parse_worktree_identity_records(
    porcelain: &[u8],
) -> Result<Vec<WorktreeIdentityRecord>, IdentityError> {
    let mut records = Vec::new();
    let mut current_path: Option<ExactPath> = None;
    let mut current_branch: Option<BranchRef> = None;
    let mut detached = false;
    let finish = |records: &mut Vec<WorktreeIdentityRecord>,
                  current_path: &mut Option<ExactPath>,
                  current_branch: &mut Option<BranchRef>,
                  detached: &mut bool|
     -> Result<(), IdentityError> {
        if let Some(path) = current_path.take() {
            if *detached && current_branch.is_some() {
                return Err(IdentityError::GitProbeFailed {
                    operation: "conflicting branch and detached worktree fields",
                });
            }
            records.push(WorktreeIdentityRecord {
                path,
                branch: current_branch.take(),
            });
        }
        *detached = false;
        Ok(())
    };

    // `-z` is an exact framing contract. Missing the final NUL is a truncated
    // capture, not permission to reinterpret path bytes as line records.
    if !porcelain.last().is_some_and(|byte| *byte == 0) {
        return Err(IdentityError::GitProbeFailed {
            operation: "truncated Git worktree framing",
        });
    }
    let fields = porcelain.split(|byte| *byte == 0);
    for field in fields {
        if field.is_empty() {
            continue;
        }
        if let Some(raw) = field.strip_prefix(b"worktree ") {
            finish(
                &mut records,
                &mut current_path,
                &mut current_branch,
                &mut detached,
            )?;
            current_path = Some(ExactPath::from_git_bytes(raw)?);
        } else if let Some(raw) = field.strip_prefix(b"branch refs/heads/") {
            if current_path.is_none() || detached || current_branch.is_some() {
                return Err(IdentityError::GitProbeFailed {
                    operation: "duplicate or conflicting Git branch field",
                });
            }
            current_branch = Some(BranchRef::from_bytes(raw)?);
        } else if field == b"detached" {
            if current_path.is_none() || detached || current_branch.is_some() {
                return Err(IdentityError::GitProbeFailed {
                    operation: "duplicate or conflicting Git detached field",
                });
            }
            detached = true;
        } else if field == b"bare"
            || field == b"locked"
            || field.starts_with(b"locked ")
            || field == b"prunable"
            || field.starts_with(b"prunable ")
            || field.starts_with(b"HEAD ")
        {
            if current_path.is_none() {
                return Err(IdentityError::GitProbeFailed {
                    operation: "Git worktree field precedes its record",
                });
            }
        } else {
            return Err(IdentityError::GitProbeFailed {
                operation: "unknown Git worktree field",
            });
        }
    }
    finish(
        &mut records,
        &mut current_path,
        &mut current_branch,
        &mut detached,
    )?;
    if records.is_empty() {
        return Err(IdentityError::GitProbeFailed {
            operation: "empty Git worktree registration",
        });
    }
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

/// Check whether an exact local branch exists, preserving probe failures.
/// Lifecycle code must use this checked form so an unverifiable ref is never
/// mistaken for an absent branch.
pub fn branch_exists_checked(root: &Path, branch: &str) -> Result<bool, String> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    match crate::git_run::git_run(
        root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        &crate::git_run::GitPolicy::read(worktree_git_deadline()),
        &cancel,
    ) {
        outcome if outcome.is_success() => Ok(true),
        crate::git_run::GitOutcome::Exit { code: Some(1), .. } => Ok(false),
        outcome => Err(format!("could not check branch {branch}: {outcome}")),
    }
}

/// Prune stale worktree registrations using bounded Git execution.
pub fn prune_registrations(root: &Path) -> Result<(), String> {
    git_destructive(root, &["worktree", "prune"]).map_err(|error| error.to_string())
}

/// Delete a local branch using bounded Git execution and lock-race retries.
pub fn delete_branch(root: &Path, branch: &str) -> Result<(), String> {
    git_destructive(root, &["branch", "-D", branch]).map_err(|error| error.to_string())
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
        if let Some(worktrees) = util::worktree_branches(root) {
            taken.extend(worktrees.into_iter().filter_map(|(_, branch)| branch));
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

/// Readable prefix for a checkout directory name. Display-only: two branches
/// may share it (`feat/a`, `feat-a`, `feat_a` all label as `feat-a`); the
/// digest suffix, never the label, is what keeps their directories apart.
const MAX_CHECKOUT_LABEL_BYTES: usize = 48;

fn checkout_label(branch: &str) -> String {
    let slug = util::slugify(branch);
    // `slugify` emits ASCII only, so a byte cut is a char boundary.
    let cut = &slug[..slug.len().min(MAX_CHECKOUT_LABEL_BYTES)];
    let cut = cut.trim_end_matches('-');
    if cut.is_empty() {
        "worktree".to_string()
    } else {
        cut.to_string()
    }
}

fn path_mode_tag(cfg: &Config) -> &'static [u8] {
    match cfg.worktree_mode {
        WorktreeMode::InRepo => b"in-repo",
        WorktreeMode::Global => b"global",
    }
}

fn checkout_parent(root: &Path, repo_dir_name: &str, cfg: &Config) -> PathBuf {
    if cfg.worktree_mode == WorktreeMode::InRepo {
        root.join(".worktrees")
    } else {
        Path::new(&cfg.worktrees_dir).join(repo_dir_name)
    }
}

/// The collision-resistant checkout directory for an exact branch of an exact
/// repository (THE-516).
///
/// Pure — no Git, no DB, no filesystem: the caller supplies both the
/// [`RepositoryId`] and `repo_dir_name` (the readable per-repo parent
/// component, unused in `in_repo` mode). It takes the name as a PARAMETER
/// precisely so this stays loop-safe: resolving it from Git
/// (`repo::repo_name`) spawns `git rev-parse --show-toplevel`, which
/// [`allocate_worktree_path`] does off the loop.
///
/// Shape: `<parent>/<label>--<digest>` where `<digest>` is the full 64-hex
/// SHA-256 of the domain-separated `(repository id, exact branch bytes, path
/// mode)`. The parent keeps the historical `<worktrees_dir>/<repo name>` (or
/// `<root>/.worktrees`) layout, so tooling that walks one level per repo is
/// unaffected; same-basename repositories may share that parent but never a
/// checkout, because their repository IDs differ. Existing checkouts are never
/// moved to this shape — they stay where Git has them registered.
pub fn worktree_path_for(
    repository: &RepositoryId,
    root: &Path,
    repo_dir_name: &str,
    branch: &str,
    cfg: &Config,
) -> Result<PathBuf, IdentityError> {
    let exact = BranchRef::from_bytes(branch.as_bytes())?;
    let digest = crate::identity::digest_parts(
        b"thegn/worktree-checkout",
        &[
            BytePart::new(repository.as_bytes()),
            BytePart::new(exact.raw()),
            BytePart::new(path_mode_tag(cfg)),
        ],
    )?;
    let leaf = format!(
        "{}--{}",
        checkout_label(branch),
        crate::identity::hex_bytes(&digest)
    );
    Ok(checkout_parent(root, repo_dir_name, cfg).join(leaf))
}

/// Resolve the repository identity from Git and allocate the checkout path
/// for `branch`. Fallible by design: a failed common-dir probe refuses rather
/// than falling back to a basename- or slug-derived path.
pub fn allocate_worktree_path(
    root: &Path,
    branch: &str,
    cfg: &Config,
) -> Result<PathBuf, IdentityError> {
    let repository = repo::repository_id(root)?;
    // `repo_name` asks Git for the toplevel — off-loop work, which is why the
    // pure helper takes the resolved name instead of computing it.
    worktree_path_for(&repository, root, &repo::repo_name(root), branch, cfg)
}

/// A placeholder path for a creation that has not been allocated yet (the
/// optimistic tab the UI opens before the worker reports the real path). Pure
/// and loop-safe: no Git, no DB. Its digest is domain-separated from
/// [`worktree_path_for`], so it never names a real checkout — a placeholder
/// can therefore never alias another worktree's directory while it waits for
/// the worker's authoritative path. Never create, open or remove this path.
pub fn provisional_worktree_path(root: &Path, branch: &str, cfg: &Config) -> PathBuf {
    let digest = crate::identity::digest_parts(
        b"thegn/provisional-checkout",
        &[
            BytePart::new(root.as_os_str().as_encoded_bytes()),
            BytePart::new(branch.as_bytes()),
            BytePart::new(path_mode_tag(cfg)),
        ],
    )
    .unwrap_or([0; 32]);
    let leaf = format!(
        "{}--{}",
        checkout_label(branch),
        crate::identity::hex_bytes(&digest)
    );
    checkout_parent(root, &repo::repo_name_from_path(root), cfg).join(leaf)
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
    /// The destination already existed when the add was attempted under the
    /// mutation lock, so the add was refused before Git ran. Rollback MUST NOT
    /// touch that path: it belongs to someone else (another worktree, user
    /// data, or a racing create).
    pub destination_preexisted: bool,
    /// The repository mutation lock could not be taken, so the add was refused
    /// before Git ran. Nothing was mutated, and no rollback may run: without
    /// the lock a concurrent create could be mid-flight on the same path, and
    /// a rollback would destroy ITS checkout and branch.
    pub lock_unavailable: bool,
    /// A leftover rename intent journal could not be reconciled; the add was
    /// refused before Git ran and no rollback may run.
    pub rename_journal_pending: bool,
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
    // Serialize against other thegn callers on this repo's shared
    // `.git` (held until the subprocess returns). Without the lock the
    // pre-existence check below is worthless — two processes would both pass
    // it, and the loser's rollback would remove the WINNER's checkout — so an
    // unavailable lock refuses instead of proceeding unlocked.
    let _lock = match acquire_mutation_lock(root) {
        Ok(lock) => lock,
        Err(error) => {
            let lock_path = util::git_common_dir(root).join("thegn-git.lock");
            return Err(AddError {
                message: format!(
                    "refusing to create worktree for {branch}: could not take the repository \
                     mutation lock at {}: {error}; retry",
                    lock_path.display()
                ),
                branch_created: false,
                destination_preexisted: false,
                lock_unavailable: true,
                rename_journal_pending: false,
            });
        }
    };
    match reconcile_rename_intent_locked(root) {
        Ok(Some(recovery)) => tracing::info!(
            target: "thegn::worktree",
            ?recovery,
            "reconciled leftover rename intent before worktree creation"
        ),
        Ok(None) => {}
        Err(reason) => {
            return Err(AddError {
                message: format!("refusing to create worktree for {branch}: {reason}"),
                branch_created: false,
                destination_preexisted: false,
                lock_unavailable: false,
                rename_journal_pending: true,
            });
        }
    }
    // A pre-created destination (directory, file or symlink — `git worktree
    // add` happily adopts an empty directory) is never reused: it may be
    // another branch's checkout, a racing create, or unrelated user data.
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(AddError {
            message: format!(
                "refusing to create worktree for {branch}: destination {} already exists. \
                 Inspect it, then either remove the leftover directory (after \
                 `git -C {} worktree list` shows it unregistered and \
                 `git -C {} worktree prune` has run) or pick another branch name",
                path.display(),
                root.display(),
                root.display()
            ),
            branch_created: false,
            destination_preexisted: true,
            lock_unavailable: false,
            rename_journal_pending: false,
        });
    }
    let branch_preexisted = branch_exists(root, branch);
    let out = util::git_cmd(root)
        // Worktree creation runs on a worker, but Git must still fail fast if
        // a remote credential helper would otherwise prompt for input.
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
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
                destination_preexisted: false,
                lock_unavailable: false,
                rename_journal_pending: false,
            })
        }
        Err(e) => Err(AddError {
            message: format!("could not run git worktree add: {e}"),
            branch_created: !branch_preexisted && branch_exists(root, branch),
            destination_preexisted: false,
            lock_unavailable: false,
            rename_journal_pending: false,
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
    remove_detailed(root, path, branch, delete_branch).is_ok()
}

/// Whether git's stderr says the command lost a lock race (transient) rather
/// than being refused for a real reason (unmerged branch, missing ref, ...).
pub fn is_lock_contention(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    s.contains("cannot lock ref")
        || s.contains("unable to create '") && s.contains(".lock'")
        || s.contains("another git process seems to be running")
        || s.contains("could not lock")
}

/// Bounded retry budget for lock contention: 6 tries, 40ms doubling to a 320ms
/// cap (about 1.3s worst case). Non-contention failures are never retried.
const LOCK_RETRY_ATTEMPTS: u32 = 6;

#[derive(Debug)]
enum DestructiveError {
    Refused(String),
    Failed(String),
}

impl std::fmt::Display for DestructiveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(text) | Self::Failed(text) => formatter.write_str(text),
        }
    }
}

/// Run a destructive git command, keeping stderr so a failure carries its
/// reason, and retry within a bound only when it lost a lock race.
fn git_destructive(root: &Path, args: &[&str]) -> Result<(), DestructiveError> {
    #[cfg(test)]
    if WORKTREE_NEXT_DESTRUCTIVE_TIMEOUT.replace(false) {
        return Err(DestructiveError::Failed(
            "Git command timed out".to_string(),
        ));
    }

    let mut delay = std::time::Duration::from_millis(40);
    let mut attempt = 1;
    loop {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let outcome = crate::git_run::git_run(root, args, &destructive_policy(), &cancel);
        match outcome {
            outcome if outcome.is_success() => return Ok(()),
            crate::git_run::GitOutcome::Exit { stderr_tail, .. } => {
                if attempt >= LOCK_RETRY_ATTEMPTS || !is_lock_contention(&stderr_tail) {
                    return Err(DestructiveError::Refused(stderr_tail));
                }
            }
            outcome => {
                return Err(DestructiveError::Failed(format!(
                    "git {}: {outcome}",
                    args.join(" ")
                )));
            }
        }
        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_millis(320));
        attempt += 1;
    }
}

/// [`remove`], but a failure says why (git's own stderr or the ownership
/// reason) so a multi-target delete can report each target that did not go.
/// A failed branch delete after a successful removal is a warning, not an
/// error: the worktree is gone.
pub fn remove_detailed(
    root: &Path,
    path: &Path,
    branch: &str,
    delete_branch: bool,
) -> Result<(), String> {
    let _lock = acquire_mutation_lock(root).map_err(|error| {
        format!(
            "refusing to remove worktree at {}: could not take the repository mutation lock at {}: {error}; retry",
            path.display(),
            util::git_common_dir(root).join("thegn-git.lock").display()
        )
    })?;
    let path_s = path.to_string_lossy();
    let mut failure = None;
    let mut removed = true;
    if let Err(first) = git_destructive(root, &["worktree", "remove", &path_s]) {
        match first {
            DestructiveError::Refused(first) => {
                match git_destructive(root, &["worktree", "remove", "--force", &path_s]) {
                    Ok(()) => {}
                    Err(forced) => {
                        removed = false;
                        failure = Some(if forced.to_string().is_empty() {
                            first
                        } else {
                            forced.to_string()
                        });
                    }
                }
            }
            DestructiveError::Failed(error) => {
                removed = false;
                failure = Some(error);
            }
        }
    }
    if !removed {
        let why = failure
            .take()
            .unwrap_or_else(|| "Git refused removal".to_string());
        match git_worktree_registration(root, path) {
            Some(false) if !path.exists() && !worktree_paths_equal(root, path) => {
                removed = true;
                msg::info(&format!(
                    "Git-unregistered worktree directory at {} was already absent",
                    path.display()
                ));
            }
            Some(false) => {
                let m = format!(
                    "kept Git-unregistered path at {}: Git can no longer prove that this directory belongs to the worktree",
                    path.display()
                );
                msg::warn(&m);
                failure = Some(m);
            }
            Some(true) => {
                let m = format!(
                    "could not remove registered worktree at {} (uncommitted changes or read-only mount?): {why}",
                    path.display()
                );
                msg::warn(&m);
                failure = Some(m);
            }
            None => {
                let m = format!(
                    "could not remove worktree at {} (could not verify Git worktree registry): {why}",
                    path.display()
                );
                msg::warn(&m);
                failure = Some(m);
            }
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
        } else if let Err(why) = git_destructive(root, &["branch", "-D", branch]) {
            msg::warn(&format!("could not delete branch {branch}: {why}"));
        }
    }
    if removed {
        Ok(())
    } else {
        Err(failure.unwrap_or_else(|| format!("could not remove worktree at {}", path.display())))
    }
}

/// Ask Git whether `path` is one of the repository's registered worktrees.
/// `None` is deliberately fail-closed: a broken/unreadable repository is not
/// permission to recursively delete a directory.
fn git_worktree_registration(root: &Path, path: &Path) -> Option<bool> {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let outcome = crate::git_run::git_run(
        root,
        &util::WORKTREE_LIST_Z,
        &crate::git_run::GitPolicy::read(worktree_git_deadline()),
        &cancel,
    );
    let porcelain = outcome.into_text().ok()?;
    let worktrees = util::parse_worktree_branches(&porcelain)?;
    Some(
        worktrees
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

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct RenameIntent {
    version: u32,
    old_branch: String,
    new_branch: String,
    old_path: String,
    new_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameRecovery {
    NotStarted,
    Completed,
    RolledBack,
}

fn rename_intent_path(root: &Path) -> PathBuf {
    util::git_common_dir(root).join("thegn-rename-intent.json")
}

fn write_rename_intent(root: &Path, intent: &RenameIntent) -> Result<(), String> {
    let path = rename_intent_path(root);
    let bytes = serde_json::to_vec(intent).map_err(|error| {
        format!(
            "could not encode rename intent at {}: {error}",
            path.display()
        )
    })?;
    crate::fsperm::write_owner_only_atomic(&path, &bytes).map_err(|error| {
        format!(
            "could not write rename intent at {}: {error}",
            path.display()
        )
    })
}

fn clear_rename_intent(root: &Path) -> Result<(), String> {
    let path = rename_intent_path(root);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not remove rename intent at {}: {error}",
            path.display()
        )),
    }
}

fn registered_branch(root: &Path, path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not inspect worktree path {}: {error}",
                path.display()
            ));
        }
        Ok(_) => {}
    }
    let identity = inspect_registered(root, path).map_err(|error| error.to_string())?;
    Ok(Some(
        identity
            .branch()
            .map_or_else(Vec::new, |branch| branch.raw().to_vec()),
    ))
}

/// Reconcile an interrupted rename while holding the repository mutation lock.
pub fn reconcile_rename_intent(root: &Path) -> Result<Option<RenameRecovery>, String> {
    let _lock = acquire_mutation_lock(root).map_err(|error| {
        format!(
            "could not take the repository mutation lock at {}: {error}; retry",
            util::git_common_dir(root).join("thegn-git.lock").display()
        )
    })?;
    reconcile_rename_intent_locked(root)
}

/// Check whether a rename intent journal is present using filesystem metadata
/// only. This does not run git or acquire the repository mutation lock.
pub fn rename_intent_pending(root: &Path) -> Result<bool, String> {
    let path = rename_intent_path(root);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "could not inspect rename intent at {}: {error}",
            path.display()
        )),
    }
}

pub(crate) fn reconcile_rename_intent_locked(
    root: &Path,
) -> Result<Option<RenameRecovery>, String> {
    let path = rename_intent_path(root);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read rename intent at {}: {error}",
                path.display()
            ));
        }
    };
    let intent: RenameIntent = serde_json::from_slice(&bytes)
        .map_err(|error| format!("malformed rename intent at {}: {error}", path.display()))?;
    if intent.version != 1 {
        return Err(format!(
            "unsupported rename intent at {} (version {})",
            path.display(),
            intent.version
        ));
    }
    if intent.old_branch == intent.new_branch
        || !crate::project::is_valid_branch_name(&intent.old_branch)
        || !crate::project::is_valid_branch_name(&intent.new_branch)
    {
        return Err(format!(
            "invalid branch names in rename intent at {}; journal kept",
            path.display()
        ));
    }

    let old_path = Path::new(&intent.old_path);
    let new_path = Path::new(&intent.new_path);
    let old_branch = intent.old_branch.as_bytes();
    let new_branch = intent.new_branch.as_bytes();
    let old_registered = registered_branch(root, old_path).map_err(|error| {
        format!(
            "rename intent at {} could not be classified: {error}; journal kept",
            path.display()
        )
    })?;
    let new_registered = registered_branch(root, new_path).map_err(|error| {
        format!(
            "rename intent at {} could not be classified: {error}; journal kept",
            path.display()
        )
    })?;
    let old_at_old = old_registered.as_deref() == Some(old_branch);
    let old_at_new = old_registered.as_deref() == Some(new_branch);
    let new_at_new = new_registered.as_deref() == Some(new_branch);
    let old_absent = old_registered.is_none();
    let new_absent = new_registered.is_none();
    let old_branch_exists = branch_exists_checked(root, &intent.old_branch).map_err(|error| {
        format!(
            "rename intent at {} could not be classified: {error}; journal kept",
            path.display()
        )
    })?;
    let new_branch_exists = branch_exists_checked(root, &intent.new_branch).map_err(|error| {
        format!(
            "rename intent at {} could not be classified: {error}; journal kept",
            path.display()
        )
    })?;

    if old_at_old && new_absent && !new_branch_exists {
        clear_rename_intent(root)?;
        return Ok(Some(RenameRecovery::NotStarted));
    }
    if new_at_new && old_absent && !old_branch_exists {
        clear_rename_intent(root)?;
        return Ok(Some(RenameRecovery::Completed));
    }
    if old_at_new && new_absent && !old_branch_exists {
        if util::git_ok(
            root,
            &["branch", "-m", &intent.new_branch, &intent.old_branch],
        ) {
            clear_rename_intent(root)?;
            return Ok(Some(RenameRecovery::RolledBack));
        }
        return Err(format!(
            "SPLIT STATE: branch is now {} but the checkout is still at {}; restore with: git -C {} branch -m {} {}; an intent journal at {} lets the next rename finish the rollback",
            intent.new_branch,
            old_path.display(),
            shell_quote(&root.to_string_lossy()),
            shell_quote(&intent.new_branch),
            shell_quote(&intent.old_branch),
            path.display()
        ));
    }
    Err(format!(
        "an interrupted rename of {} → {} left an ambiguous state; journal kept at {}; inspect `git worktree list` and remove the journal after repairing",
        intent.old_branch,
        intent.new_branch,
        path.display()
    ))
}

/// Rename a worktree's branch (`git branch -m`) and move its checkout to the
/// collision-resistant path allocated for the new branch (`git worktree
/// move`). An owner-only intent journal records both exact paths before Git is
/// mutated. A later call to [`reconcile_rename_intent`] classifies a leftover
/// journal and rolls back the branch if a crash split the two Git steps. A
/// later worktree creation also reconciles a leftover journal under the same
/// repository mutation lock before checking its destination or running Git.
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
    // Validate before any mutation: an invalid ref would fail at `git branch
    // -m` anyway, and a name Git accepts but we cannot round-trip has no
    // business becoming a tab key or a path label.
    if !crate::project::is_valid_branch_name(new_branch) {
        return Err(format!(
            "{new_branch:?} is not a valid Git branch name; rename refused before any change"
        ));
    }
    let new_path = allocate_worktree_path(root, new_branch, cfg)
        .map_err(|e| format!("cannot allocate a checkout for {new_branch}: {e}"))?;
    if let Some(parent) = new_path.parent() {
        let _ = std::fs::create_dir_all(parent); // best-effort: dir prep: a later write reports the real failure
    }
    // As in `add_checked_with_state`: without the lock the preflight below
    // cannot fence a concurrent create/rename, so refuse rather than mutate.
    let _lock = acquire_mutation_lock(root).map_err(|error| {
        format!(
            "refusing to rename {old_branch} → {new_branch}: could not take the repository \
             mutation lock at {}: {error}; retry",
            util::git_common_dir(root).join("thegn-git.lock").display()
        )
    })?;
    if let Err(reason) = reconcile_rename_intent_locked(root) {
        return Err(format!(
            "refusing to rename {old_branch} → {new_branch}: {reason}"
        ));
    }
    if std::fs::symlink_metadata(&new_path).is_ok() {
        return Err(format!(
            "refusing to rename {old_branch} → {new_branch}: destination {} already exists. \
             Inspect it, then remove the leftover directory (after `git -C {} worktree prune`) \
             or choose another name",
            new_path.display(),
            root.display()
        ));
    }
    let identity = inspect_registered(root, old_path).map_err(|error| {
        format!("{old_path:?} is not a registered worktree on {old_branch}: {error}")
    })?;
    if identity.branch().map(|branch| branch.raw()) != Some(old_branch.as_bytes()) {
        return Err(format!(
            "{old_path:?} is not a registered worktree on {old_branch}"
        ));
    }
    let registered_old_path = identity.registered_path().as_path().to_path_buf();
    let Some(old_path_exact) = registered_old_path.to_str() else {
        return Err("cannot journal a non-UTF-8 path; rename refused before any change".into());
    };
    let Some(new_path_exact) = new_path.to_str() else {
        return Err("cannot journal a non-UTF-8 path; rename refused before any change".into());
    };
    let intent = RenameIntent {
        version: 1,
        old_branch: old_branch.to_owned(),
        new_branch: new_branch.to_owned(),
        old_path: old_path_exact.to_owned(),
        new_path: new_path_exact.to_owned(),
    };
    write_rename_intent(root, &intent)?;
    if !util::git_ok(root, &["branch", "-m", old_branch, new_branch]) {
        if let Err(error) = clear_rename_intent(root) {
            return Err(format!(
                "could not rename branch {old_branch} → {new_branch}; {error}"
            ));
        }
        return Err(format!(
            "could not rename branch {old_branch} → {new_branch}"
        ));
    }
    let moved = util::git_cmd(root)
        .args(["worktree", "move"])
        .arg(&registered_old_path)
        .arg(&new_path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if moved {
        if let Err(error) = clear_rename_intent(root) {
            tracing::warn!(target: "thegn::worktree", %error, path = %rename_intent_path(root).display(), "could not clear completed rename intent");
        }
        return Ok(new_path);
    }
    if util::git_ok(root, &["branch", "-m", new_branch, old_branch]) {
        if let Err(error) = clear_rename_intent(root) {
            tracing::warn!(target: "thegn::worktree", %error, path = %rename_intent_path(root).display(), "could not clear rolled-back rename intent");
        }
        return Err(format!(
            "moving the worktree to {} failed; the branch rename was rolled back to {old_branch}",
            new_path.display()
        ));
    }
    Err(format!(
        "SPLIT STATE: branch is now {new_branch} but the checkout is still at {}; \
         restore with: git -C {} branch -m {} {}; an intent journal at {} lets the next rename finish the rollback",
        old_path.display(),
        shell_quote(&root.to_string_lossy()),
        shell_quote(new_branch),
        shell_quote(old_branch),
        rename_intent_path(root).display()
    ))
}

/// Single-quote a value for a copy-pasteable POSIX shell recovery command.
/// Recovery instructions carry branch names and paths that can contain
/// spaces; an unquoted command would silently do the wrong thing.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tg-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: test setup: fresh scratch dir
        std::fs::create_dir_all(&dir).unwrap();
        temp_repo_in(&dir);
        dir
    }

    fn temp_repo_in(dir: &Path) {
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
            assert!(util::git_cmd(dir).args(args).status().unwrap().success());
        }
    }

    #[test]
    fn remove_detailed_refuses_when_the_mutation_lock_is_unavailable() {
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        temp_repo_in(&repo);
        let path = repo.join(".wt-locked");
        assert!(util::git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "locked",
                &path.to_string_lossy()
            ]
        ));
        let lock_path = util::git_common_dir(&repo).join("thegn-git.lock");
        std::fs::create_dir(&lock_path).unwrap();

        let error = remove_detailed(&repo, &path, "locked", true).unwrap_err();
        assert!(error.contains("mutation lock"), "{error}");
        assert!(
            error.contains(&lock_path.to_string_lossy().to_string()),
            "{error}"
        );
        assert!(path.exists(), "the worktree must remain untouched");
        assert!(branch_exists(&repo, "locked"), "the branch must remain");
    }

    #[test]
    fn remove_detailed_times_out_without_forcing_and_keeps_the_branch() {
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        temp_repo_in(&repo);
        let path = repo.join(".wt-deadline");
        assert!(util::git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "deadline",
                &path.to_string_lossy()
            ]
        ));

        // Inject a one-shot timeout for the initial remove. The fallback force
        // remove would succeed, so this deterministically checks that a
        // supervision failure is not retried with --force.
        WORKTREE_NEXT_DESTRUCTIVE_TIMEOUT.set(true);
        let result = remove_detailed(&repo, &path, "deadline", true);

        let Err(error) = result else {
            panic!("timed-out remove unexpectedly succeeded");
        };
        assert!(error.contains("timed out"), "{error}");
        assert!(path.exists(), "the worktree must remain");
        assert_eq!(branch_exists_checked(&repo, "deadline"), Ok(true));
    }

    #[test]
    fn branch_exists_checked_tells_absent_from_unverifiable() {
        let repo = temp_repo("checked-branch");
        assert_eq!(branch_exists_checked(&repo, "main"), Ok(true));
        assert_eq!(branch_exists_checked(&repo, "missing"), Ok(false));
        let not_repo = tempfile::tempdir().unwrap();
        let result = branch_exists_checked(not_repo.path(), "main");
        assert!(matches!(result, Err(error) if error.contains("not a git repository")));
    }

    #[test]
    fn prune_registrations_reports_git_stderr() {
        let not_repo = tempfile::tempdir().unwrap();
        let result = prune_registrations(not_repo.path());
        assert!(matches!(result, Err(error) if error.contains("not a git repository")));
        let repo = temp_repo("prune-registrations");
        assert!(prune_registrations(&repo).is_ok());
    }

    #[test]
    fn remove_detailed_gives_up_when_the_mutation_lock_stays_held() {
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        temp_repo_in(&repo);
        let path = repo.join(".wt-timeout");
        assert!(util::git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "timeout",
                &path.to_string_lossy()
            ]
        ));
        let held = util::try_lock_git_mutations(&repo).unwrap();
        let worker_repo = repo.clone();
        let worker_path = path.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            WORKTREE_MUTATION_LOCK_WAIT_OVERRIDE.with(|wait| {
                wait.set(Some(Duration::from_millis(100)));
            });
            sender
                .send(remove_detailed(&worker_repo, &worker_path, "timeout", true))
                .unwrap();
        });

        let received = receiver.recv_timeout(Duration::from_secs(10));
        drop(held);
        worker.join().unwrap();
        let error = received
            .expect("worktree removal must return while the lock remains held")
            .unwrap_err();
        assert!(error.contains("mutation lock"), "{error}");
        assert!(error.contains("timed out"), "{error}");
        assert!(path.exists(), "the worktree must remain untouched");
        assert!(branch_exists(&repo, "timeout"), "the branch must remain");
    }

    #[test]
    fn add_gives_up_when_the_mutation_lock_stays_held() {
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        temp_repo_in(&repo);
        let path = repo.join(".wt-timeout-add");
        let cfg = scratch_cfg(&repo);
        let held = util::try_lock_git_mutations(&repo).unwrap();
        let worker_repo = repo.clone();
        let worker_path = path.clone();
        let worker_cfg = cfg.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            WORKTREE_MUTATION_LOCK_WAIT_OVERRIDE.with(|wait| {
                wait.set(Some(Duration::from_millis(100)));
            });
            sender
                .send(add_checked_with_state(
                    &worker_repo,
                    "timeout-add",
                    "main",
                    &worker_path,
                    &worker_cfg,
                ))
                .unwrap();
        });

        let received = receiver.recv_timeout(Duration::from_secs(10));
        drop(held);
        worker.join().unwrap();
        let error = received
            .expect("worktree creation must return while the lock remains held")
            .unwrap_err();
        assert!(error.lock_unavailable);
        assert!(error.message.contains("timed out"), "{}", error.message);
        assert!(!path.exists());
        assert!(!branch_exists(&repo, "timeout-add"));
    }

    #[test]
    fn add_reports_why_the_mutation_lock_was_unavailable() {
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        temp_repo_in(&repo);
        let linked_root = repo.join(".wt-control");
        assert!(util::git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "control",
                &linked_root.to_string_lossy(),
            ]
        ));
        let lock_path = util::git_common_dir(&linked_root).join("thegn-git.lock");
        std::fs::create_dir(&lock_path).unwrap();
        let path = repo.join(".wt-target");
        let lock_error = match util::try_lock_git_mutations(&linked_root) {
            Ok(_) => panic!("a directory at the lock path must be rejected"),
            Err(error) => error.to_string(),
        };

        let cfg = scratch_cfg(&repo);
        let error =
            add_checked_with_state(&linked_root, "target", "main", &path, &cfg).unwrap_err();
        assert!(error.lock_unavailable);
        assert!(
            error
                .message
                .contains(&lock_path.to_string_lossy().to_string())
        );
        assert!(error.message.contains(&lock_error));
        assert!(error.message.contains("mutation lock"));
        assert!(!path.exists());
        assert!(!branch_exists(&repo, "target"));
    }

    #[test]
    fn rename_refuses_when_the_mutation_lock_is_unavailable() {
        let scratch = tempfile::tempdir().unwrap();
        let repo = scratch.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        temp_repo_in(&repo);
        let path = repo.join(".wt-old");
        assert!(util::git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "old",
                &path.to_string_lossy()
            ]
        ));
        let lock_path = util::git_common_dir(&repo).join("thegn-git.lock");
        std::fs::create_dir(&lock_path).unwrap();

        let cfg = scratch_cfg(&repo);
        let error = rename(&repo, &path, "old", "new", &cfg).unwrap_err();
        assert!(error.contains("mutation lock"), "{error}");
        assert!(
            error.contains(&lock_path.to_string_lossy().to_string()),
            "{error}"
        );
        assert!(branch_exists(&repo, "old"));
        assert!(!branch_exists(&repo, "new"));
        assert!(path.exists());
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
    fn lock_contention_is_told_apart_from_real_refusals() {
        assert!(is_lock_contention(
            "error: cannot lock ref 'refs/heads/x': Unable to create '/r/.git/refs/heads/x.lock': File exists."
        ));
        assert!(is_lock_contention(
            "fatal: Unable to create '/r/.git/packed-refs.lock': File exists."
        ));
        assert!(!is_lock_contention(
            "error: the branch 'x' is not fully merged"
        ));
        assert!(!is_lock_contention("error: branch 'x' not found."));
    }

    #[test]
    fn branch_delete_retries_a_held_ref_lock_then_succeeds() {
        let repo = temp_repo("retry-lock");
        let path = repo.join(".wt-locked");
        assert!(util::git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "locked",
                &path.to_string_lossy()
            ]
        ));
        let lock = repo.join(".git/refs/heads/locked.lock");
        std::fs::write(&lock, "").unwrap();
        let release = lock.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            let _ = std::fs::remove_file(release); // best-effort: test helper
        });
        assert!(remove_detailed(&repo, &path, "locked", true).is_ok());
        t.join().unwrap();
        assert!(
            !branch_exists(&repo, "locked"),
            "retry must land the delete"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn non_contention_failure_is_not_retried_and_carries_git_reason() {
        let repo = temp_repo("no-retry");
        let started = std::time::Instant::now();
        let err = git_destructive(&repo, &["branch", "-D", "nope"]).unwrap_err();
        assert!(
            err.to_string().contains("nope"),
            "git's own reason is kept: {err}"
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(300));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn concurrent_removals_in_one_repo_all_land() {
        let repo = temp_repo("concurrent");
        let mut jobs = Vec::new();
        for i in 0..4 {
            let path = repo.join(format!(".wt-c{i}"));
            let branch = format!("c{i}");
            assert!(util::git_ok(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &branch,
                    &path.to_string_lossy()
                ]
            ));
            jobs.push((path, branch));
        }
        let handles: Vec<_> = jobs
            .iter()
            .cloned()
            .map(|(path, branch)| {
                let repo = repo.clone();
                std::thread::spawn(move || remove_detailed(&repo, &path, &branch, true))
            })
            .collect();
        for h in handles {
            h.join().unwrap().expect("every removal lands");
        }
        for (path, branch) in &jobs {
            assert!(!path.exists());
            assert!(!branch_exists(&repo, branch));
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn remove_detailed_reports_why_for_an_unowned_directory() {
        let repo = temp_repo("why");
        let path = repo.join(".wt-orphan2");
        std::fs::create_dir_all(&path).unwrap();
        let err = remove_detailed(&repo, &path, "", false).unwrap_err();
        assert!(err.contains("Git-unregistered"), "{err}");
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
        let path = allocate_worktree_path(&repo, old_branch, &cfg).unwrap();
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
        let path = allocate_worktree_path(&repo, "keep", &cfg).unwrap();
        add_checked(&repo, "keep", "main", &path, &cfg).unwrap();
        let same = rename(&repo, &path, "keep", "keep", &cfg).unwrap();
        assert_eq!(same, path);
        // Empty new name is rejected.
        assert!(rename(&repo, &path, "keep", "", &cfg).is_err());
        // best-effort: test cleanup: scratch removal must never fail the test
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
    #[test]
    fn porcelain_identity_parser_keeps_unix_raw_path_and_ref_bytes() {
        let porcelain = b"worktree /tmp/raw-path\r\0HEAD deadbeef\0branch refs/heads/feat/\xff\0";
        let records = parse_worktree_identity_records(porcelain).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].path.as_bytes(), b"/tmp/raw-path\r");
        assert_eq!(records[0].branch.as_ref().unwrap().raw(), b"feat/\xff");
    }

    #[test]
    fn porcelain_identity_parser_rejects_truncation_and_conflicts() {
        assert!(parse_worktree_identity_records(b"worktree /tmp/raw\n").is_err());
        assert!(
            parse_worktree_identity_records(
                b"worktree /tmp/raw\0branch refs/heads/one\0branch refs/heads/two\0"
            )
            .is_err()
        );
        assert!(
            parse_worktree_identity_records(
                b"worktree /tmp/raw\0detached\0branch refs/heads/one\0"
            )
            .is_err()
        );
    }

    #[test]
    fn inspect_registered_returns_exact_common_admin_and_branch_identity() {
        let repo = temp_repo("identity-inspect");
        let cfg = Config {
            worktrees_dir: repo.join(".external").to_string_lossy().into_owned(),
            ..Default::default()
        };
        let path = allocate_worktree_path(&repo, "feat/a", &cfg).unwrap();
        add_checked(&repo, "feat/a", "main", &path, &cfg).unwrap();
        let inspected = inspect_registered(&repo, &path).unwrap();
        assert_eq!(inspected.registered_path().as_path(), path);
        assert_eq!(inspected.branch().unwrap().raw(), b"feat/a");
        assert!(inspected.common_dir().as_path().is_absolute());
        assert!(inspected.admin_dir().as_path().is_absolute());
        assert!(!inspected.admin_id().is_empty());
        assert!(
            !inspected
                .generation()
                .as_bytes()
                .iter()
                .all(|byte| *byte == 0)
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[cfg(unix)]
    #[test]
    fn inspection_preserves_newline_and_non_utf8_paths_and_detached_state() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let repo = temp_repo("identity-raw-path");
        // Scratch INSIDE the per-test repo dir, so the fixture never writes a
        // stray raw-byte path into /tmp and the repo cleanup below reclaims it
        // even when an assertion fails.
        let scratch = repo.join("raw");
        std::fs::create_dir_all(&scratch).unwrap();
        let mut raw = scratch.as_os_str().as_bytes().to_vec();
        raw.extend_from_slice(b"/thegn-raw-\n");
        raw.push(0xff);
        let path = PathBuf::from(std::ffi::OsString::from_vec(raw));
        assert!(
            util::git_cmd(&repo)
                .args(["worktree", "add", "-q", "--detach"])
                .arg(&path)
                .arg("HEAD")
                .status()
                .unwrap()
                .success()
        );
        let inspected = inspect_registered(&repo, &path).unwrap();
        assert_eq!(
            inspected.registered_path().as_bytes(),
            path.as_os_str().as_bytes()
        );
        assert!(inspected.branch().is_none());
        let _ = util::git_cmd(&repo)
            .args(["worktree", "remove", "-f"])
            .arg(&path)
            .status();
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn replacement_during_inspection_is_rejected_by_snapshot_consistency() {
        let repo = temp_repo("identity-replacement");
        let path = repo.join(".replacement");
        assert!(
            util::git_cmd(&repo)
                .args([
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "before",
                    path.to_str().unwrap()
                ])
                .status()
                .unwrap()
                .success()
        );
        let requested = std::fs::canonicalize(&path).unwrap();
        let before = util::git_stdout_bounded(
            &repo,
            &["worktree", "list", "--porcelain", "-z"],
            MAX_GIT_IDENTITY_OUTPUT,
        )
        .unwrap();
        let before = registered_entry(
            &parse_worktree_identity_records(&before).unwrap(),
            &requested,
        )
        .unwrap();
        assert!(
            util::git_cmd(&path)
                .args(["branch", "-m", "after"])
                .status()
                .unwrap()
                .success()
        );
        let after = util::git_stdout_bounded(
            &repo,
            &["worktree", "list", "--porcelain", "-z"],
            MAX_GIT_IDENTITY_OUTPUT,
        )
        .unwrap();
        let after = registered_entry(
            &parse_worktree_identity_records(&after).unwrap(),
            &requested,
        )
        .unwrap();
        assert!(registration_snapshot_consistent(&before, &after).is_err());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn generation_survives_checkout_and_branch_rename_but_changes_on_replacement() {
        let repo = temp_repo("identity-generation");
        let path = repo.join(".external/feature");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args([
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    "feat/a",
                    path.to_str().unwrap()
                ])
                .status()
                .unwrap()
                .success()
        );
        let first = inspect_registered(&repo, &path).unwrap();
        assert!(
            util::git_cmd(&path)
                .args(["switch", "--detach", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let detached = inspect_registered(&repo, &path).unwrap();
        assert_eq!(first.generation(), detached.generation());
        assert!(
            util::git_cmd(&path)
                .args(["switch", "-q", "-c", "renamed"])
                .status()
                .unwrap()
                .success()
        );
        let renamed = inspect_registered(&repo, &path).unwrap();
        assert_eq!(first.generation(), renamed.generation());

        assert!(
            util::git_cmd(&repo)
                .args(["worktree", "remove", "-f", path.to_str().unwrap()])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            util::git_cmd(&repo)
                .args(["worktree", "add", "-q", path.to_str().unwrap(), "renamed"])
                .status()
                .unwrap()
                .success()
        );
        let replacement = inspect_registered(&repo, &path).unwrap();
        assert_ne!(
            first.generation(),
            replacement.generation(),
            "a recreated Git admin directory must not retain the old generation"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn scratch_cfg(repo: &Path) -> Config {
        Config {
            worktrees_dir: repo.join(".wt").to_string_lossy().into_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn checkout_paths_never_collapse_branch_aliases() {
        let repository = crate::identity::RepositoryId::from_bytes([3; 32]);
        let root = Path::new("/r/app");
        let long = "x".repeat(400);
        let branches = [
            "feat/a",
            "feat-a",
            "feat_a",
            "FEAT-A",
            "feat/A",
            "修复/登录",
            "___",
            long.as_str(),
        ];
        for mode in [WorktreeMode::Global, WorktreeMode::InRepo] {
            let cfg = Config {
                worktrees_dir: "/wt".into(),
                worktree_mode: mode,
                ..Default::default()
            };
            let paths: Vec<PathBuf> = branches
                .iter()
                .map(|b| worktree_path_for(&repository, root, "app", b, &cfg).unwrap())
                .collect();
            for (i, p) in paths.iter().enumerate() {
                let leaf = p.file_name().unwrap().to_str().unwrap();
                assert!(leaf.len() <= 255, "leaf fits NAME_MAX: {leaf}");
                assert!(!leaf.contains('/'));
                assert!(paths[i + 1..].iter().all(|q| q != p), "{p:?} aliased");
            }
            // Deterministic for the same exact inputs.
            assert_eq!(
                paths[0],
                worktree_path_for(&repository, root, "app", "feat/a", &cfg).unwrap()
            );
        }
        // Empty and NUL-bearing branches are typed refusals, not paths.
        let cfg = Config::default();
        assert!(worktree_path_for(&repository, root, "app", "", &cfg).is_err());
        assert!(worktree_path_for(&repository, root, "app", "a\0b", &cfg).is_err());
    }

    #[test]
    fn same_basename_repositories_never_share_a_checkout() {
        let a = temp_repo("same-base-a").join("app");
        let b = temp_repo("same-base-b").join("app");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            assert!(
                util::git_cmd(dir)
                    .args(["init", "-q"])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let cfg = Config {
            worktrees_dir: "/shared-wt".into(),
            ..Default::default()
        };
        let pa = allocate_worktree_path(&a, "feat/a", &cfg).unwrap();
        let pb = allocate_worktree_path(&b, "feat/a", &cfg).unwrap();
        assert_eq!(
            pa.parent(),
            pb.parent(),
            "the readable per-repo parent is kept"
        );
        assert_ne!(pa, pb, "distinct repositories get distinct checkouts");
        // A provisional placeholder never names the real checkout.
        assert_ne!(provisional_worktree_path(&a, "feat/a", &cfg), pa);
        let _ = std::fs::remove_dir_all(a.parent().unwrap());
        let _ = std::fs::remove_dir_all(b.parent().unwrap());
    }

    #[test]
    fn slash_dash_underscore_branches_coexist_as_real_worktrees() {
        let repo = temp_repo("alias-coexist");
        let cfg = scratch_cfg(&repo);
        let mut seen = Vec::new();
        for branch in ["feat/a", "feat-a", "feat_a"] {
            let path = allocate_worktree_path(&repo, branch, &cfg).unwrap();
            add_checked(&repo, branch, "main", &path, &cfg).unwrap();
            let inspected = inspect_registered(&repo, &path).unwrap();
            assert_eq!(inspected.branch().unwrap().raw(), branch.as_bytes());
            seen.push(path);
        }
        assert_eq!(seen.len(), 3);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn pre_created_destination_is_refused_and_left_untouched() {
        let repo = temp_repo("precreated");
        let cfg = scratch_cfg(&repo);
        let path = allocate_worktree_path(&repo, "feat/x", &cfg).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("user-data"), b"keep").unwrap();
        let err = add_checked_with_state(&repo, "feat/x", "main", &path, &cfg).unwrap_err();
        assert!(err.destination_preexisted);
        assert!(!err.branch_created);
        assert!(!branch_exists(&repo, "feat/x"), "no branch was created");
        assert_eq!(std::fs::read(path.join("user-data")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn rename_refuses_existing_destination_without_touching_the_branch() {
        let repo = temp_repo("rename-precreated");
        let cfg = scratch_cfg(&repo);
        let path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &path, &cfg).unwrap();
        let dest = allocate_worktree_path(&repo, "new", &cfg).unwrap();
        std::fs::create_dir_all(&dest).unwrap();
        assert!(rename(&repo, &path, "old", "new", &cfg).is_err());
        assert!(branch_exists(&repo, "old"));
        assert!(!branch_exists(&repo, "new"));
        assert!(path.is_dir());
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn rename_refuses_an_invalid_new_branch_before_touching_git() {
        let repo = temp_repo("rename-invalid");
        let cfg = scratch_cfg(&repo);
        let path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &path, &cfg).unwrap();
        for bad in ["new branch", "x~1", "a..b", "HEAD"] {
            let err = rename(&repo, &path, "old", bad, &cfg).unwrap_err();
            assert!(err.contains("not a valid Git branch name"), "{bad}: {err}");
        }
        assert!(branch_exists(&repo, "old"), "nothing was renamed");
        assert!(path.is_dir(), "the checkout did not move");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn rename_rolls_back_the_branch_when_the_move_fails() {
        let repo = temp_repo("rename-rollback");
        let cfg = scratch_cfg(&repo);
        let path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &path, &cfg).unwrap();
        // A locked worktree refuses `git worktree move` without --force.
        assert!(
            util::git_cmd(&repo)
                .args(["worktree", "lock"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let err = rename(&repo, &path, "old", "new", &cfg).unwrap_err();
        assert!(err.contains("rolled back"), "{err}");
        assert!(branch_exists(&repo, "old"), "branch restored");
        assert!(!branch_exists(&repo, "new"));
        assert_eq!(
            inspect_registered(&repo, &path)
                .unwrap()
                .branch()
                .unwrap()
                .raw(),
            b"old"
        );
        let _ = util::git_cmd(&repo)
            .args(["worktree", "unlock"])
            .arg(&path)
            .status();
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn rename_intent_for(repo: &Path, old_path: &Path, cfg: &Config) -> RenameIntent {
        let identity = inspect_registered(repo, old_path).unwrap();
        RenameIntent {
            version: 1,
            old_branch: "old".into(),
            new_branch: "new".into(),
            old_path: identity
                .registered_path()
                .as_path()
                .to_str()
                .unwrap()
                .to_owned(),
            new_path: allocate_worktree_path(repo, "new", cfg)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned(),
        }
    }

    #[test]
    fn create_reconciles_not_started_journal_first() {
        let repo = temp_repo("create-reconciles-not-started");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        write_rename_intent(&repo, &rename_intent_for(&repo, &old_path, &cfg)).unwrap();
        let path = allocate_worktree_path(&repo, "other", &cfg).unwrap();

        add_checked_with_state(&repo, "other", "main", &path, &cfg).unwrap();

        assert!(path.exists());
        assert!(!rename_intent_path(&repo).exists());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn create_reconciles_completed_journal_first() {
        let repo = temp_repo("create-reconciles-completed");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        let intent = rename_intent_for(&repo, &old_path, &cfg);
        let new_path = PathBuf::from(&intent.new_path);
        write_rename_intent(&repo, &intent).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["branch", "-m", "old", "new"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            util::git_cmd(&repo)
                .args(["worktree", "move"])
                .arg(&old_path)
                .arg(&new_path)
                .status()
                .unwrap()
                .success()
        );
        let path = allocate_worktree_path(&repo, "other", &cfg).unwrap();

        add_checked_with_state(&repo, "other", "main", &path, &cfg).unwrap();

        assert!(path.exists());
        assert!(new_path.exists());
        assert!(!branch_exists(&repo, "old"));
        assert!(branch_exists(&repo, "new"));
        assert!(!rename_intent_path(&repo).exists());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn create_reconciles_split_journal_by_rolling_back() {
        let repo = temp_repo("create-reconciles-split");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        write_rename_intent(&repo, &rename_intent_for(&repo, &old_path, &cfg)).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["branch", "-m", "old", "new"])
                .status()
                .unwrap()
                .success()
        );
        let path = allocate_worktree_path(&repo, "other", &cfg).unwrap();

        add_checked_with_state(&repo, "other", "main", &path, &cfg).unwrap();

        assert!(path.exists());
        assert!(branch_exists(&repo, "old"));
        assert!(!branch_exists(&repo, "new"));
        assert!(!rename_intent_path(&repo).exists());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn create_reconciles_ambiguous_journal_refuses() {
        let repo = temp_repo("create-reconciles-ambiguous");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        write_rename_intent(&repo, &rename_intent_for(&repo, &old_path, &cfg)).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["branch", "new", "main"])
                .status()
                .unwrap()
                .success()
        );
        let path = allocate_worktree_path(&repo, "other", &cfg).unwrap();

        let error = add_checked_with_state(&repo, "other", "main", &path, &cfg).unwrap_err();

        assert!(error.rename_journal_pending);
        assert!(!error.branch_created);
        assert!(error.message.contains("journal kept"), "{}", error.message);
        assert!(!path.exists());
        assert!(!branch_exists(&repo, "other"));
        assert!(rename_intent_path(&repo).exists());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn create_reconciles_malformed_journal_refuses() {
        let repo = temp_repo("create-reconciles-malformed");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        let journal = rename_intent_path(&repo);
        std::fs::write(&journal, b"not json").unwrap();
        let path = allocate_worktree_path(&repo, "other", &cfg).unwrap();

        let error = add_checked_with_state(&repo, "other", "main", &path, &cfg).unwrap_err();

        assert!(error.rename_journal_pending);
        assert!(error.message.contains("malformed rename intent"));
        assert!(!path.exists());
        assert!(!branch_exists(&repo, "other"));
        assert!(journal.exists());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_intent_not_started_is_cleared_by_reconcile() {
        let repo = temp_repo("rename-intent-not-started");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        write_rename_intent(&repo, &rename_intent_for(&repo, &old_path, &cfg)).unwrap();

        assert_eq!(
            reconcile_rename_intent(&repo).unwrap(),
            Some(RenameRecovery::NotStarted)
        );
        assert!(!rename_intent_path(&repo).exists());
        assert!(branch_exists(&repo, "old"));
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_intent_split_is_rolled_back_by_reconcile() {
        let repo = temp_repo("rename-intent-split");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        let intent = rename_intent_for(&repo, &old_path, &cfg);
        write_rename_intent(&repo, &intent).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["branch", "-m", "old", "new"])
                .status()
                .unwrap()
                .success()
        );

        assert_eq!(
            reconcile_rename_intent(&repo).unwrap(),
            Some(RenameRecovery::RolledBack)
        );
        assert!(branch_exists(&repo, "old"));
        assert!(!branch_exists(&repo, "new"));
        assert!(!rename_intent_path(&repo).exists());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_intent_completed_move_is_cleared_by_reconcile() {
        let repo = temp_repo("rename-intent-completed");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        let intent = rename_intent_for(&repo, &old_path, &cfg);
        let new_path = PathBuf::from(&intent.new_path);
        write_rename_intent(&repo, &intent).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["branch", "-m", "old", "new"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            util::git_cmd(&repo)
                .args(["worktree", "move"])
                .arg(&old_path)
                .arg(&new_path)
                .status()
                .unwrap()
                .success()
        );

        assert_eq!(
            reconcile_rename_intent(&repo).unwrap(),
            Some(RenameRecovery::Completed)
        );
        assert!(!rename_intent_path(&repo).exists());
        assert!(branch_exists(&repo, "new"));
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_intent_ambiguous_state_refuses_the_next_rename() {
        let repo = temp_repo("rename-intent-ambiguous");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();
        write_rename_intent(&repo, &rename_intent_for(&repo, &old_path, &cfg)).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["branch", "new", "main"])
                .status()
                .unwrap()
                .success()
        );

        let error = rename(&repo, &old_path, "old", "other", &cfg).unwrap_err();
        assert!(
            error.contains("ambiguous") || error.contains("rename intent"),
            "{error}"
        );
        assert!(rename_intent_path(&repo).exists());
        assert!(branch_exists(&repo, "old"));
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_success_and_rollback_leave_no_intent_journal() {
        let repo = temp_repo("rename-intent-cleanup");
        let cfg = scratch_cfg(&repo);
        let old_path = allocate_worktree_path(&repo, "old", &cfg).unwrap();
        add_checked(&repo, "old", "main", &old_path, &cfg).unwrap();

        let new_path = rename(&repo, &old_path, "old", "new", &cfg).unwrap();
        assert!(new_path.is_dir());
        assert!(!rename_intent_path(&repo).exists());

        let rollback_path = allocate_worktree_path(&repo, "rollback", &cfg).unwrap();
        add_checked(&repo, "rollback", "main", &rollback_path, &cfg).unwrap();
        assert!(
            util::git_cmd(&repo)
                .args(["worktree", "lock"])
                .arg(&rollback_path)
                .status()
                .unwrap()
                .success()
        );
        let error = rename(&repo, &rollback_path, "rollback", "rollback-new", &cfg).unwrap_err();
        assert!(error.contains("rolled back"), "{error}");
        assert!(!rename_intent_path(&repo).exists());
        util::git_cmd(&repo)
            .args(["worktree", "unlock"])
            .arg(&rollback_path)
            .status()
            .unwrap();
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_refuses_an_unregistered_old_path() {
        let repo = temp_repo("rename-unregistered");
        let cfg = scratch_cfg(&repo);
        let error = rename(&repo, &repo.join("not-a-worktree"), "main", "x", &cfg).unwrap_err();
        assert!(error.contains("registered"), "{error}");
        assert!(branch_exists(&repo, "main"));
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_intent_pending_is_a_stat_only_probe() {
        let repo = temp_repo("rename-intent-pending");
        assert!(!rename_intent_pending(&repo).unwrap());

        let intent = RenameIntent {
            version: 1,
            old_branch: "old".into(),
            new_branch: "new".into(),
            old_path: repo.join("wt-old").to_string_lossy().into_owned(),
            new_path: repo.join("wt-new").to_string_lossy().into_owned(),
        };
        write_rename_intent(&repo, &intent).unwrap();
        assert!(rename_intent_pending(&repo).unwrap());

        assert!(!rename_intent_pending(&repo.join("missing")).unwrap());
        std::fs::remove_dir_all(&repo).unwrap();
    }

    #[test]
    fn rename_intent_pending_reports_non_not_found_metadata_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();

        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}/child/grandchild\n", blocker.display()),
        )
        .unwrap();

        let result = rename_intent_pending(&root);
        assert!(matches!(result, Err(error) if error.contains("could not inspect rename intent")));
    }
}
