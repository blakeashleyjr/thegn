//! Branch-name generation, base-branch resolution, and worktree add/remove.

use crate::config::{Config, NameScheme, WorktreeMode};
use crate::identity::{BranchRef, BytePart, ExactPath, IdentityError, RepositoryId};
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

fn checkout_parent(root: &Path, repo_dir_name: String, cfg: &Config) -> PathBuf {
    if cfg.worktree_mode == WorktreeMode::InRepo {
        root.join(".worktrees")
    } else {
        Path::new(&cfg.worktrees_dir).join(repo_dir_name)
    }
}

/// The collision-resistant checkout directory for an exact branch of an exact
/// repository (THE-516). Pure: the caller supplies the [`RepositoryId`]
/// already resolved from Git.
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
    Ok(checkout_parent(root, repo::repo_name(root), cfg).join(leaf))
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
    worktree_path_for(&repository, root, branch, cfg)
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
    checkout_parent(root, repo::repo_name_from_path(root), cfg).join(leaf)
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
    // A pre-created destination (directory, file or symlink — `git worktree
    // add` happily adopts an empty directory) is never reused: it may be
    // another branch's checkout, a racing create, or unrelated user data.
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(AddError {
            message: format!(
                "refusing to create worktree for {branch}: destination {} already exists",
                path.display()
            ),
            branch_created: false,
            destination_preexisted: true,
        });
    }
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
                destination_preexisted: false,
            })
        }
        Err(e) => Err(AddError {
            message: format!("could not run git worktree add: {e}"),
            branch_created: !branch_preexisted && branch_exists(root, branch),
            destination_preexisted: false,
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
/// collision-resistant path allocated for the new branch (`git worktree
/// move`). Returns the new on-disk path on success, or the reason it failed.
///
/// The two Git mutations run under the repository mutation lock after a
/// preflight that refuses an existing destination. If the move fails the
/// branch rename is rolled back, so the caller sees the old, consistent state.
/// Only when that rollback ALSO fails is the split reported — explicitly, with
/// the exact recovery command — never as success. A crash between the two
/// steps is not journaled (THE-516 chunk 6 remains open for that).
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
    let new_path = allocate_worktree_path(root, new_branch, cfg)
        .map_err(|e| format!("cannot allocate a checkout for {new_branch}: {e}"))?;
    if let Some(parent) = new_path.parent() {
        let _ = std::fs::create_dir_all(parent); // best-effort: dir prep: a later write reports the real failure
    }
    let _lock = util::lock_git_mutations(root);
    if std::fs::symlink_metadata(&new_path).is_ok() {
        return Err(format!(
            "refusing to rename {old_branch} → {new_branch}: destination {} already exists",
            new_path.display()
        ));
    }
    if !util::git_ok(root, &["branch", "-m", old_branch, new_branch]) {
        return Err(format!(
            "could not rename branch {old_branch} → {new_branch}"
        ));
    }
    let moved = util::git_cmd(root)
        .args(["worktree", "move"])
        .arg(old_path)
        .arg(&new_path)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if moved {
        return Ok(new_path);
    }
    if util::git_ok(root, &["branch", "-m", new_branch, old_branch]) {
        return Err(format!(
            "moving the worktree to {} failed; the branch rename was rolled back to {old_branch}",
            new_path.display()
        ));
    }
    Err(format!(
        "SPLIT STATE: branch is now {new_branch} but the checkout is still at {}; \
         restore with `git -C {} branch -m {new_branch} {old_branch}`",
        old_path.display(),
        root.display()
    ))
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
        let mut raw = format!("/tmp/thegn-raw-{}-\n", std::process::id()).into_bytes();
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
                .map(|b| worktree_path_for(&repository, root, b, &cfg).unwrap())
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
                worktree_path_for(&repository, root, "feat/a", &cfg).unwrap()
            );
        }
        // Empty and NUL-bearing branches are typed refusals, not paths.
        let cfg = Config::default();
        assert!(worktree_path_for(&repository, root, "", &cfg).is_err());
        assert!(worktree_path_for(&repository, root, "a\0b", &cfg).is_err());
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
}
