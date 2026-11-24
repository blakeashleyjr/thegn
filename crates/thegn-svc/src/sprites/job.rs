//! Selected build/test Jobs on one project-bound Sprite.
//!
//! This is the provider-side executor for the typed selected-runner route in
//! [`thegn_job_spec::selected`]: a pinned source digest, one closed [`Recipe`]
//! and the route's deadline go in, a bounded [`SelectedJobResult`] comes out. It
//! is deliberately not a general command transport: the only argv it ever sends
//! is [`Recipe::argv`], behind a launcher and the keep-awake hold
//! ([`thegn_core::sprite_hold`]), whose script drops the hold when the command
//! exits, so a hold outlives a quiet build but never the attempt.
//!
//! Accounting, in the order it happens:
//!
//! 1. **reserve** — attempt number, binding epoch and the spend cap are written
//!    to the attempt ledger *before* anything reaches the sprite, and the
//!    attempt is refused when the project's envelope is already committed or
//!    when the sprite is already bound to another project. The attempt number
//!    is allocated under the ledger lock and never reused.
//! 2. **materialize** — the authorized snapshot bytes are written into a unique
//!    per-attempt checkout under a separate transfer budget (transfer time
//!    never counts against the build deadline). Every file is read back and
//!    hashed, the checkout is listed, and the REAPI root `Directory` digest of
//!    what the sprite holds must equal the pinned digest, so an unverified or
//!    polluted transfer never runs.
//! 3. **dispatch** — the record moves to [`AttemptState::Dispatching`] *before*
//!    the exec socket opens, and the socket is opened exactly once: the
//!    Sprites exec API starts the command from the WebSocket upgrade, so a
//!    retried upgrade could start a second build. A failure is only "never
//!    ran" ([`AttemptState::Rejected`]) when the provider definitively rejected
//!    the upgrade (an HTTP 4xx). Any other failure to open leaves the outcome
//!    unknown: the attempt reads back as [`SelectedCompletion::Indeterminate`]
//!    with cleanup pending, and it is never rerun as an implicit recovery.
//! 4. **run** — stdout and stderr stream into bounded tails until the original
//!    exit, the deadline, or the end of the stream without one. At the deadline
//!    the runner kills the owned process group on the sprite (a second,
//!    idempotent exec), releases the keep-awake hold, and reports
//!    [`SelectedCompletion::TimedOut`]; cleanup is `Done` only when the kill was
//!    confirmed, otherwise `Pending`.
//! 5. **report** — provider identity, session handle, exit, signal, cleanup
//!    state and the per-attempt cost estimate land in the ledger and in the
//!    receipt, so a client that lost its channel can reconcile by attempt
//!    number. Once cleanup is not pending the per-attempt checkout, home and
//!    control file are deleted; only the project cache remains.
//!
//! Contract with the route: on the selected route every `Err` that
//! [`SelectedRunner::submit`] returns reads back as *indeterminate*, because the
//! runner call itself may already have started work. This module therefore only
//! claims "never ran" in its own ledger (state `rejected`) when it can prove it.
//!
//! Isolation: one sprite serves one project (a local claim registry plus an
//! owner marker on the sprite). The recipe runs under `env -i` with a
//! per-attempt `HOME` and a project-scoped `CARGO_HOME`/`CARGO_TARGET_DIR`, so
//! an adopted sprite's `~/.claude`, `~/.codex` and cloud credentials are not
//! reachable through the environment. A build is still arbitrary code running
//! as the sprite's user: a sprite that holds agent logins should not be used as
//! a job sprite.
//!
//! The ledger is a durable private file store
//! (`$XDG_STATE_HOME/thegn/sprites-jobs/<project>/`) rather than the DB because
//! both the dispatcher and a later reconciliation read it without a live
//! session. Ledger I/O inside [`SpritesSelectedRunner::run`] runs on blocking
//! threads; [`SpritesSelectedRunner::submit`] runs the attempt on the caller's
//! own thread.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thegn_cas::{Digest, Entry, encode_directory};
use thegn_core::fsperm::{PrivateDir, ensure_private_dir, open_private_dir, owned_by_current_user};
use thegn_core::sprite_hold::{HOLD_EXPIRE_SECS, hold_name, wrap_argv};
use thegn_job_spec::selected::MAX_SELECTED_LOG_BYTES;
use thegn_job_spec::{
    Recipe, RunnerRefusal, SelectedCompletion, SelectedJobResult, SelectedRunner,
};

use crate::provider::{
    ExecControl, ExecFrame, ExecSpec, ExecStderr, ProviderFiles, STDERR_TAIL_CAP, SpritesProvider,
    is_definite_exec_rejection, join_remote,
};

/// The provider name written into every record and receipt.
pub const PROVIDER: &str = "sprites";

/// Root of the per-attempt checkouts on the sprite.
pub const DEFAULT_CHECKOUT_ROOT: &str = "/workspace/.thegn/jobs";

/// Prefix of the job-spec alias a `runner:` selects (`sprites-<sprite>`).
pub const ALIAS_PREFIX: &str = "sprites-";

/// Longest wall clock an attempt may run, and the default when the route
/// forwards none shorter. The route's deadline is clamped to this.
pub const DEFAULT_ATTEMPT_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Default budget for moving the snapshot onto the sprite. Separate from the
/// build deadline: a slow transfer never shortens the build.
pub const DEFAULT_TRANSFER_BUDGET: Duration = Duration::from_secs(15 * 60);

/// Default per-attempt spend cap, in micro-USD.
pub const DEFAULT_SPEND_CAP_MICROS: u64 = 60_000;

/// Most files one snapshot may carry.
pub const SNAPSHOT_MAX_FILES: usize = 20_000;

/// Most bytes one snapshot may carry.
pub const SNAPSHOT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Largest single file a snapshot may carry. Below the provider's 256 MiB file
/// transfer limit, so every captured file can also be read back and verified.
pub const SNAPSHOT_MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;

/// Files in flight at once while materializing.
const TRANSFER_CONCURRENCY: usize = 8;

/// Largest ledger record read back (a record is under a kilobyte).
const RECORD_LIMIT: u64 = 256 * 1024;

const BINDING_FILE: &str = "binding.json";
const RESERVE_LOCK: &str = ".reserve.lock";
/// Directory beside the per-project ledgers holding one claim per sprite.
const SPRITE_REGISTRY: &str = ".sprite-bindings";

/// How long a freshly opened session may take to announce its handle.
const SESSION_HANDLE_WAIT: Duration = Duration::from_secs(10);

/// How long a closed stream's protocol verdict may take to arrive.
const VERDICT_WAIT: Duration = Duration::from_secs(2);

/// How long the kill exec may take before cleanup is reported pending.
const CLEANUP_BUDGET: Duration = Duration::from_secs(45);

/// Written to stderr by the launcher when it could not start the recipe, so a
/// launcher failure is never confused with a recipe that exits 125.
pub const LAUNCH_FAILED_MARKER: &str = "thegn-job-launch-failed";

/// Directory names a snapshot never carries, at any depth: version control,
/// thegn's own state, and agent/provider/cloud credentials. A directory is
/// pruned with its whole subtree.
const DENIED_DIRS: [&str; 10] = [
    ".git", ".thegn", ".ssh", ".aws", ".gnupg", ".claude", ".codex", ".kube", ".docker", ".direnv",
];

/// File names a snapshot never carries, at any depth.
const DENIED_NAMES: [&str; 12] = [
    ".env",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".pgpass",
    ".dockercfg",
    ".git-credentials",
    "credentials.json",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// True when `rel` is content a job snapshot must never carry. Every component
/// is checked, so `services/api/.env` and `pkg/.claude/x` are denied exactly
/// like their root-level forms.
fn denied_path(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        if DENIED_DIRS.contains(part)
            || DENIED_NAMES.contains(part)
            || (part.starts_with(".env.") && part.len() > ".env.".len())
        {
            return true;
        }
        let next = parts.get(index + 1).copied();
        if *part == ".cargo" && next.is_some_and(|name| name.starts_with("credentials")) {
            return true;
        }
        if *part == ".config" && next == Some("gh") {
            return true;
        }
    }
    false
}

/// Validate a repo-relative path: relative, no traversal, no control bytes, a
/// portable REAPI entry name per component, and not one of the credential
/// trees. Every transferred path goes through here.
fn checked_rel_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "path must not be empty");
    ensure!(path.len() <= 1024, "path is longer than 1024 bytes");
    ensure!(!path.starts_with('/'), "path must be relative: {path}");
    ensure!(!path.contains('\\'), "path must use `/` separators: {path}");
    for component in path.split('/') {
        ensure!(
            !component.is_empty() && component != "." && component != "..",
            "path has an empty or traversing component: {path}"
        );
        ensure!(component.len() <= 255, "path component is too long: {path}");
        ensure!(
            !component.chars().any(char::is_control),
            "path has a control character: {path}"
        );
        ensure!(
            !component.contains(':'),
            "path component is not a portable directory entry name: {path}"
        );
    }
    ensure!(
        !denied_path(path),
        "credentials and version-control state are never part of a job snapshot: {path}"
    );
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
            hex.push_str(&format!("{byte:02x}"));
            hex
        })
}

/// A short unique token for names that must not collide across attempts, ledger
/// resets or hosts: wall clock, process id and a process-local counter.
fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hex(&hasher.finalize().as_slice()[..4])
}

/// One node of the REAPI tree being digested.
enum Node {
    File { digest: Digest, executable: bool },
    Dir(BTreeMap<String, Node>),
}

fn place(
    dir: &mut BTreeMap<String, Node>,
    path: &str,
    digest: Digest,
    executable: bool,
) -> Result<()> {
    match path.split_once('/') {
        None => {
            let previous = dir.insert(path.to_owned(), Node::File { digest, executable });
            ensure!(previous.is_none(), "duplicate or conflicting entry {path}");
            Ok(())
        }
        Some((head, rest)) => {
            let node = dir
                .entry(head.to_owned())
                .or_insert_with(|| Node::Dir(BTreeMap::new()));
            match node {
                Node::Dir(children) => place(children, rest, digest, executable),
                Node::File { .. } => bail!("{head} is both a file and a directory"),
            }
        }
    }
}

fn dir_digest(dir: &BTreeMap<String, Node>) -> Result<Digest> {
    let entries = dir
        .iter()
        .map(|(name, node)| {
            let entry = match node {
                Node::File { digest, executable } => Entry::File {
                    digest: digest.clone(),
                    executable: *executable,
                },
                Node::Dir(children) => Entry::Directory(dir_digest(children)?),
            };
            Ok((name.clone(), entry))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Digest::of(&encode_directory(entries)?))
}

/// The canonical REAPI root `Directory` digest of a tree of files, each given
/// as `(relative path, content digest, executable)`. This is the identity a
/// `Snapshot::Pinned` carries (`docs/rfcs/job-spec-v0.md`): the digest of the
/// canonical serialized root `Directory`, built with [`thegn_cas`].
pub fn root_digest<'a>(
    entries: impl IntoIterator<Item = (&'a str, Digest, bool)>,
) -> Result<Digest> {
    let mut root = BTreeMap::new();
    for (path, digest, executable) in entries {
        place(&mut root, path, digest, executable)?;
    }
    dir_digest(&root)
}

/// One captured file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotFile {
    bytes: Vec<u8>,
    executable: bool,
}

