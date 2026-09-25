//! Fail-closed identity and no-force removal for automatic merge collection.
//! A queue row is evidence to investigate, never authority to delete a path.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use thegn_core::util;

/// Settled admission for the only automatic teardown currently supported:
/// local worktrees with no observed resource attachment and no discoverable OCI
/// runtime. This is conservative eligibility, not global historical absence.
/// Never reload configuration or guess provider ownership from a worktree name.
pub(crate) struct LocalResources {
    selected: (Option<String>, Option<String>),
    _environment: thegn_core::env::Environment,
}
impl LocalResources {
    pub(crate) fn settle(
        cfg: &thegn_core::config::Config,
        db: &thegn_core::db::Db,
        root: &Path,
        path: &str,
    ) -> Result<Self, String> {
        use thegn_core::config::{DataMode, SandboxBackend};
        let selected = Self::selection(db, root, path)?;
        let environment = cfg.resolve_env(
            root,
            &thegn_core::remote::GitLoc::Local(path.into()),
            Path::new(path),
            selected.0.as_deref().or(selected.1.as_deref()),
        );
        if environment.unresolved_selection
            || !environment.placement.is_local()
            || !matches!(environment.data, DataMode::InEnv | DataMode::LocalExec)
            || environment.sandbox.vpn.is_enabled()
            || cfg.env.get(&environment.name).is_some_and(|env| {
                let provider = &env.provider;
                !provider.provider.is_empty()
                    || !provider.id.is_empty()
                    || !provider.exec_command.is_empty()
                    || !provider.interactive_command.is_empty()
                    || !provider.up_command.is_empty()
                    || !provider.down_command.is_empty()
                    || provider.auto_checkpoint
                    || provider.auto_provision
            })
            || cfg.placement.enabled
            || (environment.sandbox.enabled
                && !matches!(
                    environment.sandbox.backend,
                    SandboxBackend::None | SandboxBackend::Bwrap
                ))
        {
            return Err("managed or unresolved runtime requires explicit owned cleanup".into());
        }
        let value = Self {
            selected,
            _environment: environment,
        };
        value.revalidate(db, root, path)?;
        Ok(value)
    }

    fn selection(
        db: &thegn_core::db::Db,
        root: &Path,
        path: &str,
    ) -> Result<(Option<String>, Option<String>), String> {
        use thegn_core::store::WorkspaceStore;
        Ok((
            db.worktree_env(path).map_err(|e| e.to_string())?,
            db.workspace_env(&root.to_string_lossy())
                .map_err(|e| e.to_string())?,
        ))
    }

    pub(crate) fn revalidate(
        &self,
        db: &thegn_core::db::Db,
        root: &Path,
        path: &str,
    ) -> Result<(), String> {
        use thegn_core::store::{NotificationStore, PlacementStore, WorkspaceStore};
        if Self::selection(db, root, path)? != self.selected {
            return Err("selected worktree/workspace environment changed during cleanup".into());
        }
        if db.has_cleanup_tenancy(path).map_err(|e| e.to_string())?
            || db
                .has_persisted_worktree_session(path)
                .map_err(|e| e.to_string())?
            || db.has_cleanup_dispatch(path).map_err(|e| e.to_string())?
        {
            return Err("runtime/session/dispatch ownership requires explicit cleanup".into());
        }
        crate::agent::automatic_cleanup_resources_absent(path)?;
        crate::bridge_sup::automatic_cleanup_resources_absent(path)?;
        crate::worktree_lifecycle::automatic_cleanup_session_absent(Path::new(path))?;
        let search = std::env::var_os("PATH");
        #[cfg(test)]
        let search = TEST_GIT_CONFIG
            .with(|slot| {
                slot.borrow()
                    .as_ref()
                    .and_then(|p| p.parent())
                    .map(|p| p.as_os_str().to_owned())
            })
            .or(search);
        oci_resources_absent(search.as_deref())
    }
}