/// The authorized input content of one attempt, captured before dispatch.
///
/// Only repo-relative paths survive [`insert`](Self::insert); the digest is the
/// REAPI root `Directory` digest of exactly the bytes (and executable bits)
/// that will be transferred, so a capture that lost or added one byte cannot
/// match the pinned identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotContent {
    files: BTreeMap<String, SnapshotFile>,
    total: u64,
}

impl SnapshotContent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one non-executable repo-relative file. See
    /// [`insert_file`](Self::insert_file).
    pub fn insert(&mut self, path: &str, bytes: Vec<u8>) -> Result<()> {
        self.insert_file(path, bytes, false)
    }

    /// Add one repo-relative file with its executable bit. Refuses traversal,
    /// absolute paths, the credential trees (at any depth), a file that is also
    /// a directory, and anything over the file, count or byte caps, so a caller
    /// cannot widen the snapshot by accident.
    pub fn insert_file(&mut self, path: &str, bytes: Vec<u8>, executable: bool) -> Result<()> {
        checked_rel_path(path)?;
        ensure!(
            bytes.len() as u64 <= SNAPSHOT_MAX_FILE_BYTES,
            "snapshot file {path} is larger than {SNAPSHOT_MAX_FILE_BYTES} bytes"
        );
        ensure!(
            self.files.len() < SNAPSHOT_MAX_FILES || self.files.contains_key(path),
            "snapshot has more than {SNAPSHOT_MAX_FILES} files"
        );
        for (index, ch) in path.char_indices() {
            ensure!(
                ch != '/' || !self.files.contains_key(&path[..index]),
                "snapshot path {path} lies below a file"
            );
        }
        let below = format!("{path}/");
        ensure!(
            self.files
                .range(below.clone()..)
                .next()
                .is_none_or(|(key, _)| !key.starts_with(&below)),
            "snapshot path {path} is also a directory"
        );
        let replaced = self
            .files
            .get(path)
            .map_or(0, |existing| existing.bytes.len() as u64);
        let total = self
            .total
            .saturating_sub(replaced)
            .saturating_add(bytes.len() as u64);
        ensure!(
            total <= SNAPSHOT_MAX_BYTES,
            "snapshot is larger than {SNAPSHOT_MAX_BYTES} bytes"
        );
        self.total = total;
        self.files
            .insert(path.to_owned(), SnapshotFile { bytes, executable });
        Ok(())
    }

    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(|file| file.bytes.as_slice())
    }

    pub fn is_executable(&self, path: &str) -> bool {
        self.files.get(path).is_some_and(|file| file.executable)
    }

    pub fn files(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files
            .iter()
            .map(|(path, file)| (path.as_str(), file.bytes.as_slice()))
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total
    }

    /// The REAPI root `Directory` digest the pinned source identity is compared
    /// against.
    pub fn digest(&self) -> Result<Digest> {
        root_digest(
            self.files
                .iter()
                .map(|(path, file)| (path.as_str(), Digest::of(&file.bytes), file.executable)),
        )
    }
}

/// Where an attempt's authorized input bytes come from.
///
/// Capture is synchronous because it reads the local tree: the dispatcher calls
/// it on its own thread, before the provider runtime is entered, so no blocking
/// filesystem walk ever runs on the event loop.
pub trait SnapshotSource: Send + Sync {
    fn capture(&self) -> Result<SnapshotContent>;
}

/// A snapshot captured from a local worktree directory with the same
/// `.gitignore`-aware walk the provider `sync` projection uses: tracked and
/// non-ignored untracked files, never ignored build output (`target/`,
/// `node_modules/`, ...). Version control, `.thegn`, agent homes and credential
/// files are denied at every depth. Symlinks are refused explicitly (capture
/// fails), because a link cannot be reproduced on the sprite without a second
/// command channel and a skipped link would silently change the tree. The
/// executable bit is preserved.
///
/// Ignore rules come from the tree itself (`.gitignore`, `.git/info/exclude`,
/// `.ignore`); the user's global gitignore is deliberately not consulted, so
/// the captured set (and its digest) does not depend on the host.
#[derive(Debug, Clone)]
pub struct DirSnapshot {
    root: PathBuf,
}

impl DirSnapshot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

fn rel_of(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let rel = rel.to_str()?;
    (!rel.is_empty()).then(|| rel.replace(std::path::MAIN_SEPARATOR, "/"))
}

impl SnapshotSource for DirSnapshot {
    fn capture(&self) -> Result<SnapshotContent> {
        ensure!(
            self.root.is_dir(),
            "snapshot root {} is not a directory",
            self.root.display()
        );
        let prune_root = self.root.clone();
        let walk = ignore::WalkBuilder::new(&self.root)
            .hidden(false)
            .git_ignore(true)
            .git_exclude(true)
            .git_global(false)
            .parents(false)
            .require_git(false)
            .follow_links(false)
            .filter_entry(move |entry| {
                rel_of(&prune_root, entry.path()).is_none_or(|rel| !denied_path(&rel))
            })
            .build();
        let mut content = SnapshotContent::new();
        for entry in walk {
            let entry = entry.context("cannot walk the snapshot tree")?;
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            let Some(rel) = rel_of(&self.root, entry.path()) else {
                continue;
            };
            if file_type.is_symlink() {
                bail!(
                    "snapshot refuses the symlink {rel}: a link cannot be reproduced faithfully \
                     on the sprite, so a tree that contains one is not a supported job input"
                );
            }
            if !file_type.is_file() {
                continue;
            }
            // Size first: one huge file must be refused before it is read.
            let metadata = entry
                .metadata()
                .with_context(|| format!("cannot stat snapshot file {rel}"))?;
            ensure!(
                metadata.len() <= SNAPSHOT_MAX_FILE_BYTES,
                "snapshot file {rel} is larger than {SNAPSHOT_MAX_FILE_BYTES} bytes"
            );
            ensure!(
                content.total_bytes().saturating_add(metadata.len()) <= SNAPSHOT_MAX_BYTES,
                "snapshot is larger than {SNAPSHOT_MAX_BYTES} bytes"
            );
            let bytes = std::fs::read(entry.path())
                .with_context(|| format!("cannot read snapshot file {rel}"))?;
            content.insert_file(
                &rel,
                bytes,
                crate::plugin::platform::is_executable_file(&metadata),
            )?;
        }
        Ok(content)
    }
}

/// Which project may use a Sprite, and under which account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectBinding {
    project: String,
    account: String,
    sprite: String,
}

/// A single path component: no separators, no traversal, no control bytes.
fn checked_component(value: &str, what: &str, max: usize) -> Result<()> {
    ensure!(!value.is_empty(), "{what} must not be empty");
    ensure!(value.len() <= max, "{what} is longer than {max} bytes");
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        "{what} must be an ASCII name ([A-Za-z0-9._-])"
    );
    ensure!(
        !value.starts_with('.') && !value.starts_with('-'),
        "{what} must not start with a separator"
    );
    ensure!(
        !value.contains(".."),
        "{what} must not contain a double dot"
    );
    Ok(())
}

/// The job-spec alias rule (`thegn-job-spec` validates a `runner:` the same
/// way): lowercase alphanumeric with interior dashes.
fn alias_is_valid(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 128
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && alias.as_bytes()[0].is_ascii_alphanumeric()
        && alias.as_bytes()[alias.len() - 1].is_ascii_alphanumeric()
}

impl ProjectBinding {
    pub fn new(project: &str, account: &str, sprite: &str) -> Result<Self> {
        checked_component(project, "project", 64)?;
        checked_component(account, "account", 128)?;
        checked_component(sprite, "sprite", 64)?;
        let binding = Self {
            project: project.to_owned(),
            account: account.to_owned(),
            sprite: sprite.to_owned(),
        };
        ensure!(
            alias_is_valid(&binding.alias()),
            "sprite {sprite:?} cannot be a selected runner: an alias is `{ALIAS_PREFIX}<sprite>` \
             and must be lowercase alphanumeric with interior dashes"
        );
        Ok(binding)
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    pub fn sprite(&self) -> &str {
        &self.sprite
    }

    /// The alias a `runner:` selects for this sprite.
    pub fn alias(&self) -> String {
        format!("{ALIAS_PREFIX}{}", self.sprite)
    }

    fn project_root(&self, root: &str) -> String {
        format!("{}/{}", root.trim_end_matches('/'), self.project)
    }

    /// The per-attempt checkout on the sprite: project-scoped and unique per
    /// attempt key (`<attempt>-<epoch>-<nonce>`), so it never inherits another
    /// attempt's tree, even after a ledger reset.
    pub fn checkout(&self, key: &str, root: &str) -> String {
        format!("{}/co/{key}", self.project_root(root))
    }

    /// The per-attempt `HOME` the recipe runs under (never the sprite user's).
    pub fn home(&self, key: &str, root: &str) -> String {
        format!("{}/home/{key}", self.project_root(root))
    }

    /// The file the launcher writes the recipe's process-group id into, so the
    /// owned command tree can be killed later.
    pub fn control_file(&self, key: &str, root: &str) -> String {
        format!("{}/ctl/{key}.pid", self.project_root(root))
    }

    /// The project-scoped build cache (`CARGO_HOME` and `CARGO_TARGET_DIR`),
    /// retained between attempts so a warm loop does not rebuild from scratch.
    /// It is the only state one attempt may leave for the next. Cache policy is
    /// the runner's own: it is not a job-spec `caches` entry, which the route
    /// rejects.
    pub fn cache(&self, root: &str) -> String {
        format!("{}/cache", self.project_root(root))
    }

    /// The marker on the sprite naming the one project that may use it.
    pub fn owner_marker(root: &str) -> String {
        format!("{}/.owner", root.trim_end_matches('/'))
    }
}

/// `<attempt>-<epoch>-<nonce>`: names one attempt's directories.
fn attempt_key(attempt: u64, epoch: u64) -> String {
    format!("{attempt}-{epoch}-{}", nonce())
}

/// Lifecycle of one reserved attempt.
///
/// The record is written before every step that can reach the provider, so an
/// interrupted run is never rerun blindly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttemptState {
    /// Slot and envelope reserved; nothing was sent.
    Reserved,
    /// A dispatch request may exist; its outcome is unknown until the provider
    /// acknowledges a session.
    Dispatching,
    /// The provider acknowledged a session; `session` names it.
    Running,
    /// A terminal exit was observed.
    Finished,
    /// The recipe's process was terminated by a signal (`signal`).
    Signalled,
    /// The runner stopped the recipe at the submitted deadline.
    TimedOut,
    /// Cancelled at our request.
    Cancelled,
    /// The stream ended without a terminal exit, or the dispatch outcome is
    /// unknown. The command may have run.
    Indeterminate,
    /// Closed before any command could have run: the transfer failed, or the
    /// provider definitively rejected the exec upgrade (an HTTP 4xx). This is
    /// the only state that proves "never ran".
    Rejected,
}

/// Cleanup state of the command tree an attempt left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cleanup {
    /// The command ended by itself (or never ran); nothing is outstanding.
    NotNeeded,
    /// The tree may still be running; a reconciler confirms it.
    Pending,
    /// Confirmed gone ([`AttemptStore::mark_cleaned`], or a confirmed kill).
    Done,
}

/// The binding a project's ledger directory is pinned to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BindingRecord {
    pub project: String,
    pub account: String,
    pub sprite: String,
    /// Increments when the project is rebound to another sprite or account, so
    /// older attempts stay readable but are visibly stale.
    pub epoch: u64,
}

/// The claim a project holds on a sprite (one project per sprite).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpriteClaim {
    project: String,
}

/// Durable record of one attempt. Everything a later reconciliation needs is
/// here: who ran, where, under which handle, how it ended, what it cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptRecord {
    pub provider: String,
    pub project: String,
    pub account: String,
    pub sprite: String,
    pub attempt: u64,
    pub epoch: u64,
    /// `<attempt>-<epoch>-<nonce>`: names the attempt's checkout, home and
    /// control file on the sprite.
    pub key: String,
    pub recipe: Recipe,
    pub source: Digest,
    pub checkout: String,
    pub state: AttemptState,
    /// The provider's exec session id once acknowledged (for reattach).
    pub session: Option<String>,
    /// The keep-awake task name the command was wrapped in.
    pub hold: Option<String>,
    /// The spend this attempt reserved against the envelope.
    pub reserved_micros: u64,
    /// The estimated spend once the attempt reported.
    pub spent_micros: Option<u64>,
    pub created_ms: i64,
    pub deadline_ms: i64,
    pub finished_ms: Option<i64>,
    /// The original exit code, when one was proven.
    pub exit: Option<i32>,
    /// The terminating signal, when one was observed.
    pub signal: Option<i32>,
    pub cleanup: Cleanup,
    /// The verified REAPI root digest of the materialized checkout.
    pub materialized: Option<Digest>,
    /// Declared output path → SHA-256 hex, when they were read.
    pub outputs: BTreeMap<String, String>,
    /// True when outputs were not read because the attempt did not prove an
    /// exit: a late writer must not be mistaken for a finished build.
    pub outputs_fenced: bool,
    /// True once the attempt's checkout, home and control file were deleted
    /// from the sprite. Only the project cache outlives an attempt.
    pub checkout_removed: bool,
    /// Why the attempt was closed without a receipt, when that happened.
    pub error: Option<String>,
}

impl AttemptRecord {
    /// What this attempt counts against the envelope: the reported spend, or
    /// the whole reserved cap while it has not reported. The envelope is a
    /// lifetime total for the project (no window or reset), and an unpriced
    /// attempt (`micros_per_second = 0`) keeps its whole cap committed.
    pub fn committed_micros(&self) -> u64 {
        self.spent_micros.unwrap_or(self.reserved_micros)
    }

    /// A terminal state means no further provider call is possible.
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self.state,
            AttemptState::Reserved | AttemptState::Dispatching | AttemptState::Running
        )
    }

    /// What a later submission of this attempt number may do.
    pub fn resume(&self, current_epoch: u64) -> Resume {
        if self.epoch != current_epoch {
            return Resume::StaleEpoch;
        }
        match self.state {
            AttemptState::Reserved => Resume::Reserved,
            AttemptState::Dispatching => Resume::UnknownDispatch,
            AttemptState::Running => Resume::Running,
            // A finished record without its exit is corrupt, never exit 1.
            AttemptState::Finished => self.exit.map_or(
                Resume::Terminal(SelectedCompletion::Indeterminate),
                |code| Resume::Terminal(SelectedCompletion::Exited(code)),
            ),
            AttemptState::Signalled => self.signal.map_or(
                Resume::Terminal(SelectedCompletion::Indeterminate),
                |signal| Resume::Terminal(SelectedCompletion::Signalled(signal)),
            ),
            AttemptState::TimedOut => Resume::Terminal(SelectedCompletion::TimedOut),
            AttemptState::Cancelled => Resume::Terminal(SelectedCompletion::Cancelled),
            AttemptState::Indeterminate => Resume::Terminal(SelectedCompletion::Indeterminate),
            AttemptState::Rejected => Resume::Rejected,
        }
    }
}

/// What the ledger says about an attempt number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// No record: the number is free.
    Fresh,
    /// The slot exists and nothing was sent: the caller may proceed.
    Reserved,
    /// A dispatch request may have reached the provider and no terminal state
    /// was observed. Never rerun implicitly.
    UnknownDispatch,
    /// A session is recorded: observe it, never resubmit.
    Running,
    /// The attempt reached a terminal state: replay it instead of rerunning.
    Terminal(SelectedCompletion),
    /// The attempt was closed before any command could run.
    Rejected,
    /// The record predates the current binding epoch.
    StaleEpoch,
}

/// Why a reservation was refused. Nothing was dispatched in either case.
#[derive(Debug)]
pub enum ReserveRefusal {
    /// The attempt number is already recorded.
    Duplicate(AttemptState),
    /// The project's spend envelope cannot cover one more attempt.
    EnvelopeExhausted {
        committed: u64,
        cap: u64,
        envelope: u64,
    },
    /// The sprite is already bound to another project.
    SpriteBound { sprite: String, project: String },
    /// The ledger could not be read or written.
    Store(anyhow::Error),
}

impl std::fmt::Display for ReserveRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(state) => {
                write!(f, "attempt is already recorded as {}", state_name(*state))
            }
            Self::EnvelopeExhausted {
                committed,
                cap,
                envelope,
            } => write!(
                f,
                "spend envelope exhausted ({committed} committed + {cap} cap > {envelope} micros)"
            ),
            Self::SpriteBound { sprite, project } => write!(
                f,
                "sprite {sprite:?} is already bound to project {project:?}; one project per sprite"
            ),
            Self::Store(error) => write!(f, "attempt ledger: {error:#}"),
        }
    }
}

fn state_name(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Reserved => "reserved",
        AttemptState::Dispatching => "dispatching",
        AttemptState::Running => "running",
        AttemptState::Finished => "finished",
        AttemptState::Signalled => "signalled",
        AttemptState::TimedOut => "timed-out",
        AttemptState::Cancelled => "cancelled",
        AttemptState::Indeterminate => "indeterminate",
        AttemptState::Rejected => "rejected",
    }
}

fn attempt_file(attempt: u64) -> String {
    format!("attempt-{attempt:010}.json")
}

fn attempt_from_file(name: &str) -> Option<u64> {
    name.strip_prefix("attempt-")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

/// The requested reservation of one attempt.
pub struct ReserveRequest<'a> {
    /// An explicit attempt number, or `None` to allocate the next free one
    /// under the ledger lock (two concurrent submits never pick the same).
    pub attempt: Option<u64>,
    pub recipe: Recipe,
    pub source: &'a Digest,
    /// Where per-attempt directories live on the sprite.
    pub checkout_root: &'a str,
    pub deadline_ms: i64,
    pub spend_cap_micros: u64,
    pub spend_envelope_micros: u64,
}

/// The durable attempt ledger for one project, as a private file store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptStore {
    dir: PathBuf,
}

impl AttemptStore {
    /// `<state home>/thegn/sprites-jobs/<project>`.
    pub fn for_project(project: &str) -> Self {
        Self {
            dir: thegn_core::util::xdg_state_home()
                .join("thegn")
                .join("sprites-jobs")
                .join(project),
        }
    }

    /// A ledger at an explicit directory (tests, or an operator-chosen root).
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Open the ledger root, optionally creating it. Absent is `None`; anything
    /// that is not an owned real directory is an error.
    fn open(&self, create: bool) -> Result<Option<PrivateDir>> {
        if create {
            ensure_private_dir(&self.dir)
                .context("sprites job ledger: not an owned private directory")?;
            return open_private_dir(&self.dir)
                .map(Some)
                .context("sprites job ledger: not an owned private directory");
        }
        let metadata = match std::fs::symlink_metadata(&self.dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("sprites job ledger: cannot stat the ledger"),
        };
        ensure!(
            !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && owned_by_current_user(&metadata),
            "sprites job ledger: {} is not an owned real directory",
            self.dir.display()
        );
        open_private_dir(&self.dir)
            .map(Some)
            .context("sprites job ledger: not an owned private directory")
    }

    fn read_record<T: DeserializeOwned>(&self, root: &PrivateDir, name: &str) -> Result<Option<T>> {
        let Some(bytes) = root
            .read_regular(name, RECORD_LIMIT)
            .with_context(|| format!("sprites job ledger: cannot read {name}"))?
        else {
            return Ok(None);
        };
        let record = serde_json::from_slice(&bytes)
            .with_context(|| format!("sprites job ledger: {name} is not a valid record"))?;
        Ok(Some(record))
    }

    /// The binding this ledger directory is pinned to.
    pub fn binding(&self) -> Result<Option<BindingRecord>> {
        let Some(root) = self.open(false)? else {
            return Ok(None);
        };
        self.read_record(&root, BINDING_FILE)
    }

    /// Every attempt record, oldest first. A record that cannot be read is an
    /// error, not a skip: an unreadable ledger must be repaired, never ignored.
    pub fn records(&self) -> Result<Vec<AttemptRecord>> {
        let Some(root) = self.open(false)? else {
            return Ok(Vec::new());
        };
        let mut records = Vec::new();
        for name in root
            .file_names()
            .context("sprites job ledger: cannot list")?
        {
            let Some(name) = name.to_str() else {
                continue;
            };
            if attempt_from_file(name).is_none() {
                continue;
            }
            if let Some(record) = self.read_record(&root, name)? {
                records.push(record);
            }
        }
        records.sort_by_key(|record| record.attempt);
        Ok(records)
    }

    pub fn read(&self, attempt: u64) -> Result<Option<AttemptRecord>> {
        let Some(root) = self.open(false)? else {
            return Ok(None);
        };
        self.read_record(&root, &attempt_file(attempt))
    }

    /// The next free attempt number. Attempt numbers are never reused.
    pub fn next_attempt(&self) -> Result<u64> {
        Ok(self
            .records()?
            .iter()
            .map(|record| record.attempt)
            .max()
            .unwrap_or(0)
            .saturating_add(1))
    }

    /// Publish a record atomically (tmp + fsync + rename).
    pub fn mark(&self, record: &AttemptRecord) -> Result<()> {
        let root = self
            .open(true)?
            .context("sprites job ledger: the ledger root vanished")?;
        let bytes = serde_json::to_vec_pretty(record).context("sprites job ledger: encode")?;
        root.write_owner_only_atomic(&attempt_file(record.attempt), &bytes)
            .with_context(|| {
                format!(
                    "sprites job ledger: cannot publish attempt {}",
                    record.attempt
                )
            })?;
        Ok(())
    }

    /// Confirm the owned command tree of a cancelled or indeterminate attempt is
    /// gone. Idempotent; a record that is not pending is left alone.
    pub fn mark_cleaned(&self, attempt: u64) -> Result<bool> {
        let Some(root) = self.open(false)? else {
            return Ok(false);
        };
        let Some(mut record) = self.read_record::<AttemptRecord>(&root, &attempt_file(attempt))?
        else {
            return Ok(false);
        };
        if record.cleanup != Cleanup::Pending {
            return Ok(false);
        }
        record.cleanup = Cleanup::Done;
        self.mark(&record)?;
        Ok(true)
    }

    /// What a later submission of `attempt` may do, against the current epoch.
    pub fn resume_attempt(&self, attempt: u64) -> Result<Resume> {
        let epoch = self.binding()?.map_or(1, |binding| binding.epoch.max(1));
        Ok(match self.read(attempt)? {
            None => Resume::Fresh,
            Some(record) => record.resume(epoch),
        })
    }