fn oci_resources_absent(search: Option<&std::ffi::OsStr>) -> Result<(), String> {
    let search = search.ok_or("OCI availability unknown: PATH missing")?;
    if search.len() > 64 * 1024 {
        return Err("OCI availability search exceeds bound".into());
    }
    let paths: Vec<_> = std::env::split_paths(search).take(257).collect();
    if paths.is_empty() || paths.len() > 256 || paths.iter().any(|p| !p.is_absolute()) {
        return Err(
            "OCI availability unknown: PATH must contain bounded absolute directories".into(),
        );
    }
    for backend in thegn_core::sandbox::Backend::all_oci() {
        for dir in &paths {
            for suffix in ["", ".exe", ".cmd", ".bat"] {
                match std::fs::symlink_metadata(dir.join(format!("{}{suffix}", backend.binary()))) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                    Err(_) => return Err("OCI availability could not be proven; explicit cleanup required".into()),
                    Ok(_) => return Err("OCI runtime discoverable; historical resource ownership requires explicit cleanup".into()),
                }
            }
        }
    }
    Ok(())
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    Dirty,
    Unsafe(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dirty => f.write_str("uncommitted, untracked or ignored files are present"),
            Self::Unsafe(reason) => f.write_str(reason),
        }
    }
}

fn unsafe_reason(message: impl Into<String>) -> Refusal {
    Refusal::Unsafe(message.into())
}

#[derive(Clone, Copy)]
enum IdentityKind {
    File,
    Directory,
}

/// Open the identity itself without following its final component. Unix's
/// nonblocking platform open prevents FIFO substitution from wedging cleanup.
/// Platforms unable to open a directory this way conservatively refuse.
fn identity(path: &Path, kind: IdentityKind) -> Result<same_file::Handle, Refusal> {
    let file = match kind {
        IdentityKind::File => crate::platform::open_nofollow(path),
        IdentityKind::Directory => crate::platform::open_directory_nofollow(path),
    }
    .map_err(|e| unsafe_reason(format!("identity handle unavailable: {e}")))?;
    let metadata = file.metadata().map_err(|e| unsafe_reason(e.to_string()))?;
    let accepted = match kind {
        IdentityKind::File => metadata.is_file(),
        IdentityKind::Directory => metadata.is_dir(),
    };
    if !accepted {
        return Err(unsafe_reason(
            "identity object has an unexpected filesystem type",
        ));
    }
    same_file::Handle::from_file(file).map_err(|e| unsafe_reason(e.to_string()))
}

/// Fixed local Git operations; output errors never mean a clean tree. Pipe
/// buffers and completion waits are bounded. OS spawn/filesystem calls are not
/// an atomic or interruptible filesystem boundary.
fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, Refusal> {
    git_input(root, args, None)
}

fn git_input(root: &Path, args: &[&str], input: Option<String>) -> Result<Vec<u8>, Refusal> {
    let mut command = util::git_cmd(root);
    #[cfg(test)]
    TEST_GIT_CONFIG.with(|config| {
        if let Some(path) = config.borrow().as_ref() {
            command
                .env("GIT_CONFIG_GLOBAL", path)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_CONFIG_COUNT")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_TEMPLATE_DIR");
        }
    });
    command
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .args(args);
    crate::bounded_git_probe::capture(command, input, args.first().copied().unwrap_or("probe"))
        .map_err(|error| unsafe_reason(error.to_string()))
}

fn text(root: &Path, args: &[&str]) -> Result<String, Refusal> {
    String::from_utf8(git(root, args)?)
        .map(|s| s.trim_end_matches('\n').to_string())
        .map_err(|_| unsafe_reason("Git identity is not valid UTF-8"))
}

fn canonical(path: &Path) -> Result<PathBuf, Refusal> {
    path.canonicalize()
        .map_err(|e| unsafe_reason(format!("unreadable path {}: {e}", path.display())))
}

pub(crate) fn common(root: &Path) -> Result<PathBuf, Refusal> {
    canonical(Path::new(&text(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?))
}

fn oid(root: &Path, reference: &str) -> Result<String, Refusal> {
    let value = text(
        root,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )?;
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unsafe_reason("invalid commit identity"));
    }
    Ok(value)
}

fn direct_ref(root: &Path, reference: &str) -> Result<(), Refusal> {
    let out = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(symref)",
            "--",
            reference,
        ],
    )?;
    if out != format!("{reference}\0\n").as_bytes() {
        return Err(unsafe_reason(
            "source and target must remain direct local branch refs",
        ));
    }
    Ok(())
}

/// Names of filter drivers that could actually execute here, i.e. those with a
/// `clean` or `process` command configured. `smudge`/`required` cannot run
/// during the `status` this module performs.
///
/// Config is data-only: only the driver NAME (a key fragment) is retained, and
/// no configured value is ever read into the returned set or logged.
fn configured_filter_drivers(path: &Path) -> Result<BTreeSet<String>, Refusal> {
    let config = git(path, &["config", "--null", "--includes", "--list"])?;
    let mut names = BTreeSet::new();
    for entry in config.split(|b| *b == 0) {
        let key = entry.split(|b| *b == b'\n').next().unwrap_or_default();
        let key = String::from_utf8_lossy(key).to_ascii_lowercase();
        let Some(rest) = key.strip_prefix("filter.") else {
            continue;
        };
        let Some(name) = rest
            .strip_suffix(".clean")
            .or_else(|| rest.strip_suffix(".process"))
        else {
            continue;
        };
        if !name.is_empty() {
            names.insert(name.to_string());
        }
    }
    Ok(names)
}