    /// The directory holding one claim file per sprite, beside the per-project
    /// ledgers, so two projects on this host cannot bind the same sprite.
    fn registry(&self) -> Result<PrivateDir> {
        let parent = self
            .dir
            .parent()
            .context("sprites job ledger: the ledger has no parent directory")?;
        let path = parent.join(SPRITE_REGISTRY);
        ensure_private_dir(&path)
            .context("sprites job ledger: cannot create the claim registry")?;
        open_private_dir(&path).context("sprites job ledger: cannot open the claim registry")
    }

    /// Claim `binding`'s sprite for its project, or refuse when another project
    /// already holds it. Exclusive-create makes the first claim atomic.
    fn claim_sprite(&self, binding: &ProjectBinding) -> Result<Option<String>> {
        let registry = self.registry()?;
        let name = format!("{}.json", binding.sprite());
        for _ in 0..3 {
            if let Some(bytes) = registry
                .read_regular(&name, RECORD_LIMIT)
                .context("sprites job ledger: cannot read the sprite claim")?
            {
                let claim: SpriteClaim = serde_json::from_slice(&bytes)
                    .context("sprites job ledger: the sprite claim is not valid")?;
                return Ok((claim.project != binding.project()).then_some(claim.project));
            }
            let bytes = serde_json::to_vec(&SpriteClaim {
                project: binding.project().to_owned(),
            })
            .context("sprites job ledger: encode")?;
            match registry.create_owner_only_exclusive(&name) {
                Ok(mut file) => {
                    file.write_all(&bytes)
                        .and_then(|()| file.sync_all())
                        .context("sprites job ledger: cannot write the sprite claim")?;
                    return Ok(None);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).context("sprites job ledger: cannot claim the sprite");
                }
            }
        }
        bail!("sprites job ledger: the sprite claim is contended; retry")
    }

    /// Drop this project's claim on a sprite it no longer uses (best effort: a
    /// stale claim only ever refuses, never admits).
    fn release_sprite(&self, project: &str, sprite: &str) {
        let Ok(registry) = self.registry() else {
            return;
        };
        let name = format!("{sprite}.json");
        let owned = matches!(
            registry.read_regular(&name, RECORD_LIMIT),
            Ok(Some(bytes)) if serde_json::from_slice::<SpriteClaim>(&bytes)
                .is_ok_and(|claim| claim.project == project)
        );
        if owned && let Err(error) = registry.remove_file(&name) {
            tracing::debug!(
                target: "thegn::sprites::job",
                %error,
                sprite,
                "cannot release the old sprite claim"
            );
        }
    }

    /// Count-and-reserve under the ledger lock. Everything that can refuse does
    /// so before any provider call.
    pub fn reserve(
        &self,
        binding: &ProjectBinding,
        request: ReserveRequest<'_>,
    ) -> Result<AttemptRecord, ReserveRefusal> {
        let root = store_error(self.open(true))?
            .ok_or_else(|| ReserveRefusal::Store(anyhow::anyhow!("the ledger root vanished")))?;
        let _lock = store_error(lock_ledger(&root))?;
        if let Some(project) = store_error(self.claim_sprite(binding))? {
            return Err(ReserveRefusal::SpriteBound {
                sprite: binding.sprite().to_owned(),
                project,
            });
        }
        let epoch = store_error(self.binding_epoch(&root, binding))?;
        let attempt = match request.attempt {
            Some(attempt) => attempt,
            None => store_error(self.max_attempt(&root))?.saturating_add(1),
        };
        if let Some(existing) =
            store_error(self.read_record::<AttemptRecord>(&root, &attempt_file(attempt)))?
        {
            return Err(ReserveRefusal::Duplicate(existing.state));
        }
        let committed = store_error(self.committed_micros(&root))?;
        if request.spend_envelope_micros > 0 {
            let after = committed.saturating_add(request.spend_cap_micros);
            if after > request.spend_envelope_micros {
                return Err(ReserveRefusal::EnvelopeExhausted {
                    committed,
                    cap: request.spend_cap_micros,
                    envelope: request.spend_envelope_micros,
                });
            }
        }
        let key = attempt_key(attempt, epoch);
        let record = AttemptRecord {
            provider: PROVIDER.to_owned(),
            project: binding.project().to_owned(),
            account: binding.account().to_owned(),
            sprite: binding.sprite().to_owned(),
            attempt,
            epoch,
            checkout: binding.checkout(&key, request.checkout_root),
            key,
            recipe: request.recipe,
            source: request.source.clone(),
            state: AttemptState::Reserved,
            session: None,
            hold: None,
            reserved_micros: request.spend_cap_micros,
            spent_micros: None,
            created_ms: thegn_core::util::now_ms(),
            deadline_ms: request.deadline_ms,
            finished_ms: None,
            exit: None,
            signal: None,
            cleanup: Cleanup::NotNeeded,
            materialized: None,
            outputs: BTreeMap::new(),
            outputs_fenced: false,
            checkout_removed: false,
            error: None,
        };
        store_error(self.mark(&record))?;
        Ok(record)
    }

    fn max_attempt(&self, root: &PrivateDir) -> Result<u64> {
        let mut max = 0u64;
        for name in root
            .file_names()
            .context("sprites job ledger: cannot list")?
        {
            if let Some(attempt) = name.to_str().and_then(attempt_from_file) {
                max = max.max(attempt);
            }
        }
        Ok(max)
    }

    /// The current epoch for `binding`, bumped when the project is rebound.
    fn binding_epoch(&self, root: &PrivateDir, binding: &ProjectBinding) -> Result<u64> {
        let intended = BindingRecord {
            project: binding.project().to_owned(),
            account: binding.account().to_owned(),
            sprite: binding.sprite().to_owned(),
            epoch: 1,
        };
        let Some(existing) = self.read_record::<BindingRecord>(root, BINDING_FILE)? else {
            self.write_binding(root, &intended)?;
            return Ok(1);
        };
        ensure!(
            existing.project == intended.project,
            "sprites job ledger: this directory is bound to project {:?}",
            existing.project
        );
        if existing.sprite == intended.sprite && existing.account == intended.account {
            return Ok(existing.epoch.max(1));
        }
        let rebound = BindingRecord {
            epoch: existing.epoch.saturating_add(1),
            ..intended
        };
        self.write_binding(root, &rebound)?;
        if existing.sprite != rebound.sprite {
            self.release_sprite(&existing.project, &existing.sprite);
        }
        Ok(rebound.epoch)
    }

    fn write_binding(&self, root: &PrivateDir, binding: &BindingRecord) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(binding).context("sprites job ledger: encode")?;
        root.write_owner_only_atomic(BINDING_FILE, &bytes)
            .context("sprites job ledger: cannot publish the binding")?;
        Ok(())
    }

    /// Spend already committed by this project's attempts.
    fn committed_micros(&self, root: &PrivateDir) -> Result<u64> {
        let mut committed = 0u64;
        for name in root
            .file_names()
            .context("sprites job ledger: cannot list")?
        {
            let Some(name) = name.to_str() else {
                continue;
            };
            if attempt_from_file(name).is_none() {
                continue;
            }
            if let Some(record) = self.read_record::<AttemptRecord>(root, name)? {
                committed = committed.saturating_add(record.committed_micros());
            }
        }
        Ok(committed)
    }
}

fn store_error<T>(result: Result<T>) -> Result<T, ReserveRefusal> {
    result.map_err(ReserveRefusal::Store)
}

/// Take the ledger's reservation lock without waiting: a second writer is a
/// retryable refusal, never an unbounded block on the dispatcher's thread.
fn lock_ledger(root: &PrivateDir) -> Result<std::fs::File> {
    let file = root
        .open_or_create_regular_nofollow(RESERVE_LOCK)
        .context("sprites job ledger: cannot open the ledger lock")?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("sprites job ledger: another attempt is reserving; retry")
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).context("sprites job ledger: cannot take the ledger lock")
        }
    }
}

/// Fixed execution parameters for one binding.
#[derive(Debug, Clone)]
pub struct AttemptOptions {
    /// The longest wall clock an attempt may run. The route's deadline is
    /// clamped to this: a binding never runs longer than it supports.
    pub deadline: Duration,
    /// Budget for moving the snapshot onto the sprite. It runs before the build
    /// clock starts, so transfer time never counts against the deadline.
    pub transfer_budget: Duration,
    /// Spend this attempt reserves, in micro-USD.
    pub spend_cap_micros: u64,
    /// Total the project may commit across attempts (0 = unlimited).
    pub spend_envelope_micros: u64,
    /// Configured compute price, in micro-USD per second (0 = unpriced).
    pub micros_per_second: u64,
    /// Keep-awake ceiling (0 = the attempt deadline plus one hold expiry).
    pub hold_ceiling_secs: u64,
    /// Declared outputs, relative to the checkout, hashed after a terminal exit.
    pub outputs: Vec<String>,
    /// Where per-attempt checkouts live on the sprite.
    pub checkout_root: String,
}

impl Default for AttemptOptions {
    fn default() -> Self {
        Self {
            deadline: DEFAULT_ATTEMPT_DEADLINE,
            transfer_budget: DEFAULT_TRANSFER_BUDGET,
            spend_cap_micros: DEFAULT_SPEND_CAP_MICROS,
            spend_envelope_micros: 0,
            micros_per_second: 0,
            hold_ceiling_secs: 0,
            outputs: Vec::new(),
            checkout_root: DEFAULT_CHECKOUT_ROOT.to_owned(),
        }
    }
}

impl AttemptOptions {
    fn validate(&self) -> Result<()> {
        ensure!(!self.deadline.is_zero(), "attempt deadline must be nonzero");
        ensure!(
            !self.transfer_budget.is_zero(),
            "transfer budget must be nonzero"
        );
        ensure!(
            self.checkout_root.starts_with('/') && !self.checkout_root.ends_with('/'),
            "checkout root must be an absolute path without a trailing slash"
        );
        ensure!(
            !self.checkout_root.contains(".."),
            "checkout root must not traverse"
        );
        for output in &self.outputs {
            checked_rel_path(output)?;
        }
        Ok(())
    }

    /// The keep-awake ceiling for an attempt that runs for `deadline`: explicit,
    /// else the deadline plus one expiry, so a quiet build never loses its hold
    /// before its own deadline.
    pub fn effective_hold_ceiling_secs(&self, deadline: Duration) -> u64 {
        if self.hold_ceiling_secs > 0 {
            self.hold_ceiling_secs
        } else {
            deadline.as_secs().saturating_add(HOLD_EXPIRE_SECS)
        }
    }
}

/// One attempt's outcome, including the accounting a report needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptReceipt {
    pub provider: String,
    pub project: String,
    pub sprite: String,
    pub attempt: u64,
    pub epoch: u64,
    pub checkout: String,
    pub source: Digest,
    pub materialized: Digest,
    pub session: Option<String>,
    pub hold: Option<String>,
    pub completion: SelectedCompletion,
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    pub cleanup: Cleanup,
    pub checkout_removed: bool,
    pub outputs: BTreeMap<String, String>,
    pub outputs_missing: Vec<String>,
    pub outputs_fenced: bool,
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub log_tail: String,
    /// Earlier output was discarded by this runner (a tail overflowed, or the
    /// composed log was trimmed to the contract bound).
    pub truncated: bool,
    pub elapsed_ms: u64,
    pub cost_micros: Option<u64>,
    pub cost_basis: String,
}

/// A bounded byte tail: the last `cap` bytes of a stream plus the total seen.
#[derive(Debug)]
struct Tail {
    bytes: std::collections::VecDeque<u8>,
    cap: usize,
    total: u64,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Self {
            bytes: std::collections::VecDeque::new(),
            cap,
            total: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        if chunk.len() >= self.cap {
            self.bytes.clear();
            self.bytes
                .extend(chunk[chunk.len() - self.cap..].iter().copied());
            return;
        }
        let excess = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(self.cap);
        for _ in 0..excess {
            self.bytes.pop_front();
        }
        self.bytes.extend(chunk.iter().copied());
    }

    fn render(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<u8>>()).into_owned()
    }

    fn total(&self) -> u64 {
        self.total
    }

    /// True when earlier bytes fell off the front.
    fn truncated(&self) -> bool {
        self.total > self.bytes.len() as u64
    }
}

/// Trim a log tail to the contract's bound from the front, on a char boundary.
/// The flag says whether anything was trimmed.
fn bound_log(mut log: String) -> (String, bool) {
    if log.len() <= MAX_SELECTED_LOG_BYTES {
        return (log, false);
    }
    let mut start = log.len() - MAX_SELECTED_LOG_BYTES;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    (log.split_off(start), true)
}

/// The keep-awake task name for one attempt: `thegn-hold-<16 hex>`, derived
/// from the binding and the attempt key (attempt, epoch and a nonce), so a ledger
/// reset or rebind never reuses the name of an older, possibly still running
/// attempt's hold.
fn hold_task(project: &str, sprite: &str, key: &str) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(project.as_bytes());
    hasher.update([0]);
    hasher.update(sprite.as_bytes());
    hasher.update([0]);
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();
    hold_name(&hex(&digest.as_slice()[..8]))
}

/// The provider's session handle, announced asynchronously on connect.
/// Bounded: a server that never announces one leaves it `None` rather than
/// stalling the attempt.
async fn session_handle(
    session_id: &mut tokio::sync::watch::Receiver<Option<String>>,
) -> Option<String> {
    let until = tokio::time::Instant::now() + SESSION_HANDLE_WAIT;
    loop {
        let announced = session_id.borrow().as_deref().map(str::to_owned);
        if let Some(handle) = announced {
            return Some(handle);
        }
        match tokio::time::timeout_at(until, session_id.changed()).await {
            Ok(Ok(())) => {}
            // The wait elapsed, or the driver is gone: record no handle.
            Ok(Err(_)) | Err(_) => return None,
        }
    }
}

/// The launcher the recipe runs behind (inside the keep-awake hold). Positional
/// arguments: the control file, the checkout, the per-attempt `HOME`, the
/// project cache, then the recipe argv.
///
/// It records its own pid in the control file, then `exec`s the recipe under
/// `setsid env -i`: the recipe leads a fresh process group whose id is the
/// recorded pid (so the whole tree can be killed by group), and it sees only a
/// scrubbed environment with a per-attempt `HOME` and a project-scoped
/// `CARGO_HOME`/`CARGO_TARGET_DIR`. Any failure to get that far prints
/// [`LAUNCH_FAILED_MARKER`] on stderr and exits 125, so a launcher failure is
/// never mistaken for a recipe that exits 125.
pub const LAUNCH_SCRIPT: &str = r#"fail() { echo "thegn-job-launch-failed: $1" >&2; exit 125; }
pidfile=$1
cwd=$2
home=$3
cache=$4
shift 4
rustup_home=${RUSTUP_HOME:-${HOME:-}/.rustup}
command -v setsid >/dev/null 2>&1 || fail setsid
mkdir -p -- "${pidfile%/*}" "$home/tmp" "$cache/cargo" "$cache/target" || fail dirs
cd -- "$cwd" || fail cwd
echo $$ >"$pidfile" || fail pidfile
exec setsid env -i PATH="$PATH" HOME="$home" TMPDIR="$home/tmp" CARGO_HOME="$cache/cargo" \
    CARGO_TARGET_DIR="$cache/target" RUSTUP_HOME="$rustup_home" LANG=C.UTF-8 "$@"
"#;

/// Kills the owned process group recorded in the control file and releases the
/// keep-awake hold. Positional arguments: the control file and the hold task.
/// Exit 0 means the group is confirmed gone; 3 means it survived SIGKILL; 4
/// means there is no usable control file, so nothing can be confirmed. It only
/// ever signals the recorded group, never a process it did not start.
pub const KILL_SCRIPT: &str = r#"pidfile=$1
hold=$2
K=/bin/kill
[ -x "$K" ] || K=kill
pid=$(cat "$pidfile" 2>/dev/null)
case $pid in '' | *[!0-9]*) exit 4 ;; esac
[ "$pid" -gt 1 ] || exit 4
if "$K" -0 -- "-$pid" 2>/dev/null; then
    "$K" -s TERM -- "-$pid" 2>/dev/null
    n=0
    while [ "$n" -lt 10 ] && "$K" -0 -- "-$pid" 2>/dev/null; do
        sleep 1
        n=$((n + 1))
    done
    if "$K" -0 -- "-$pid" 2>/dev/null; then
        "$K" -s KILL -- "-$pid" 2>/dev/null
        sleep 1
        if "$K" -0 -- "-$pid" 2>/dev/null; then exit 3; fi
    fi
fi
case $hold in '' | *[!a-z0-9-]*) ;; *)
    curl -fsS --max-time 10 --unix-socket "${THEGN_HOLD_SOCK:-/.sprite/api.sock}" -X DELETE \
        "http://sprite/v1/tasks/$hold" >/dev/null 2>&1
    ;;
esac
exit 0
"#;

/// `Recipe::argv` behind the launcher, wrapped in the hold, as the sprite
/// receives it.
fn attempt_argv(
    recipe: Recipe,
    hold: &str,
    ceiling_secs: u64,
    control_file: &str,
    checkout: &str,
    home: &str,
    cache: &str,
) -> Vec<String> {
    let mut launcher: Vec<String> = [
        "/bin/sh",
        "-c",
        LAUNCH_SCRIPT,
        "thegn-job",
        control_file,
        checkout,
        home,
        cache,
    ]
    .iter()
    .map(|part| (*part).to_owned())
    .collect();
    launcher.extend(recipe.argv().iter().map(|arg| (*arg).to_owned()));
    wrap_argv(&launcher, hold, ceiling_secs)
}

/// The kill exec's argv.
fn kill_argv(control_file: &str, hold: &str) -> Vec<String> {
    [
        "/bin/sh",
        "-c",
        KILL_SCRIPT,
        "thegn-job-kill",
        control_file,
        hold,
    ]
    .iter()
    .map(|part| (*part).to_owned())
    .collect()
}

/// Exit codes `128 + N` (N in 1..=31) are how a POSIX shell's `wait` reports a
/// signalled child, and the Sprites exec API reports only an exit code. The
/// runner cannot tell a signal from a recipe that genuinely exits with such a
/// code, so it reports the conventional reading.
fn signal_of(code: i32) -> Option<i32> {
    (129..=159).contains(&code).then_some(code - 128)
}

/// Sprites-side executor for the typed selected-runner route.
///
/// One runner is bound to one project and one sprite. It holds a runtime handle
/// rather than a runtime so the [`SelectedRunner`] contract stays synchronous:
/// `submit` is called from the dispatcher's own thread and enters the runtime
/// itself. Calling it *from* a runtime worker would panic inside
/// [`tokio::runtime::Handle::block_on`], and the handle must belong to a
/// multi-threaded runtime (or one driven elsewhere); the async entry point
/// [`run`](Self::run) exists for in-runtime callers.
pub struct SpritesSelectedRunner {
    provider: SpritesProvider,
    binding: ProjectBinding,
    store: AttemptStore,
    snapshot: Box<dyn SnapshotSource>,
    handle: tokio::runtime::Handle,
    options: AttemptOptions,
    alias: String,
}

impl SpritesSelectedRunner {
    /// Bind a provider to a project and its attempt ledger.
    pub fn new(
        provider: SpritesProvider,
        binding: ProjectBinding,
        snapshot: Box<dyn SnapshotSource>,
        handle: tokio::runtime::Handle,
    ) -> Result<Self> {
        let store = AttemptStore::for_project(binding.project());
        Self::with_options(
            provider,
            binding,
            store,
            snapshot,
            handle,
            AttemptOptions::default(),
        )
    }

    /// [`new`](Self::new) with an explicit ledger directory and options.
    pub fn with_options(
        provider: SpritesProvider,
        binding: ProjectBinding,
        store: AttemptStore,
        snapshot: Box<dyn SnapshotSource>,
        handle: tokio::runtime::Handle,
        options: AttemptOptions,
    ) -> Result<Self> {
        options.validate()?;
        let alias = binding.alias();
        Ok(Self {
            provider,
            binding,
            store,
            snapshot,
            handle,
            options,
            alias,
        })
    }

    /// The job-spec alias this runner answers to.
    pub fn alias(&self) -> &str {
        &self.alias
    }

    /// The durable ledger, readable after the process is gone.
    pub fn store(&self) -> &AttemptStore {
        &self.store
    }

    /// Publish a record on a blocking thread (the ledger does synchronous file
    /// I/O with fsync and rename).
    async fn persist(&self, record: &AttemptRecord) -> Result<()> {
        let store = self.store.clone();
        let record = record.clone();
        tokio::task::spawn_blocking(move || store.mark(&record))
            .await
            .map_err(|error| anyhow!("ledger task failed: {error}"))?
    }

    async fn reserve(
        &self,
        recipe: Recipe,
        source: &Digest,
        deadline: Duration,
    ) -> Result<AttemptRecord, ReserveRefusal> {
        let store = self.store.clone();
        let binding = self.binding.clone();
        let source = source.clone();
        let root = self.options.checkout_root.clone();
        let deadline_ms = thegn_core::util::now_ms().saturating_add(millis_i64(deadline));
        let spend_cap_micros = self.options.spend_cap_micros;
        let spend_envelope_micros = self.options.spend_envelope_micros;
        tokio::task::spawn_blocking(move || {
            store.reserve(
                &binding,
                ReserveRequest {
                    attempt: None,
                    recipe,
                    source: &source,
                    checkout_root: &root,
                    deadline_ms,
                    spend_cap_micros,
                    spend_envelope_micros,
                },
            )
        })
        .await
        .map_err(|error| ReserveRefusal::Store(anyhow!("ledger task failed: {error}")))?
    }