pub(crate) fn clean(path: &Path) -> Result<(), Refusal> {
    // status may run clean/process drivers while refreshing index content, and
    // fsmonitor=false alone cannot make that safe — so an APPLICABLE driver is
    // still a hard refusal.
    //
    // But a driver only runs when BOTH a `filter=<name>` attribute selects it
    // AND that driver is configured. The old check tested configuration alone
    // (THE-685), and `git config --list` includes global/system scope: one
    // machine-wide git-lfs install therefore disabled merged-worktree cleanup
    // in EVERY repository, including repositories with no LFS content at all.
    // Measured here: 35 merged worktrees stuck, the oldest 9 days past its TTL,
    // in a tree with no `.gitattributes` whatsoever.
    for driver in configured_filter_drivers(path)? {
        // The driver name is interpolated into a pathspec below, so only accept
        // names that cannot change how that pathspec parses. Anything stranger
        // keeps the old conservative refusal rather than being trusted.
        if !driver
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(unsafe_reason(
                "a Git filter driver with an unexpected name is configured; cleanup requires explicit review",
            ));
        }
        // Attribute lookup only — reads the index and .gitattributes, and
        // cannot itself invoke a driver the way `status`/`add` would.
        let applied = git(
            path,
            &["ls-files", "-z", "--", &format!(":(attr:filter={driver})")],
        )?;
        if applied.split(|b| *b == 0).any(|entry| !entry.is_empty()) {
            return Err(unsafe_reason(format!(
                "tracked paths use the {driver:?} Git clean/process filter; cleanup requires explicit review"
            )));
        }
    }
    let flags = git(path, &["ls-files", "-v", "-z"])?;
    if flags.split(|b| *b == 0).any(|entry| {
        entry
            .first()
            .is_some_and(|b| *b == b'S' || b.is_ascii_lowercase())
    }) {
        return Err(unsafe_reason(
            "skip-worktree/assume-unchanged entries make cleanliness unverified",
        ));
    }
    let index = git(path, &["ls-files", "--stage", "-z"])?;
    if index
        .split(|b| *b == 0)
        .any(|entry| entry.starts_with(b"160000 "))
    {
        return Err(unsafe_reason(
            "submodule worktrees require explicit cleanup",
        ));
    }
    if git(
        path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ],
    )?
    .is_empty()
    {
        Ok(())
    } else {
        Err(Refusal::Dirty)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Registration {
    path: PathBuf,
    branch: String,
    head: String,
    protected: bool,
}

fn registrations(root: &Path) -> Result<Vec<Registration>, Refusal> {
    let bytes = git(root, &["worktree", "list", "--porcelain", "-z"])?;
    let text = String::from_utf8(bytes).map_err(|_| unsafe_reason("non-UTF8 worktree registry"))?;
    let mut rows = Vec::new();
    for record in text.split("\0\0").filter(|s| !s.is_empty()) {
        let fields: Vec<_> = record.split('\0').filter(|s| !s.is_empty()).collect();
        let path = fields
            .first()
            .and_then(|s| s.strip_prefix("worktree "))
            .ok_or_else(|| unsafe_reason("malformed Git worktree registry"))?;
        rows.push(Registration {
            path: PathBuf::from(path),
            branch: fields
                .iter()
                .find_map(|s| s.strip_prefix("branch "))
                .unwrap_or("")
                .to_string(),
            head: fields
                .iter()
                .find_map(|s| s.strip_prefix("HEAD "))
                .unwrap_or("")
                .to_string(),
            protected: rows.is_empty()
                || fields.iter().any(|s| {
                    *s == "bare"
                        || *s == "detached"
                        || s.starts_with("locked")
                        || s.starts_with("prunable")
                }),
        });
    }
    if rows.is_empty() {
        return Err(unsafe_reason("empty Git worktree registry"));
    }
    Ok(rows)
}

/// A private snapshot held only across the synchronous, claimed transaction.
pub(crate) struct Verified {
    history: crate::canonical_history::CanonicalHistory,
    root: PathBuf,
    path: PathBuf,
    common: PathBuf,
    gitdir: PathBuf,
    branch: String,
    target: String,
    head: String,
    landed: Option<String>,
    identities: Vec<same_file::Handle>,
}

impl Verified {
    pub(crate) fn probe(
        root: &Path,
        worktree: &str,
        branch: &str,
        target: &str,
        landed: Option<&str>,
    ) -> Result<Self, Refusal> {
        let root = canonical(root)?;
        let history = crate::canonical_history::CanonicalHistory::capture(&root)
            .map_err(|error| unsafe_reason(error.to_string()))?;
        let path = canonical(Path::new(worktree))?;
        if path != Path::new(worktree) || path == root {
            return Err(unsafe_reason(
                "main checkout or noncanonical/symlink worktree path",
            ));
        }
        let common_dir = common(&root)?;
        if common(&path)? != common_dir {
            return Err(unsafe_reason("worktree belongs to a different repository"));
        }
        if canonical(Path::new(&text(&path, &["rev-parse", "--show-toplevel"])?))? != path {
            return Err(unsafe_reason(
                "Git working directory is redirected or not a root",
            ));
        }
        let branch_ref = format!("refs/heads/{branch}");
        let target_ref = format!("refs/heads/{target}");
        if branch_ref.len() > 1024 || target_ref.len() > 1024 {
            return Err(unsafe_reason("branch identity exceeds cleanup bounds"));
        }
        for reference in [&branch_ref, &target_ref] {
            git(&root, &["check-ref-format", reference])?;
        }
        if branch == target || branch.is_empty() || target.is_empty() {
            return Err(unsafe_reason(
                "target branch cannot be automatically removed",
            ));
        }
        let rows = registrations(&root)?;
        let row = rows
            .iter()
            .find(|r| r.path == path)
            .ok_or_else(|| unsafe_reason("path is not an exact registered worktree"))?;
        if row.protected || row.branch != branch_ref {
            return Err(unsafe_reason(
                "main, locked, detached or mismatched worktree identity",
            ));
        }
        if text(&path, &["symbolic-ref", "--quiet", "HEAD"])? != branch_ref {
            return Err(unsafe_reason("worktree branch changed"));
        }
        let head = oid(&root, &branch_ref)?;
        direct_ref(&root, &branch_ref)?;
        direct_ref(&root, &target_ref)?;
        if oid(&path, "HEAD")? != head || row.head != head {
            return Err(unsafe_reason(
                "worktree and branch commit identities disagree",
            ));
        }
        let target_oid = oid(&root, &target_ref)?;
        git(&root, &["merge-base", "--is-ancestor", &head, &target_oid])?;
        if let Some(landed) = landed {
            if oid(&root, landed)? != landed {
                return Err(unsafe_reason("invalid landed commit identity"));
            }
            git(&root, &["merge-base", "--is-ancestor", landed, &target_oid])?;
        }
        let gitdir = canonical(Path::new(&text(
            &path,
            &["rev-parse", "--absolute-git-dir"],
        )?))?;
        if gitdir == common_dir
            || !gitdir.starts_with(common_dir.join("worktrees"))
            || !std::fs::symlink_metadata(path.join(".git"))
                .map_err(|e| unsafe_reason(e.to_string()))?
                .is_file()
        {
            return Err(unsafe_reason(
                "not a verified linked-worktree metadata directory",
            ));
        }
        clean(&path)?;
        let identities = [
            (&path, IdentityKind::Directory),
            (&common_dir, IdentityKind::Directory),
            (&gitdir, IdentityKind::Directory),
            (&path.join(".git"), IdentityKind::File),
            (&root, IdentityKind::Directory),
        ]
        .into_iter()
        .map(|(path, kind)| identity(path, kind))
        .collect::<Result<Vec<_>, _>>()?;
        history
            .revalidate()
            .map_err(|error| unsafe_reason(error.to_string()))?;
        Ok(Self {
            history,
            root,
            path,
            common: common_dir,
            gitdir,
            branch: branch.into(),
            target: target.into(),
            head,
            landed: landed.map(str::to_owned),
            identities,
        })
    }

    pub(crate) fn revalidate(&self) -> Result<(), Refusal> {
        self.history
            .revalidate()
            .map_err(|error| unsafe_reason(error.to_string()))?;
        let now = Self::probe(
            &self.root,
            self.path
                .to_str()
                .ok_or_else(|| unsafe_reason("non-UTF8 path"))?,
            &self.branch,
            &self.target,
            self.landed.as_deref(),
        )?;
        if self.common != now.common
            || self.gitdir != now.gitdir
            || self.head != now.head
            || self.identities != now.identities
        {
            return Err(unsafe_reason("worktree identity changed during cleanup"));
        }
        Ok(())
    }

    pub(crate) fn verify_cached_repository(&self, path: &Path) -> Result<(), Refusal> {
        if common(path)? != self.common {
            return Err(unsafe_reason(
                "cached repository belongs to a different Git repository",
            ));
        }
        self.verify_repository()
    }

    #[cfg(test)]
    fn remove(&self) -> Result<(), Refusal> {
        self.remove_checked(&|| Ok(()))
    }

    pub(crate) fn remove_checked(
        &self,
        final_guard: &dyn Fn() -> Result<(), String>,
    ) -> Result<(), Refusal> {
        let _lock = self.mutation_lock()?;
        self.revalidate()?;
        final_guard().map_err(unsafe_reason)?;
        git(
            &self.root,
            &[
                "worktree",
                "remove",
                "--",
                self.path
                    .to_str()
                    .ok_or_else(|| unsafe_reason("non-UTF8 path"))?,
            ],
        )?;
        self.verify_removed()
    }

    pub(crate) fn verify_removed(&self) -> Result<(), Refusal> {
        self.verify_repository()?;
        match std::fs::symlink_metadata(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(unsafe_reason("physical removal could not be verified")),
        }
        if registrations(&self.root)?
            .iter()
            .any(|r| r.path == self.path)
        {
            return Err(unsafe_reason("Git still registers the removed path"));
        }
        Ok(())
    }

    fn verify_repository(&self) -> Result<(), Refusal> {
        self.history
            .revalidate()
            .map_err(|error| unsafe_reason(error.to_string()))?;
        let root = identity(&self.root, IdentityKind::Directory)?;
        let common_handle = identity(&self.common, IdentityKind::Directory)?;
        if root != self.identities[4]
            || common_handle != self.identities[1]
            || common(&self.root)? != self.common
        {
            return Err(unsafe_reason("repository object identity changed"));
        }
        Ok(())
    }

    fn mutation_lock(&self) -> Result<std::fs::File, Refusal> {
        self.verify_repository()?;
        let path = self.common.join("thegn-git.lock");
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(unsafe_reason("mutation lock is not a regular file")),
        }
        let file = match std::fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::platform::open_nofollow(&path)
                    .map_err(|e| unsafe_reason(format!("mutation lock unavailable: {e}")))?
            }
            Err(error) => return Err(unsafe_reason(format!("mutation lock unavailable: {error}"))),
        };
        if !file
            .metadata()
            .map_err(|e| unsafe_reason(e.to_string()))?
            .is_file()
            || !std::fs::symlink_metadata(&path)
                .map_err(|e| unsafe_reason(e.to_string()))?
                .is_file()
            || same_file::Handle::from_file(
                file.try_clone().map_err(|e| unsafe_reason(e.to_string()))?,
            )
            .map_err(|e| unsafe_reason(e.to_string()))?
                != identity(&path, IdentityKind::File)?
        {
            return Err(unsafe_reason("mutation lock identity changed"));
        }
        file.try_lock()
            .map_err(|e| unsafe_reason(format!("repository mutation is busy: {e}")))?;
        self.verify_repository()?;
        Ok(file)
    }
}