    /// Run one attempt now, for at most `deadline` (clamped to the binding's
    /// supported maximum, [`AttemptOptions::deadline`]).
    ///
    /// An error here means the attempt was refused or closed *before any command
    /// could have run* (an unreadable ledger, a sprite bound to another project,
    /// a transfer that did not verify, a definitive 4xx on the exec upgrade).
    /// Note the selected route reads every error from
    /// [`SelectedRunner::submit`] back as indeterminate regardless: it cannot
    /// know what the runner did, so the proof of "never ran" lives in this
    /// module's ledger (state `rejected`), not in the readback. Once dispatch
    /// began, an unproven outcome is a receipt with
    /// [`SelectedCompletion::Indeterminate`] and cleanup pending, never an error
    /// a caller might retry as though nothing had run. A launcher failure (the
    /// checkout directory could not be entered, no `setsid`) is likewise
    /// `Indeterminate`.
    pub async fn run(
        &self,
        recipe: Recipe,
        source: &Digest,
        content: &SnapshotContent,
        deadline: Duration,
    ) -> Result<AttemptReceipt, RunnerRefusal> {
        let observed = content
            .digest()
            .map_err(|error| self.unavailable(format!("cannot digest the snapshot: {error:#}")))?;
        if &observed != source {
            return Err(RunnerRefusal::SourceMismatch {
                expected: source.clone(),
                observed,
            });
        }
        if content.is_empty() {
            return Err(RunnerRefusal::InvalidSpec(
                "a selected attempt needs at least one snapshot file: an empty checkout has no \
                 directory to run in"
                    .to_owned(),
            ));
        }
        let deadline = deadline.min(self.options.deadline);
        if deadline.is_zero() {
            return Err(RunnerRefusal::InvalidSpec(
                "a selected attempt needs a nonzero deadline".to_owned(),
            ));
        }
        let begun = tokio::time::Instant::now();
        let mut record = self
            .reserve(recipe, source, deadline)
            .await
            .map_err(|refusal| self.unavailable(refusal))?;

        // One project per sprite, verified on the sprite itself before anything
        // is written there.
        if let Err(error) = self.claim_remote().await {
            return Err(self.reject(record, error).await);
        }

        let transferred = tokio::time::timeout(
            self.options.transfer_budget,
            self.materialize(&record.checkout, content),
        )
        .await;
        let transfer = match transferred {
            Ok(Ok(transfer)) => transfer,
            Ok(Err(error)) => return Err(self.reject(record, error).await),
            Err(_) => {
                let error = anyhow!(
                    "the transfer exceeded its {}s budget",
                    self.options.transfer_budget.as_secs()
                );
                return Err(self.reject(record, error).await);
            }
        };
        if &transfer.digest != source {
            let error = anyhow!(
                "the materialized checkout digest {} does not match the pinned source {}",
                transfer.digest.hash(),
                source.hash()
            );
            return Err(self.reject(record, error).await);
        }
        record.materialized = Some(transfer.digest.clone());
        let Some(hold) = hold_task(&record.project, &record.sprite, &record.key) else {
            let error = anyhow!("cannot derive a hold task name");
            return Err(self.reject(record, error).await);
        };
        record.hold = Some(hold.clone());

        // The fence. Written before the socket opens: a crash from here on reads
        // back as an unknown dispatch and is never rerun blindly.
        record.state = AttemptState::Dispatching;
        if let Err(error) = self.persist(&record).await {
            return Err(self.reject(record, error).await);
        }

        let root = &self.options.checkout_root;
        let spec = ExecSpec {
            argv: attempt_argv(
                recipe,
                &hold,
                self.options.effective_hold_ceiling_secs(deadline),
                &self.binding.control_file(&record.key, root),
                &record.checkout,
                &self.binding.home(&record.key, root),
                &self.binding.cache(root),
            ),
            tty: false,
            cols: 0,
            rows: 0,
            // The launcher builds the recipe's environment (`env -i`); nothing
            // from the sprite's own environment rides the exec request.
            env: Vec::new(),
            cwd: None,
        };
        // The build clock starts at dispatch, after the transfer.
        let started = tokio::time::Instant::now();
        // Exactly one upgrade request: the command starts from it, so a retried
        // upgrade could run the build twice.
        let streaming = match self
            .provider
            .open_exec_streaming_once(self.binding.sprite(), &spec)
            .await
        {
            Ok(streaming) => streaming,
            Err(error) if is_definite_exec_rejection(&error) => {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    %error,
                    attempt = record.attempt,
                    "the provider rejected the exec upgrade; no command ran"
                );
                return Err(self.reject(record, error).await);
            }
            Err(error) => {
                // A timeout or 5xx can follow an accepted upgrade, so the
                // command may be running. Leave the outcome unknown, try to
                // kill whatever was started, and never redispatch.
                tracing::warn!(
                    target: "thegn::sprites::job",
                    %error,
                    attempt = record.attempt,
                    "exec dispatch outcome is unknown; the attempt is indeterminate"
                );
                record.error = Some(format!("exec dispatch outcome unknown: {error:#}"));
                let empty = Tail::new(1);
                return Ok(self
                    .conclude(
                        record,
                        transfer,
                        begun,
                        SelectedCompletion::Indeterminate,
                        false,
                        &empty,
                        None,
                    )
                    .await);
            }
        };
        let mut session = streaming.session;
        let stderr = streaming.stderr;
        let verdict = streaming.verdict;
        record.session = session_handle(&mut session.session_id).await;
        record.state = AttemptState::Running;
        if let Err(error) = self.persist(&record).await {
            // A failed re-mark leaves the earlier `dispatching` fence, which is
            // already enough to forbid an implicit rerun.
            tracing::warn!(
                target: "thegn::sprites::job",
                %error,
                attempt = record.attempt,
                "cannot record the exec session handle"
            );
        }

        let until = started + deadline;
        let mut stdout = Tail::new(MAX_SELECTED_LOG_BYTES);
        let mut exit = None;
        let mut timed_out = false;
        loop {
            match tokio::time::timeout_at(until, session.frames.recv()).await {
                Ok(Some(ExecFrame::Stdout(bytes))) => stdout.push(&bytes),
                Ok(Some(ExecFrame::Exit(code))) => {
                    exit = Some(code);
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
        }
        // Close our end of the socket. This alone does not stop the command
        // (Sprites exec sessions are detachable); the kill exec in `conclude`
        // does.
        if let Err(error) = session.control.send(ExecControl::Close).await {
            tracing::debug!(
                target: "thegn::sprites::job",
                %error,
                attempt = record.attempt,
                "exec session was already gone"
            );
        }
        tracing::debug!(
            target: "thegn::sprites::job",
            attempt = record.attempt,
            stdout_bytes = stdout.total(),
            "attempt output stream closed"
        );
        let launch_failed = !timed_out
            && exit == Some(crate::provider::EXEC_CWD_FAILED_STATUS)
            && stderr.render().contains(LAUNCH_FAILED_MARKER);
        let completion = match (timed_out, exit) {
            (true, _) => SelectedCompletion::TimedOut,
            (false, Some(_)) if launch_failed => {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    attempt = record.attempt,
                    checkout = %record.checkout,
                    "the launcher could not start the recipe"
                );
                SelectedCompletion::Indeterminate
            }
            (false, Some(code)) => match signal_of(code) {
                Some(signal) => SelectedCompletion::Signalled(signal),
                None => SelectedCompletion::Exited(code),
            },
            (false, None) => SelectedCompletion::Indeterminate,
        };
        if timed_out {
            tracing::warn!(
                target: "thegn::sprites::job",
                attempt = record.attempt,
                deadline_secs = deadline.as_secs(),
                "attempt deadline reached; killing the owned command tree"
            );
        }
        // A stream that ended without a terminal exit is worth explaining: the
        // driver's verdict distinguishes a protocol violation from a plain
        // close, and lands in the record either way. The wait is bounded so a
        // missing verdict never stalls the attempt.
        if exit.is_none() && !timed_out {
            record.error = match tokio::time::timeout(VERDICT_WAIT, verdict).await {
                Ok(Ok(Some(error))) => Some(error.to_string()),
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => None,
            };
        }
        Ok(self
            .conclude(
                record,
                transfer,
                begun,
                completion,
                launch_failed,
                &stdout,
                Some(&stderr),
            )
            .await)
    }

    /// Close a reservation that provably never ran a command: nothing was
    /// dispatched (or the provider definitively rejected the dispatch), so no
    /// kill is needed and no spend is committed. Best effort: the partial
    /// checkout is removed, and cleanup stays pending if that fails.
    async fn reject(&self, mut record: AttemptRecord, error: anyhow::Error) -> RunnerRefusal {
        let removed = self.remove_attempt_dirs(&record).await;
        record.state = AttemptState::Rejected;
        record.cleanup = if removed {
            Cleanup::NotNeeded
        } else {
            Cleanup::Pending
        };
        record.checkout_removed = removed;
        record.spent_micros = Some(0);
        record.outputs_fenced = true;
        record.finished_ms = Some(thegn_core::util::now_ms());
        record.error = Some(format!("{error:#}"));
        if let Err(write_error) = self.persist(&record).await {
            tracing::warn!(
                target: "thegn::sprites::job",
                %write_error,
                attempt = record.attempt,
                "cannot record the rejected attempt"
            );
        }
        self.unavailable(error)
    }

    /// Verify (or establish) on the sprite that it serves only this project.
    /// Fails closed: anything but a clean "absent" or a matching marker refuses.
    async fn claim_remote(&self) -> Result<()> {
        let marker = ProjectBinding::owner_marker(&self.options.checkout_root);
        let sprite = self.binding.sprite();
        match self.provider.read(sprite, &marker).await {
            Ok(bytes) => {
                ensure!(
                    bytes == self.binding.project().as_bytes(),
                    "sprite {sprite:?} is already bound to another project; one project per sprite"
                );
                Ok(())
            }
            Err(error) if format!("{error:#}").contains("404") => self
                .provider
                .write(sprite, &marker, self.binding.project().as_bytes())
                .await
                .with_context(|| format!("sprites: cannot write the owner marker {marker}")),
            Err(error) => Err(error)
                .with_context(|| format!("sprites: cannot read the owner marker {marker}")),
        }
    }

    /// Write the authorized bytes into the attempt checkout, read every file
    /// back and hash it, list the checkout, and return the REAPI root digest of
    /// what the sprite holds. Files go through bounded parallel writes; each
    /// read-back is hashed and dropped, so memory holds the snapshot once.
    async fn materialize(&self, checkout: &str, content: &SnapshotContent) -> Result<Transfer> {
        let sprite = self.binding.sprite();
        let provider = &self.provider;
        let verified: Vec<(&str, Digest, bool)> = stream::iter(content.files())
            .map(|(path, wanted)| async move {
                let remote = join_remote(checkout, path);
                let executable = content.is_executable(path);
                let expected = Digest::of(wanted);
                let written = if executable {
                    provider.write_exec(sprite, &remote, wanted).await
                } else {
                    provider.write(sprite, &remote, wanted).await
                };
                written.with_context(|| format!("sprites: cannot write {remote}"))?;
                let read_back = provider
                    .read(sprite, &remote)
                    .await
                    .with_context(|| format!("sprites: cannot verify {remote}"))?;
                let observed = Digest::of(&read_back);
                ensure!(
                    observed == expected,
                    "sprites: {remote} did not round-trip ({} bytes written, {} bytes read)",
                    wanted.len(),
                    read_back.len()
                );
                Ok::<_, anyhow::Error>((path, observed, executable))
            })
            .buffer_unordered(TRANSFER_CONCURRENCY)
            .try_collect()
            .await?;
        self.verify_listing(checkout, content).await?;
        let transferred = verified.len();
        let digest = root_digest(verified)?;
        Ok(Transfer {
            digest,
            transferred,
            bytes: content.total_bytes(),
        })
    }

    /// The checkout must hold exactly the snapshot's entries: a stale or extra
    /// file (for example a deletion that did not apply) is refused.
    async fn verify_listing(&self, checkout: &str, content: &SnapshotContent) -> Result<()> {
        let mut expected: BTreeMap<String, BTreeSet<(String, bool)>> = BTreeMap::new();
        for (path, _) in content.files() {
            let mut dir = String::new();
            let mut parts = path.split('/').peekable();
            while let Some(part) = parts.next() {
                let is_dir = parts.peek().is_some();
                expected
                    .entry(dir.clone())
                    .or_default()
                    .insert((part.to_owned(), is_dir));
                if is_dir {
                    if !dir.is_empty() {
                        dir.push('/');
                    }
                    dir.push_str(part);
                }
            }
        }
        for (dir, wanted) in &expected {
            let remote = join_remote(checkout, dir);
            let listed = self
                .provider
                .list_dir(self.binding.sprite(), &remote)
                .await
                .with_context(|| format!("sprites: cannot list {remote}"))?;
            let held: BTreeSet<(String, bool)> = listed
                .into_iter()
                .filter(|entry| entry.name != "." && entry.name != "..")
                .map(|entry| (entry.name, entry.is_dir))
                .collect();
            ensure!(
                &held == wanted,
                "sprites: the checkout directory {remote} does not hold exactly the snapshot's \
                 entries"
            );
        }
        Ok(())
    }

    /// Read and hash the declared outputs. Only ever called for an attempt that
    /// proved a terminal exit: a late writer must not be mistaken for a build.
    async fn hash_outputs(&self, checkout: &str) -> (BTreeMap<String, String>, Vec<String>) {
        let mut outputs = BTreeMap::new();
        let mut missing = Vec::new();
        for path in &self.options.outputs {
            let remote = join_remote(checkout, path);
            match self.provider.read(self.binding.sprite(), &remote).await {
                Ok(bytes) => {
                    outputs.insert(path.clone(), hex(Sha256::digest(&bytes)));
                }
                Err(_) => missing.push(path.clone()),
            }
        }
        (outputs, missing)
    }

    /// Kill the attempt's owned process group and release its keep-awake hold,
    /// through one idempotent exec. True only when the kill is confirmed.
    async fn kill_tree(&self, record: &AttemptRecord) -> bool {
        let Some(hold) = record.hold.as_deref() else {
            return false;
        };
        let control = self
            .binding
            .control_file(&record.key, &self.options.checkout_root);
        let argv = kill_argv(&control, hold);
        match self
            .provider
            .run_exec_within(self.binding.sprite(), &argv, None, &[], CLEANUP_BUDGET)
            .await
        {
            Ok((0, _)) => true,
            Ok((code, output)) => {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    attempt = record.attempt,
                    code,
                    output = %output.trim(),
                    "the owned command tree is not confirmed gone"
                );
                false
            }
            Err(error) => {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    %error,
                    attempt = record.attempt,
                    "cannot run the kill exec; cleanup stays pending"
                );
                false
            }
        }
    }

    /// Delete the attempt's checkout, home and control file. The project cache
    /// is never touched. True when every deletion succeeded.
    async fn remove_attempt_dirs(&self, record: &AttemptRecord) -> bool {
        let root = &self.options.checkout_root;
        let paths = [
            record.checkout.clone(),
            self.binding.home(&record.key, root),
            self.binding.control_file(&record.key, root),
        ];
        let mut removed = true;
        for path in &paths {
            if let Err(error) = self.provider.delete(self.binding.sprite(), path).await {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    %error,
                    attempt = record.attempt,
                    path = %path,
                    "cannot delete a per-attempt path"
                );
                removed = false;
            }
        }
        removed
    }

    /// Record the terminal state, kill what may still run, hash what may be
    /// hashed, delete the per-attempt directories, and build the receipt. Every
    /// exit from [`run`](Self::run) after the dispatch fence goes through here,
    /// so no path can forget the ledger.
    #[allow(clippy::too_many_arguments)]
    async fn conclude(
        &self,
        mut record: AttemptRecord,
        transfer: Transfer,
        begun: tokio::time::Instant,
        completion: SelectedCompletion,
        launch_failed: bool,
        stdout: &Tail,
        stderr: Option<&ExecStderr>,
    ) -> AttemptReceipt {
        let proven = matches!(
            completion,
            SelectedCompletion::Exited(_) | SelectedCompletion::Signalled(_)
        );
        // Anything short of a proven exit may still be running. Stop it and
        // release the hold now rather than letting it bill until the ceiling.
        record.cleanup = if proven || launch_failed {
            Cleanup::NotNeeded
        } else if self.kill_tree(&record).await {
            Cleanup::Done
        } else {
            Cleanup::Pending
        };
        let (cost_micros, cost_basis) = self.cost(begun.elapsed());
        record.state = match completion {
            SelectedCompletion::Exited(_) => AttemptState::Finished,
            SelectedCompletion::Signalled(_) => AttemptState::Signalled,
            SelectedCompletion::TimedOut => AttemptState::TimedOut,
            SelectedCompletion::Cancelled => AttemptState::Cancelled,
            SelectedCompletion::Indeterminate => AttemptState::Indeterminate,
        };
        record.exit = match completion {
            SelectedCompletion::Exited(code) => Some(code),
            _ => None,
        };
        record.signal = match completion {
            SelectedCompletion::Signalled(signal) => Some(signal),
            _ => None,
        };
        record.finished_ms = Some(thegn_core::util::now_ms());
        record.spent_micros = cost_micros;
        record.outputs_fenced = !matches!(completion, SelectedCompletion::Exited(_));
        let (outputs, outputs_missing) = if record.outputs_fenced {
            (BTreeMap::new(), Vec::new())
        } else {
            self.hash_outputs(&record.checkout).await
        };
        record.outputs = outputs.clone();
        // Keep the directories (and the control file) while the tree may still
        // be running: a reconciler needs them. Otherwise only the cache stays.
        record.checkout_removed = if record.cleanup == Cleanup::Pending {
            false
        } else {
            self.remove_attempt_dirs(&record).await
        };
        if let Err(error) = self.persist(&record).await {
            tracing::warn!(
                target: "thegn::sprites::job",
                %error,
                attempt = record.attempt,
                "cannot record the attempt outcome"
            );
        }
        let stdout_tail = stdout.render();
        let stderr_tail = stderr.map(ExecStderr::render).unwrap_or_default();
        let (composed, stderr_truncated) = compose_log(&stdout_tail, stderr, record.exit);
        let (log_tail, trimmed) = bound_log(composed);
        AttemptReceipt {
            provider: PROVIDER.to_owned(),
            project: record.project.clone(),
            sprite: record.sprite.clone(),
            attempt: record.attempt,
            epoch: record.epoch,
            checkout: record.checkout.clone(),
            source: record.source.clone(),
            materialized: transfer.digest,
            session: record.session.clone(),
            hold: record.hold.clone(),
            completion,
            exit: record.exit,
            signal: record.signal,
            cleanup: record.cleanup,
            checkout_removed: record.checkout_removed,
            outputs,
            outputs_missing,
            outputs_fenced: record.outputs_fenced,
            files_transferred: transfer.transferred,
            bytes_transferred: transfer.bytes,
            truncated: stdout.truncated() || stderr_truncated || trimmed,
            log_tail,
            stdout_tail,
            stderr_tail,
            elapsed_ms: u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX),
            cost_micros,
            cost_basis,
        }
    }

    /// The per-attempt estimate: elapsed time (transfer included) at the
    /// configured rate, or an explicitly unlabeled (`None`) cost when no rate is
    /// configured.
    fn cost(&self, elapsed: Duration) -> (Option<u64>, String) {
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if self.options.micros_per_second == 0 {
            return (None, format!("elapsed {elapsed_ms}ms (no configured rate)"));
        }
        let micros = elapsed_ms.saturating_mul(self.options.micros_per_second) / 1000;
        (
            Some(micros),
            format!(
                "elapsed {elapsed_ms}ms x {} micros/s (configured estimate)",
                self.options.micros_per_second
            ),
        )
    }

    fn unavailable(&self, error: impl std::fmt::Display) -> RunnerRefusal {
        // `{:#}` keeps the cause chain: a refusal is the operator's only clue
        // about which call the sprite rejected and why.
        RunnerRefusal::RunnerUnavailable(format!("{}: {error:#}", self.alias))
    }
}

fn millis_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl SelectedRunner for SpritesSelectedRunner {
    fn submit(
        &self,
        runner_alias: &str,
        recipe: Recipe,
        source: &Digest,
        deadline: Duration,
    ) -> Result<SelectedJobResult, RunnerRefusal> {
        if runner_alias != self.alias {
            return Err(RunnerRefusal::WrongRunner);
        }
        let content = self
            .snapshot
            .capture()
            .map_err(|error| self.unavailable(format!("cannot capture the snapshot: {error:#}")))?;
        let receipt = self
            .handle
            .block_on(self.run(recipe, source, &content, deadline))?;
        Ok(SelectedJobResult {
            runner: self.alias.clone(),
            recipe,
            source: source.clone(),
            completion: receipt.completion,
            log_tail: receipt.log_tail,
            truncated: receipt.truncated,
        })
    }
}

/// What one materialization moved.
struct Transfer {
    digest: Digest,
    transferred: usize,
    bytes: u64,
}