#[cfg(test)]
#[path = "merge_cleanup_tests.rs"]
mod tests;

#[cfg(test)]
thread_local! {
    static TEST_GIT_CONFIG: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Test-only ambient state isolation. Commands are constructed on the owning
/// test thread; pipe workers do not construct Git commands or inherit this slot.
#[cfg(test)]
pub(crate) struct TestIsolation {
    previous: Option<PathBuf>,
    _env: thegn_core::testenv::EnvGuard,
    state: tempfile::TempDir,
}
#[cfg(test)]
impl TestIsolation {
    pub(crate) fn new() -> Self {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().to_str().unwrap();
        let config = state.path().join("absent.gitconfig");
        let env = thegn_core::testenv::EnvGuard::mutate_pairs(&[
            ("XDG_STATE_HOME", Some(path)),
            ("XDG_CONFIG_HOME", Some(path)),
            ("LOCALAPPDATA", Some(path)),
            ("APPDATA", Some(path)),
            ("THEGN_DIR", Some(path)),
            ("TMPDIR", Some(path)),
            ("TMP", Some(path)),
            ("TEMP", Some(path)),
            ("THEGN_SANDBOX_ENABLED", Some("false")),
            ("THEGN_SANDBOX_BACKEND", Some("none")),
            ("THEGN_PROFILE", Some("")),
            ("GIT_CONFIG_GLOBAL", Some(config.to_str().unwrap())),
            ("GIT_CONFIG_NOSYSTEM", Some("1")),
            ("GIT_CONFIG_COUNT", None),
            ("GIT_CONFIG_PARAMETERS", None),
            ("GIT_TEMPLATE_DIR", None),
            ("GIT_DIR", None),
            ("GIT_COMMON_DIR", None),
            ("GIT_WORK_TREE", None),
            ("GIT_INDEX_FILE", None),
            ("GIT_OBJECT_DIRECTORY", None),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", None),
        ]);
        let previous = TEST_GIT_CONFIG.with(|slot| slot.replace(Some(config)));
        Self {
            previous,
            _env: env,
            state,
        }
    }

    pub(crate) fn git(&self, root: &Path) -> std::process::Command {
        let mut command = util::git_cmd(root);
        command
            .env(
                "GIT_CONFIG_GLOBAL",
                self.state.path().join("absent.gitconfig"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_TEMPLATE_DIR");
        command
    }
}

#[cfg(test)]
pub(crate) fn test_git(root: &Path) -> std::process::Command {
    let mut command = util::git_cmd(root);
    TEST_GIT_CONFIG.with(|slot| {
        command
            .env(
                "GIT_CONFIG_GLOBAL",
                slot.borrow().as_ref().expect("private Git test scope"),
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_TEMPLATE_DIR");
    });
    command
}
#[cfg(test)]
impl Drop for TestIsolation {
    fn drop(&mut self) {
        TEST_GIT_CONFIG.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}