/// The bounded log tail: stdout, then the stderr tail for a non-zero exit — the
/// same shape the provider's one-shot exec reports. The flag says the included
/// stderr tail already dropped earlier bytes.
fn compose_log(stdout: &str, stderr: Option<&ExecStderr>, exit: Option<i32>) -> (String, bool) {
    let mut log = stdout.to_owned();
    if exit == Some(0) {
        return (log, false);
    }
    let Some(stderr) = stderr else {
        return (log, false);
    };
    let seen = stderr.total().min(STDERR_TAIL_CAP as u64);
    if seen == 0 {
        return (log, false);
    }
    log.push_str(&format!(
        "\n[stderr, last {seen} bytes]\n{}",
        stderr.render()
    ));
    (log, stderr.total() > STDERR_TAIL_CAP as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins a tuple's slice type at the call, so literals of different lengths
    /// can share one array.
    fn f<'a>(path: &'a str, bytes: &'a [u8], executable: bool) -> (&'a str, &'a [u8], bool) {
        (path, bytes, executable)
    }

    fn files(entries: &[(&str, &[u8], bool)]) -> SnapshotContent {
        let mut content = SnapshotContent::new();
        for (path, bytes, executable) in entries {
            content
                .insert_file(path, bytes.to_vec(), *executable)
                .unwrap();
        }
        content
    }

    #[test]
    fn the_snapshot_digest_is_the_reapi_root_directory_digest() {
        // Built by hand with `thegn_cas`, never through this module's own tree
        // walk: a real pinned snapshot digest is exactly this.
        let leaf = |bytes: &[u8], executable| Entry::File {
            digest: Digest::of(bytes),
            executable,
        };
        let src =
            encode_directory(vec![("main.rs".to_owned(), leaf(b"fn main() {}\n", false))]).unwrap();
        let root = encode_directory(vec![
            ("src".to_owned(), Entry::Directory(Digest::of(&src))),
            ("build.sh".to_owned(), leaf(b"#!/bin/sh\n", true)),
            ("Cargo.toml".to_owned(), leaf(b"[package]\n", false)),
        ])
        .unwrap();
        let content = files(&[
            f("Cargo.toml", b"[package]\n", false),
            f("src/main.rs", b"fn main() {}\n", false),
            f("build.sh", b"#!/bin/sh\n", true),
        ]);
        assert_eq!(content.digest().unwrap(), Digest::of(&root));
    }

    #[test]
    fn the_digest_is_order_independent_and_sensitive_to_bytes_paths_and_mode() {
        let one = files(&[f("b.txt", b"two", false), f("a.txt", b"one", false)]);
        let two = files(&[f("a.txt", b"one", false), f("b.txt", b"two", false)]);
        assert_eq!(one.digest().unwrap(), two.digest().unwrap());
        let edited = files(&[f("a.txt", b"one!", false), f("b.txt", b"two", false)]);
        assert_ne!(one.digest().unwrap(), edited.digest().unwrap());
        let executable = files(&[f("a.txt", b"one", true), f("b.txt", b"two", false)]);
        assert_ne!(one.digest().unwrap(), executable.digest().unwrap());
        let split = files(&[f("a", b"bc", false)]);
        let joined = files(&[f("ab", b"c", false)]);
        assert_ne!(split.digest().unwrap(), joined.digest().unwrap());
    }

    #[test]
    fn a_path_cannot_be_both_a_file_and_a_directory() {
        let mut content = SnapshotContent::new();
        content.insert("a/b", b"x".to_vec()).unwrap();
        assert!(content.insert("a", b"y".to_vec()).is_err());
        let mut other = SnapshotContent::new();
        other.insert("a", b"y".to_vec()).unwrap();
        assert!(other.insert("a/b", b"x".to_vec()).is_err());
        // A path that merely shares a prefix is fine.
        other.insert("a.txt", b"z".to_vec()).unwrap();
        other.insert("a-b/c", b"z".to_vec()).unwrap();
    }

    #[test]
    fn credential_and_vcs_paths_are_denied_at_every_depth() {
        for denied in [
            ".git/config",
            ".thegn/state.json",
            ".ssh/id_ed25519",
            ".aws/credentials",
            ".gnupg/secring",
            ".claude/settings.json",
            ".codex/auth.json",
            ".kube/config",
            ".docker/config.json",
            ".env",
            ".env.local",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".git-credentials",
            "services/api/.env",
            "web/.npmrc",
            "web/.env.production",
            "pkg/.claude/settings.json",
            "vendor/sub/.git/HEAD",
            "tools/id_rsa",
            "ci/.cargo/credentials.toml",
            "home/.config/gh/hosts.yml",
        ] {
            assert!(denied_path(denied), "{denied} must be denied");
            assert!(
                checked_rel_path(denied).is_err(),
                "{denied} must be refused"
            );
        }
        assert!(!denied_path(".environment"));
        assert!(!denied_path("src/environment.rs"));
        assert!(!denied_path(".cargo/config.toml"));
        checked_rel_path("src/main.rs").unwrap();
        checked_rel_path("a/b/c.txt").unwrap();
        assert!(checked_rel_path("../escape").is_err());
        assert!(checked_rel_path("/abs").is_err());
        assert!(checked_rel_path("a//b").is_err());
        assert!(checked_rel_path("a/./b").is_err());
        assert!(checked_rel_path("a\\b").is_err());
        assert!(checked_rel_path("a:b").is_err());
    }

    #[test]
    fn tail_keeps_the_last_cap_bytes_and_the_total() {
        let mut tail = Tail::new(8);
        tail.push(b"12345");
        assert!(!tail.truncated());
        tail.push(b"6789");
        assert_eq!(tail.total(), 9);
        assert_eq!(tail.render(), "23456789");
        assert!(tail.truncated());
        tail.push(b"0123456789abc");
        assert_eq!(tail.render(), "56789abc");
        assert_eq!(tail.total(), 22);
    }

    #[test]
    fn log_tail_is_bounded_from_the_front_and_says_so() {
        let long = "x".repeat(MAX_SELECTED_LOG_BYTES + 100);
        let (bounded, trimmed) = bound_log(long);
        assert_eq!(bounded.len(), MAX_SELECTED_LOG_BYTES);
        assert!(trimmed);
        assert_eq!(bound_log("short".to_owned()), ("short".to_owned(), false));
    }

    #[test]
    fn hold_task_names_are_shared_shape_and_unique_per_attempt_key() {
        let name = hold_task("proj", "sprite-one", "2-1-aaaaaaaa").unwrap();
        assert_eq!(name.len(), "thegn-hold-".len() + 16);
        assert!(name.starts_with("thegn-hold-"));
        assert_eq!(
            Some(name.clone()),
            hold_task("proj", "sprite-one", "2-1-aaaaaaaa")
        );
        // A ledger reset reuses attempt 2, but the nonce keeps the hold distinct.
        assert_ne!(
            Some(name.clone()),
            hold_task("proj", "sprite-one", "2-1-bbbbbbbb")
        );
        assert_ne!(Some(name), hold_task("proj", "sprite-one", "3-1-aaaaaaaa"));
    }

    #[test]
    fn attempt_argv_runs_only_the_recipe_behind_the_launcher_and_the_hold() {
        let argv = attempt_argv(
            Recipe::CargoTest,
            "thegn-hold-0123456789abcdef",
            900,
            "/r/p/ctl/k.pid",
            "/r/p/co/k",
            "/r/p/home/k",
            "/r/p/cache",
        );
        assert_eq!(argv.len(), 16);
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "-c");
        assert_eq!(argv[2], thegn_core::sprite_hold::HOLD_SCRIPT);
        assert_eq!(argv[3], "thegn-hold");
        assert_eq!(argv[4], "thegn-hold-0123456789abcdef");
        assert_eq!(argv[5], "900");
        assert_eq!(argv[6], "/bin/sh");
        assert_eq!(argv[8], LAUNCH_SCRIPT);
        assert_eq!(argv[9], "thegn-job");
        assert_eq!(
            &argv[10..14],
            ["/r/p/ctl/k.pid", "/r/p/co/k", "/r/p/home/k", "/r/p/cache"]
        );
        let recipe: Vec<&str> = argv[14..].iter().map(String::as_str).collect();
        assert_eq!(recipe, vec!["cargo", "test"]);
    }

    #[test]
    fn the_launcher_scrubs_the_environment_and_marks_its_own_failures() {
        assert!(LAUNCH_SCRIPT.contains("env -i"));
        assert!(LAUNCH_SCRIPT.contains("setsid"));
        assert!(LAUNCH_SCRIPT.contains(LAUNCH_FAILED_MARKER));
        assert!(!LAUNCH_SCRIPT.contains("$HOME/.claude"));
    }

    #[test]
    fn signal_exits_are_recognized_by_the_shell_convention() {
        assert_eq!(signal_of(137), Some(9));
        assert_eq!(signal_of(143), Some(15));
        assert_eq!(signal_of(129), Some(1));
        assert_eq!(signal_of(101), None);
        assert_eq!(signal_of(1), None);
        assert_eq!(signal_of(160), None);
    }

    #[test]
    fn aliases_reject_uppercase_and_edge_dashes() {
        assert!(ProjectBinding::new("proj", "acct", "sprite-one").is_ok());
        assert_eq!(
            ProjectBinding::new("proj", "acct", "sprite-one")
                .unwrap()
                .alias(),
            "sprites-sprite-one"
        );
        assert!(ProjectBinding::new("proj", "acct", "Sprite").is_err());
        assert!(ProjectBinding::new("proj", "acct", "-sprite").is_err());
        assert!(ProjectBinding::new("../proj", "acct", "sprite").is_err());
        assert!(ProjectBinding::new("proj", "acct", "a/b").is_err());
    }

    #[test]
    fn checkouts_homes_and_caches_are_project_scoped_and_unique_per_attempt() {
        let one = ProjectBinding::new("proj-one", "acct", "sprite-a").unwrap();
        let two = ProjectBinding::new("proj-two", "acct", "sprite-a").unwrap();
        let first = one.checkout("1-1-aaaa", DEFAULT_CHECKOUT_ROOT);
        assert_eq!(first, "/workspace/.thegn/jobs/proj-one/co/1-1-aaaa");
        assert_ne!(first, two.checkout("1-1-aaaa", DEFAULT_CHECKOUT_ROOT));
        // Same attempt number after a ledger reset: a new nonce, a new tree.
        assert_ne!(first, one.checkout("1-1-bbbb", DEFAULT_CHECKOUT_ROOT));
        assert_eq!(
            one.home("1-1-aaaa", DEFAULT_CHECKOUT_ROOT),
            "/workspace/.thegn/jobs/proj-one/home/1-1-aaaa"
        );
        assert_eq!(
            one.control_file("1-1-aaaa", DEFAULT_CHECKOUT_ROOT),
            "/workspace/.thegn/jobs/proj-one/ctl/1-1-aaaa.pid"
        );
        assert_ne!(
            one.cache(DEFAULT_CHECKOUT_ROOT),
            two.cache(DEFAULT_CHECKOUT_ROOT)
        );
        assert_eq!(
            one.cache(DEFAULT_CHECKOUT_ROOT),
            "/workspace/.thegn/jobs/proj-one/cache"
        );
        assert!(!one.cache(DEFAULT_CHECKOUT_ROOT).starts_with(first.as_str()));
        assert_eq!(
            ProjectBinding::owner_marker(DEFAULT_CHECKOUT_ROOT),
            "/workspace/.thegn/jobs/.owner"
        );
    }

    #[test]
    fn the_hold_ceiling_follows_the_attempt_deadline_unless_it_is_pinned() {
        let options = AttemptOptions::default();
        let deadline = Duration::from_secs(60);
        assert_eq!(
            options.effective_hold_ceiling_secs(deadline),
            60 + HOLD_EXPIRE_SECS
        );
        let pinned = AttemptOptions {
            hold_ceiling_secs: 900,
            ..AttemptOptions::default()
        };
        assert_eq!(pinned.effective_hold_ceiling_secs(deadline), 900);
    }

    #[test]
    fn a_finished_record_without_an_exit_is_indeterminate_never_exit_one() {
        let source = Digest::of(b"x");
        let mut record = AttemptRecord {
            provider: PROVIDER.to_owned(),
            project: "p".to_owned(),
            account: "a".to_owned(),
            sprite: "s".to_owned(),
            attempt: 1,
            epoch: 1,
            key: "1-1-aaaa".to_owned(),
            recipe: Recipe::CargoBuild,
            source,
            checkout: "/c".to_owned(),
            state: AttemptState::Finished,
            session: None,
            hold: None,
            reserved_micros: 0,
            spent_micros: None,
            created_ms: 0,
            deadline_ms: 0,
            finished_ms: None,
            exit: None,
            signal: None,
            cleanup: Cleanup::NotNeeded,
            materialized: None,
            outputs: BTreeMap::new(),
            outputs_fenced: false,
            checkout_removed: false,
            error: None,
        };
        assert_eq!(
            record.resume(1),
            Resume::Terminal(SelectedCompletion::Indeterminate)
        );
        record.exit = Some(3);
        assert_eq!(
            record.resume(1),
            Resume::Terminal(SelectedCompletion::Exited(3))
        );
        record.state = AttemptState::Signalled;
        record.signal = Some(9);
        assert_eq!(
            record.resume(1),
            Resume::Terminal(SelectedCompletion::Signalled(9))
        );
        record.state = AttemptState::TimedOut;
        assert_eq!(
            record.resume(1),
            Resume::Terminal(SelectedCompletion::TimedOut)
        );
        record.state = AttemptState::Rejected;
        assert_eq!(record.resume(1), Resume::Rejected);
        assert_eq!(record.resume(2), Resume::StaleEpoch);
    }
}
