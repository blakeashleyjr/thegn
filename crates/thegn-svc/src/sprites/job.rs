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
//!    confirmed, otherwise `Pending`. A [`CancelToken`] stops the attempt the
//!    same way ([`SpritesSelectedRunner::run_cancellable`]) and reports
//!    [`SelectedCompletion::Cancelled`], which is never confused with a timeout.
//! 5. **report** — provider identity, session handle, exit, signal, cleanup
//!    state and the per-attempt cost estimate land in the ledger and in the
//!    receipt, so a client that lost its channel can reconcile by attempt
//!    number. Once cleanup is not pending the per-attempt checkout, home and
//!    control file are deleted; only the project cache remains.
//!
//! Recovery: a crash can leave an attempt non-terminal or with cleanup pending.
//! [`SpritesSelectedRunner::reconcile_pending`] re-issues the idempotent kill
//! for the recorded control file and hold and marks cleanup `Done` only on a
//! confirmed kill; it never dispatches, so a command whose dispatch outcome is
//! unknown is never rerun as an implicit recovery. [`SpritesSelectedRunner::reattach`]
//! reconnects to a recorded exec session when the provider still holds it, and
//! otherwise settles the attempt as `Indeterminate`. Neither touches a
//! non-terminal row younger than its staleness age, and every recovery write is
//! a compare-and-swap against the row it read.
//!
//! Owner lease: the running attempt keeps an [`OwnerLease`] (pid, boot id,
//! process start time, heartbeat) in its non-terminal row and refreshes the
//! heartbeat every 15 s by compare-and-swap. Recovery skips a row whose lease
//! is live (fresh heartbeat, or on Linux the same process still alive in the
//! same boot) whatever its age, so a host suspended past the staleness margin
//! no longer makes a live run look stale. A heartbeat that loses its
//! compare-and-swap means a recovery settled the row: the owner fences, stops
//! trusting its own state and never overwrites the row. Rows without a lease
//! keep the age-only rule, and terminal rows carry none.
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
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    Recipe, RunnerRefusal, SelectedCancel, SelectedCompletion, SelectedJobResult, SelectedRunner,
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

/// The marker the launcher sets (to `1`) in the recipe's scrubbed environment so
/// a `thegn job run` started inside a selected job refuses to recurse. Both
/// executors' launch scripts set it literally.
pub const SELECTED_JOB_ENV: &str = "THEGN_SELECTED_JOB";

/// Directory names a snapshot never carries, at any depth: version control,
/// thegn's own state, and agent/provider/cloud credentials. A directory is
/// pruned with its whole subtree.
const DENIED_DIRS: [&str; 10] = [
    ".git", ".thegn", ".ssh", ".aws", ".gnupg", ".claude", ".codex", ".kube", ".docker", ".direnv",
];

/// File names a snapshot never carries, at any depth.
const DENIED_NAMES: [&str; 8] = [
    ".env",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".pgpass",
    ".dockercfg",
    ".git-credentials",
    "credentials.json",
];

/// SSH private keys by pattern: `id_rsa`, `id_ed25519_sk`, `id_ecdsa`, `id_dsa`
/// and any other `id_*` file (case-insensitive) except a `.pub` public key.
fn is_ssh_key_name(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    lower.starts_with("id_") && !lower.ends_with(".pub")
}

/// Secret-shaped file names by pattern, case-insensitive: private keys and
/// certificates, direnv files, Terraform variables and state, and service
/// account key files.
fn is_secret_shaped_name(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    [".pem", ".key", ".p12", ".pfx", ".tfvars"]
        .iter()
        .any(|suffix| lower.ends_with(suffix))
        || lower.starts_with(".envrc")
        || lower.contains(".tfstate")
        || (lower.starts_with("service-account") && lower.ends_with(".json"))
}

/// True when `rel` is content a job snapshot must never carry. Every component
/// is checked, so `services/api/.env` and `pkg/.claude/x` are denied exactly
/// like their root-level forms.
fn denied_path(rel: &str) -> bool {
    let parts: Vec<&str> = rel.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        if DENIED_DIRS.contains(part)
            || DENIED_NAMES.contains(part)
            || is_ssh_key_name(part)
            || is_secret_shaped_name(part)
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
pub(crate) fn checked_rel_path(path: &str) -> Result<()> {
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

pub(crate) fn hex(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
            write!(hex, "{byte:02x}").expect("writing to String cannot fail");
            hex
        })
}

/// A short unique token for names that must not collide across attempts, ledger
/// resets or hosts: wall clock, process id and a process-local counter.
pub(crate) fn nonce() -> String {
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
/// `.ignore`) and from the user's global gitignore, so a file the user keeps
/// out of version control only through their global excludes is never
/// uploaded. The pin and the executor capture on the same host with this same
/// walker, so their digests still match. Secret-shaped names (keys,
/// certificates, `.envrc`, `*.tfvars`, ...) are denied regardless.
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
            .git_global(true)
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
pub(crate) fn checked_component(value: &str, what: &str, max: usize) -> Result<()> {
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
pub(crate) fn alias_is_valid(alias: &str) -> bool {
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

/// How often a live owner refreshes its lease heartbeat. Recovery treats a
/// heartbeat younger than twice this as proof of a live owner.
pub(crate) const LEASE_HEARTBEAT: Duration = Duration::from_secs(15);

/// How far ahead of the reader's clock a heartbeat may sit and still count as
/// fresh.
const LEASE_CLOCK_SKEW_MS: i64 = 5_000;

/// The ownership lease a running attempt keeps in its non-terminal row, shared
/// by the Sprites and Vercel executors.
///
/// Recovery treats a row as owned and live, and skips it whatever its age, when
/// either the heartbeat is fresh or (Linux only) the recorded process still
/// exists in the same boot with the same start time. A host suspended past the
/// staleness margin therefore no longer makes a live run look stale: after the
/// resume the pid is still there. A row with no lease (an older record) keeps
/// the age-only staleness rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OwnerLease {
    /// The owning process.
    pub pid: u32,
    /// `/proc/sys/kernel/random/boot_id` at the time the lease was taken; `None`
    /// where the platform has none (pid checks are then skipped).
    pub boot_id: Option<String>,
    /// The owner's process start time (`/proc/<pid>/stat` field 22), so a
    /// recycled pid is not mistaken for the owner.
    pub start_ticks: Option<u64>,
    pub started_at_ms: i64,
    /// Refreshed every `LEASE_HEARTBEAT` by the owner, by compare-and-swap.
    pub heartbeat_ms: i64,
}

/// What `/proc/<pid>/stat` says about a process.
struct ProcStat {
    zombie: bool,
    start_ticks: u64,
}

/// Parse `/proc/<pid>/stat`; `None` off Linux or when the process is gone. The
/// `comm` field may hold spaces and parentheses, so fields are read after the
/// last `)`.
fn proc_stat(pid: u32) -> Option<ProcStat> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?;
    // Field 3 was the state; field 22 (start time) is the 19th after it.
    let start_ticks = fields.nth(18)?.parse().ok()?;
    Some(ProcStat {
        zombie: state.starts_with('Z'),
        start_ticks,
    })
}

/// This boot's id (Linux), read once; `None` elsewhere.
pub(crate) fn boot_id() -> Option<String> {
    static BOOT_ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    BOOT_ID
        .get_or_init(|| {
            if !cfg!(target_os = "linux") {
                return None;
            }
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        })
        .clone()
}

impl OwnerLease {
    /// A lease for this process, heartbeat now.
    pub fn current(now_ms: i64) -> Self {
        let pid = std::process::id();
        Self {
            pid,
            boot_id: boot_id(),
            start_ticks: proc_stat(pid).map(|stat| stat.start_ticks),
            started_at_ms: now_ms,
            heartbeat_ms: now_ms,
        }
    }

    /// The same lease with its heartbeat moved to `now_ms`.
    #[must_use]
    pub fn refreshed(&self, now_ms: i64) -> Self {
        Self {
            heartbeat_ms: now_ms,
            ..self.clone()
        }
    }

    /// True while the owner can still be driving the attempt: a fresh heartbeat,
    /// or the same process (same boot, same start time, not a zombie) is alive.
    pub fn is_live(&self, now_ms: i64) -> bool {
        let window = millis_i64(LEASE_HEARTBEAT * 2);
        // A heartbeat a little in the future is clock skew; one far in the future
        // is a wrong clock or a corrupt row and proves nothing.
        let age = now_ms.saturating_sub(self.heartbeat_ms);
        if (-LEASE_CLOCK_SKEW_MS..=window).contains(&age) {
            return true;
        }
        let Some(recorded) = self.boot_id.as_deref() else {
            return false;
        };
        if boot_id().as_deref() != Some(recorded) {
            return false;
        }
        proc_stat(self.pid).is_some_and(|stat| {
            !stat.zombie
                && self
                    .start_ticks
                    .is_none_or(|ticks| ticks == stat.start_ticks)
        })
    }
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
    /// The running owner's lease; absent on rows written before leases existed
    /// and on rows no run has started yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<OwnerLease>,
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

pub(crate) fn state_name(state: AttemptState) -> &'static str {
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

pub(crate) fn attempt_file(attempt: u64) -> String {
    format!("attempt-{attempt:010}.json")
}

pub(crate) fn attempt_from_file(name: &str) -> Option<u64> {
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
        let mut records: Vec<AttemptRecord> = Vec::new();
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

    /// Move the owner's heartbeat to `now_ms` by compare-and-swap against
    /// `record`, under the ledger lock: the stored row must be non-terminal and
    /// carry this run's lease; only the lease is rewritten. False means a
    /// recovery took the row and nothing was written: the owner must stop
    /// trusting its own state. `record.lease` is updated only on success.
    pub fn refresh_lease(&self, record: &mut AttemptRecord, now_ms: i64) -> Result<bool> {
        let Some(lease) = record.lease.clone() else {
            return Ok(true);
        };
        let Some(root) = self.open(false)? else {
            return Ok(false);
        };
        let _lock = lock_ledger(&root)?;
        let Some(mut current) =
            self.read_record::<AttemptRecord>(&root, &attempt_file(record.attempt))?
        else {
            return Ok(false);
        };
        // Ownership, not equality: the run changes other fields locally between
        // persists. Recovery always clears or replaces the lease when it settles
        // a row, so a changed lease or a terminal row means the run lost it.
        if current.is_terminal() || current.lease.as_ref() != Some(&lease) {
            return Ok(false);
        }
        let refreshed = lease.refreshed(now_ms);
        current.lease = Some(refreshed.clone());
        self.mark(&current)?;
        record.lease = Some(refreshed);
        Ok(true)
    }

    /// Publish `record` only when the stored row still equals `expected` (the
    /// row as a recovery pass read it). The check and the write happen under the
    /// ledger lock, so a recovery never overwrites what the owning run wrote in
    /// between. False when the row changed or vanished; nothing is written.
    pub fn mark_if_unchanged(
        &self,
        expected: &AttemptRecord,
        record: &AttemptRecord,
    ) -> Result<bool> {
        let Some(root) = self.open(false)? else {
            return Ok(false);
        };
        let _lock = lock_ledger(&root)?;
        let current = self.read_record::<AttemptRecord>(&root, &attempt_file(expected.attempt))?;
        if current.as_ref() != Some(expected) {
            return Ok(false);
        }
        self.mark(record)?;
        Ok(true)
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
            // Publish the complete claim in one step (temp file, then a
            // no-replace link), so a crash or a concurrent reader never sees
            // an empty claim.
            let temp = format!("{name}.{}.claim", nonce());
            registry
                .write_owner_only_atomic(&temp, &bytes)
                .context("sprites job ledger: cannot write the sprite claim")?;
            // A hard link is the portable no-replace publish.
            let published =
                std::fs::hard_link(registry.path().join(&temp), registry.path().join(&name));
            if let Err(error) = registry.remove_file(&temp) {
                tracing::debug!(%error, "sprites job ledger: temp claim cleanup failed");
            }
            match published {
                Ok(()) => return Ok(None),
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
            lease: None,
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
    /// How often the running owner refreshes its lease heartbeat
    /// (`LEASE_HEARTBEAT` by default); injectable so a test can use a short one.
    pub lease_heartbeat: Duration,
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
            lease_heartbeat: LEASE_HEARTBEAT,
        }
    }
}

impl AttemptOptions {
    fn validate(&self) -> Result<()> {
        ensure!(!self.deadline.is_zero(), "attempt deadline must be nonzero");
        ensure!(
            !self.lease_heartbeat.is_zero(),
            "lease heartbeat must be nonzero"
        );
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
pub(crate) struct Tail {
    bytes: std::collections::VecDeque<u8>,
    cap: usize,
    total: u64,
}

impl Tail {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            bytes: std::collections::VecDeque::new(),
            cap,
            total: 0,
        }
    }

    pub(crate) fn push(&mut self, chunk: &[u8]) {
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

    pub(crate) fn render(&self) -> String {
        String::from_utf8_lossy(&self.bytes.iter().copied().collect::<Vec<u8>>()).into_owned()
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// True when earlier bytes fell off the front.
    pub(crate) fn truncated(&self) -> bool {
        self.total > self.bytes.len() as u64
    }
}

/// Trim a log tail to the contract's bound from the front, on a char boundary.
/// The flag says whether anything was trimmed.
pub(crate) fn bound_log(mut log: String) -> (String, bool) {
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
/// It `exec`s a fresh `setsid` shell that records its own pid and start time in
/// the control file (so the file never exists before the group does) and then
/// `exec`s the recipe under `env -i`, keeping only proxy and CA variables from
/// the caller's environment: the recipe leads a fresh process group whose id is the
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
[ -n "${PATH:-}" ] || fail path
mkdir -p -- "${pidfile%/*}" "$home/tmp" "$cache/cargo" "$cache/target" || fail dirs
cd -- "$cwd" || fail cwd
for v in HTTP_PROXY HTTPS_PROXY NO_PROXY ALL_PROXY http_proxy https_proxy no_proxy all_proxy \
    SSL_CERT_FILE SSL_CERT_DIR NIX_SSL_CERT_FILE; do
    eval "val=\${$v:-}"
    [ -z "$val" ] || set -- "$v=$val" "$@"
done
set -- PATH="$PATH" HOME="$home" TMPDIR="$home/tmp" CARGO_HOME="$cache/cargo" \
    CARGO_TARGET_DIR="$cache/target" RUSTUP_HOME="$rustup_home" LANG=C.UTF-8 \
    THEGN_SELECTED_JOB=1 "$@"
exec setsid sh -c 'pidfile=$1; shift
st=
getst() { set -- $1; shift 19; st=$1; }
if { read -r l </proc/$$/stat; } 2>/dev/null; then getst "${l##*) }"; fi
echo "$$ $st" >"$pidfile" || { echo "thegn-job-launch-failed: pidfile" >&2; exit 125; }
exec env -i "$@"' thegn-job-run "$pidfile" "$@"
"#;

/// Kills the owned process group recorded in the control file and releases the
/// keep-awake hold. Positional arguments: the control file and the hold task.
/// Exit 0 means the group is confirmed gone; 3 means it survived SIGKILL; 4
/// means there is no usable control file, so nothing can be confirmed. It only
/// ever signals the recorded group, never a process it did not start: the
/// control file holds `<pid> <start time>` (written by the recipe's own
/// process after `setsid`), and when `/proc` shows that pid with a different
/// start time the pid was reused and the group is treated as gone. A group
/// whose remaining members are all zombies also counts as gone.
pub const KILL_SCRIPT: &str = r#"pidfile=$1
hold=$2
K=/bin/kill
[ -x "$K" ] || K=kill
read -r pid start <"$pidfile" 2>/dev/null
case $pid in '' | *[!0-9]*) exit 4 ;; esac
[ "$pid" -gt 1 ] || exit 4
start_of() {
    { read -r l <"/proc/$1/stat"; } 2>/dev/null || return 1
    r=${l##*) }
    set -- $r
    shift 19
    echo "$1"
}
group_alive() {
    "$K" -0 -- "-$pid" 2>/dev/null || return 1
    [ -r /proc/self/stat ] || return 0
    for s in /proc/[0-9]*/stat; do
        { read -r l <"$s"; } 2>/dev/null || continue
        r=${l##*) }
        set -- $r
        if [ "$3" = "$pid" ] && [ "$1" != Z ]; then return 0; fi
    done
    return 1
}
reused=0
if [ -n "$start" ] && now=$(start_of "$pid") && [ "$now" != "$start" ]; then reused=1; fi
if [ "$reused" = 0 ] && group_alive; then
    "$K" -s TERM -- "-$pid" 2>/dev/null
    n=0
    while [ "$n" -lt 10 ] && group_alive; do
        sleep 1
        n=$((n + 1))
    done
    if group_alive; then
        "$K" -s KILL -- "-$pid" 2>/dev/null
        sleep 1
        if group_alive; then exit 3; fi
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
pub(crate) fn signal_of(code: i32) -> Option<i32> {
    (129..=159).contains(&code).then_some(code - 128)
}

/// A user cancel handle for one running attempt. Clones share the flag; cancel
/// is idempotent and may be called before, during or after the attempt.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    inner: Arc<CancelInner>,
}

#[derive(Debug, Default)]
struct CancelInner {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation of the attempt this token was given to.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        // `notify_one` stores a permit, so a cancel that lands before the
        // runner starts waiting is not lost.
        self.inner.notify.notify_one();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Resolves once [`cancel`](Self::cancel) was called.
    pub(crate) async fn cancelled(&self) {
        while !self.is_cancelled() {
            self.inner.notify.notified().await;
        }
    }
}

/// What one reconciliation pass did to one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileOutcome {
    /// The owned tree is confirmed gone and cleanup is now `Done`.
    Cleaned,
    /// The kill could not be confirmed; cleanup stays `Pending` for a later pass.
    StillPending,
    /// The row changed since the pass read it (its owner or another recovery
    /// wrote it), so this pass left it alone.
    Superseded,
}

/// One attempt a reconciliation pass touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reconciled {
    pub attempt: u64,
    pub outcome: ReconcileOutcome,
}

/// The result of [`SpritesSelectedRunner::reattach`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReattachReport {
    pub attempt: u64,
    pub completion: SelectedCompletion,
    pub cleanup: Cleanup,
    /// True when the provider still held the exec session and its stream was
    /// followed to the end; false when the ledger (or a reconciliation) answered.
    pub reattached: bool,
    /// The settled receipt when this call observed or concluded the attempt.
    pub receipt: Option<AttemptReceipt>,
    /// Why this call left the row untouched (a live owner may hold it, or the
    /// provider answered transiently), when it did.
    pub note: Option<String>,
}

/// The smallest time a reattached stream may take to replay its scrollback and
/// deliver the exit, even when the recorded deadline already passed.
const REATTACH_MIN_WAIT: Duration = Duration::from_secs(5);

/// Records younger than their deadline plus this are left to the process that
/// may still own them: transfer budget, kill budget and slack.
const RECONCILE_SLACK: Duration = Duration::from_secs(60);

/// Extra margin on the staleness test. The ledger deadline is wall-clock, but
/// a live owner times its stream on the monotonic clock, which stops while the
/// host is suspended; the margin only absorbs small skew. A suspend longer than
/// it can still make a live row look stale (the owner lease in the row closes
/// that gap: a live lease is never stale).
const RECONCILE_CLOCK_MARGIN: Duration = Duration::from_secs(120);

/// The `Instant` an attempt's build clock is read from when it is concluded
/// later, capped at its deadline window so downtime is never billed.
fn begun_for(record: &AttemptRecord) -> tokio::time::Instant {
    let window = u64::try_from(record.deadline_ms.saturating_sub(record.created_ms)).unwrap_or(0);
    let since =
        u64::try_from(thegn_core::util::now_ms().saturating_sub(record.created_ms)).unwrap_or(0);
    let ago = Duration::from_millis(window.min(since));
    tokio::time::Instant::now()
        .checked_sub(ago)
        .unwrap_or_else(tokio::time::Instant::now)
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

    /// [`persist`](Self::persist) guarded by [`AttemptStore::mark_if_unchanged`].
    async fn persist_if_unchanged(
        &self,
        expected: &AttemptRecord,
        record: &AttemptRecord,
    ) -> Result<bool> {
        let store = self.store.clone();
        let expected = expected.clone();
        let record = record.clone();
        tokio::task::spawn_blocking(move || store.mark_if_unchanged(&expected, &record))
            .await
            .map_err(|error| anyhow!("ledger task failed: {error}"))?
    }

    /// True while the stored row still equals what a recovery read. Checked
    /// right before a kill, so a row its owner moved on is never killed.
    async fn unchanged(&self, expected: &AttemptRecord) -> bool {
        let store = self.store.clone();
        let attempt = expected.attempt;
        let current = tokio::task::spawn_blocking(move || store.read(attempt)).await;
        matches!(current, Ok(Ok(Some(ref record))) if record == expected)
    }

    /// The age past which a non-terminal row can no longer belong to a live run:
    /// deadline plus transfer budget, kill budget, slack and clock margin.
    fn stale_after_ms(&self) -> i64 {
        millis_i64(
            self.options.transfer_budget
                + CLEANUP_BUDGET
                + RECONCILE_SLACK
                + RECONCILE_CLOCK_MARGIN,
        )
    }

    /// False for a row whose owner lease is live, whatever its age; otherwise
    /// the age-only rule (rows without a lease keep it).
    fn is_stale(&self, record: &AttemptRecord, now_ms: i64) -> bool {
        if record
            .lease
            .as_ref()
            .is_some_and(|lease| lease.is_live(now_ms))
        {
            return false;
        }
        now_ms >= record.deadline_ms.saturating_add(self.stale_after_ms())
    }

    /// Refresh the owner's heartbeat by compare-and-swap against the row this
    /// run last wrote. False means the stored row is no longer ours (a recovery
    /// settled it): the owner must stop trusting its own state. A failed write
    /// is not a loss; the next tick retries.
    async fn refresh_lease(&self, record: &mut AttemptRecord) -> bool {
        let store = self.store.clone();
        let mut held = record.clone();
        let now_ms = thegn_core::util::now_ms();
        let result = tokio::task::spawn_blocking(move || {
            let ours = store.refresh_lease(&mut held, now_ms)?;
            anyhow::Ok((ours, held))
        })
        .await
        .map_err(|error| anyhow!("ledger task failed: {error}"))
        .and_then(|inner| inner);
        match result {
            Ok((true, held)) => {
                *record = held;
                true
            }
            Ok((false, _)) => false,
            Err(error) => {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    %error,
                    attempt = record.attempt,
                    "cannot refresh the owner lease heartbeat"
                );
                true
            }
        }
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
        self.run_cancellable(recipe, source, content, deadline, &CancelToken::new())
            .await
    }

    /// [`run`](Self::run) with a user cancel handle. Cancelling the token while
    /// the command runs issues the same owned-tree kill as the deadline,
    /// releases the hold, fences outputs and reports
    /// [`SelectedCompletion::Cancelled`]; cleanup is `Done` on a confirmed kill
    /// and `Pending` otherwise. A token cancelled before dispatch closes the
    /// attempt as rejected: nothing was sent, so no command ran.
    pub async fn run_cancellable(
        &self,
        recipe: Recipe,
        source: &Digest,
        content: &SnapshotContent,
        deadline: Duration,
        cancel: &CancelToken,
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
            return Err(self.reject(record, error, false).await);
        }

        let transferred = tokio::time::timeout(
            self.options.transfer_budget,
            self.materialize(&record.checkout, content),
        )
        .await;
        let transfer = match transferred {
            Ok(Ok(transfer)) => transfer,
            Ok(Err(error)) => return Err(self.reject(record, error, true).await),
            Err(_) => {
                let error = anyhow!(
                    "the transfer exceeded its {}s budget",
                    self.options.transfer_budget.as_secs()
                );
                return Err(self.reject(record, error, true).await);
            }
        };
        if &transfer.digest != source {
            let error = anyhow!(
                "the materialized checkout digest {} does not match the pinned source {}",
                transfer.digest.hash(),
                source.hash()
            );
            return Err(self.reject(record, error, true).await);
        }
        record.materialized = Some(transfer.digest.clone());
        let Some(hold) = hold_task(&record.project, &record.sprite, &record.key) else {
            let error = anyhow!("cannot derive a hold task name");
            return Err(self.reject(record, error, true).await);
        };
        record.hold = Some(hold.clone());

        if cancel.is_cancelled() {
            let error = anyhow!("cancelled before dispatch");
            return Err(self.reject(record, error, true).await);
        }

        // The fence. Written before the socket opens: a crash from here on reads
        // back as an unknown dispatch and is never rerun blindly.
        record.state = AttemptState::Dispatching;
        record.lease = Some(OwnerLease::current(thegn_core::util::now_ms()));
        if let Err(error) = self.persist(&record).await {
            return Err(self.reject(record, error, true).await);
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
                return Err(self.reject(record, error, true).await);
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
        // The heartbeat compare-and-swaps against the stored row, so it only runs
        // while the stored row is the one this run wrote.
        let mut beats = true;
        if let Err(error) = self.persist(&record).await {
            beats = false;
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
        let mut cancelled = false;
        let mut fenced = false;
        let mut beat = tokio::time::interval_at(
            tokio::time::Instant::now() + self.options.lease_heartbeat,
            self.options.lease_heartbeat,
        );
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                _ = beat.tick(), if beats => {
                    if !self.refresh_lease(&mut record).await {
                        // A recovery settled the row: stop trusting this run's
                        // state and fence.
                        fenced = true;
                        break;
                    }
                    continue;
                }
                next = tokio::time::timeout_at(until, session.frames.recv()) => next,
            };
            match next {
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
        if cancelled {
            // A frame already queued when the cancel fired is not lost: when it
            // is the exit, the attempt finished by itself and reports that.
            while let Ok(frame) = session.frames.try_recv() {
                match frame {
                    ExecFrame::Stdout(bytes) => stdout.push(&bytes),
                    ExecFrame::Exit(code) => {
                        exit = Some(code);
                        cancelled = false;
                        break;
                    }
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
            && !cancelled
            && exit == Some(crate::provider::EXEC_CWD_FAILED_STATUS)
            && stderr.render().contains(LAUNCH_FAILED_MARKER);
        let completion = match (timed_out, exit) {
            _ if cancelled => SelectedCompletion::Cancelled,
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
        if cancelled {
            tracing::warn!(
                target: "thegn::sprites::job",
                attempt = record.attempt,
                "attempt cancelled; killing the owned command tree"
            );
        }
        if fenced {
            tracing::warn!(
                target: "thegn::sprites::job",
                attempt = record.attempt,
                "the ledger row was settled by another process; this run is fenced"
            );
            record.error =
                Some("owner lease lost: the ledger row was settled by another process".to_owned());
        }
        if exit.is_none() && !timed_out && !cancelled && !fenced {
            record.error = match tokio::time::timeout(VERDICT_WAIT, verdict).await {
                Ok(Ok(Some(error))) => Some(error.to_string()),
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => None,
            };
        }
        // A fenced run never overwrites the row whoever settled it wrote: the
        // write is guarded by the (now stale) row this run last held.
        let guard = fenced.then(|| record.clone());
        Ok(self
            .conclude(
                record,
                transfer,
                begun,
                completion,
                launch_failed,
                &stdout,
                Some(&stderr),
                guard.as_ref(),
            )
            .await)
    }

    /// Close a reservation that provably never ran a command: nothing was
    /// dispatched (or the provider definitively rejected the dispatch), so no
    /// kill is needed and no spend is committed. Best effort: the partial
    /// checkout is removed, and cleanup stays pending if that fails.
    ///
    /// `touched` is false when nothing was written to the sprite (the project
    /// claim refused): then no `DELETE` is sent, because the sprite may belong
    /// to another project.
    async fn reject(
        &self,
        mut record: AttemptRecord,
        error: anyhow::Error,
        touched: bool,
    ) -> RunnerRefusal {
        let removed = !touched || self.remove_attempt_dirs(&record).await;
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
        record.lease = None;
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
            Err(error) if format!("{error:#}").contains("404") => {
                let project = self.binding.project().as_bytes();
                let first = self.provider.write(sprite, &marker, project).await;
                match first {
                    Ok(()) => Ok(()),
                    // The fs write API may not create parent directories, and a
                    // sprite that was never prepared has no checkout root: make
                    // it, then write the marker once more.
                    Err(error) if format!("{error:#}").contains("404") => {
                        self.make_marker_parent(&marker).await?;
                        self.provider
                            .write(sprite, &marker, project)
                            .await
                            .with_context(|| {
                                format!("sprites: cannot write the owner marker {marker}")
                            })
                    }
                    Err(error) => Err(error).with_context(|| {
                        format!("sprites: cannot write the owner marker {marker}")
                    }),
                }
            }
            Err(error) => Err(error)
                .with_context(|| format!("sprites: cannot read the owner marker {marker}")),
        }
    }

    /// `mkdir -p` the directory that holds the owner marker, over exec.
    async fn make_marker_parent(&self, marker: &str) -> Result<()> {
        let parent = marker.rsplit_once('/').map_or("/", |(dir, _)| dir);
        let parent = if parent.is_empty() { "/" } else { parent };
        let argv = ["mkdir", "-p", "--", parent].map(str::to_owned);
        let (code, output) = self
            .provider
            .run_exec(self.binding.sprite(), &argv, None, &[])
            .await
            .with_context(|| format!("sprites: cannot create {parent}"))?;
        ensure!(
            code == 0,
            "sprites: cannot create {parent} (exit {code}): {}",
            output.trim()
        );
        Ok(())
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
        let cache = self.binding.cache(&self.options.checkout_root);
        for path in &self.options.outputs {
            // `CARGO_TARGET_DIR` is the project cache's `target`, so a declared
            // `target/...` output lives there, not under the checkout.
            let remote = if path.starts_with("target/") {
                join_remote(&cache, path)
            } else {
                join_remote(checkout, path)
            };
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
        self.kill_tree_in(record, false).await
    }

    /// [`kill_tree`](Self::kill_tree); with `nothing_ok`, exit 4 (no control
    /// file, so no command was ever launched) also counts as confirmed.
    async fn kill_tree_in(&self, record: &AttemptRecord, nothing_ok: bool) -> bool {
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
            Ok((4, _)) if nothing_ok => true,
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
        expected: Option<&AttemptRecord>,
    ) -> AttemptReceipt {
        // A recovery of a `Dispatching` row that left no control file has
        // nothing to kill: the launcher never ran.
        let never_started = expected.is_some() && record.state == AttemptState::Dispatching;
        let proven = matches!(
            completion,
            SelectedCompletion::Exited(_) | SelectedCompletion::Signalled(_)
        );
        // Anything short of a proven exit may still be running. Stop it and
        // release the hold now rather than letting it bill until the ceiling.
        record.cleanup = if proven || launch_failed {
            Cleanup::NotNeeded
        } else if self.kill_tree_in(&record, never_started).await {
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
        record.lease = None;
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
        let written = match expected {
            None => self.persist(&record).await.map(|()| true),
            Some(expected) => self.persist_if_unchanged(expected, &record).await,
        };
        match written {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                target: "thegn::sprites::job",
                attempt = record.attempt,
                "the ledger row changed during recovery; its newer state is kept"
            ),
            Err(error) => tracing::warn!(
                target: "thegn::sprites::job",
                %error,
                attempt = record.attempt,
                "cannot record the attempt outcome"
            ),
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

    /// Re-issue the kill for every attempt a crash left unsettled, and mark
    /// cleanup `Done` only when the kill is confirmed. See
    /// [`reconcile_pending_at`](Self::reconcile_pending_at).
    pub async fn reconcile_pending(&self) -> Result<Vec<Reconciled>> {
        self.reconcile_pending_at(thegn_core::util::now_ms()).await
    }

    /// One reconciliation pass at `now_ms`. It handles, for this binding only:
    ///
    /// - terminal attempts whose cleanup is `Pending`: the recorded control file
    ///   and hold are killed again (a rejected attempt only has its directories
    ///   removed, since nothing ever ran), and cleanup becomes `Done` on a
    ///   confirmed kill;
    /// - attempts still `Dispatching` or `Running` once their deadline plus the
    ///   transfer budget, kill budget, a minute of slack and a clock margin have
    ///   passed (an earlier record may still belong to a live run): they are
    ///   killed and closed as `Indeterminate`, because their outcome is unknown.
    ///   A `Dispatching` row that left no control file has nothing to kill.
    ///
    /// Every write is compare-and-swap: the row is re-read before the kill and
    /// published only if it still equals what the pass scanned, so a write by
    /// the owning run or another recovery in between is kept
    /// ([`ReconcileOutcome::Superseded`]).
    ///
    /// This never opens a job exec. The only provider calls are the idempotent
    /// kill and the deletion of per-attempt paths, so a command whose dispatch
    /// outcome is unknown is never rerun as an implicit recovery.
    pub async fn reconcile_pending_at(&self, now_ms: i64) -> Result<Vec<Reconciled>> {
        let store = self.store.clone();
        let records = tokio::task::spawn_blocking(move || store.records())
            .await
            .map_err(|error| anyhow!("ledger task failed: {error}"))??;
        let mut touched = Vec::new();
        for record in records {
            if record.project != self.binding.project()
                || record.sprite != self.binding.sprite()
                || record.account != self.binding.account()
            {
                continue;
            }
            let attempt = record.attempt;
            let outcome = if record.is_terminal() {
                if record.cleanup != Cleanup::Pending {
                    continue;
                }
                self.settle_pending(record).await
            } else {
                if record.state == AttemptState::Reserved || !self.is_stale(&record, now_ms) {
                    continue;
                }
                self.settle_unknown(record).await
            };
            touched.push(Reconciled { attempt, outcome });
        }
        Ok(touched)
    }

    /// Kill again and finish the cleanup of a terminal attempt.
    async fn settle_pending(&self, mut record: AttemptRecord) -> ReconcileOutcome {
        let expected = record.clone();
        if !self.unchanged(&expected).await {
            return ReconcileOutcome::Superseded;
        }
        let (confirmed, removed) = if record.state == AttemptState::Rejected {
            let removed = self.remove_attempt_dirs(&record).await;
            (removed, removed)
        } else if self.kill_tree(&record).await {
            (true, self.remove_attempt_dirs(&record).await)
        } else {
            (false, false)
        };
        if !confirmed {
            return ReconcileOutcome::StillPending;
        }
        record.cleanup = Cleanup::Done;
        record.checkout_removed = removed;
        match self.persist_if_unchanged(&expected, &record).await {
            Ok(true) => ReconcileOutcome::Cleaned,
            Ok(false) => ReconcileOutcome::Superseded,
            Err(error) => {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    %error,
                    attempt = record.attempt,
                    "cannot record the reconciled cleanup"
                );
                ReconcileOutcome::StillPending
            }
        }
    }

    /// Close a non-terminal attempt whose outcome is unknown: kill what may
    /// still run and record `Indeterminate`. Never dispatches.
    async fn settle_unknown(&self, record: AttemptRecord) -> ReconcileOutcome {
        let expected = record.clone();
        if !self.unchanged(&expected).await {
            return ReconcileOutcome::Superseded;
        }
        let mut record = record;
        record.error.get_or_insert_with(|| {
            "reconciled after an interruption: the dispatch outcome is unknown".to_owned()
        });
        let receipt = self
            .conclude_recovered(
                record,
                expected,
                SelectedCompletion::Indeterminate,
                false,
                &Tail::new(1),
            )
            .await;
        if receipt.cleanup == Cleanup::Done {
            ReconcileOutcome::Cleaned
        } else {
            ReconcileOutcome::StillPending
        }
    }

    /// [`conclude`](Self::conclude) for an attempt this call did not dispatch:
    /// no transfer is claimed, the cost window is capped at the deadline, and
    /// the write only lands when the row still equals `expected`.
    async fn conclude_recovered(
        &self,
        record: AttemptRecord,
        expected: AttemptRecord,
        completion: SelectedCompletion,
        launch_failed: bool,
        stdout: &Tail,
    ) -> AttemptReceipt {
        let begun = begun_for(&record);
        let transfer = Transfer {
            digest: record
                .materialized
                .clone()
                .unwrap_or_else(|| record.source.clone()),
            transferred: 0,
            bytes: 0,
        };
        self.conclude(
            record,
            transfer,
            begun,
            completion,
            launch_failed,
            stdout,
            None,
            Some(&expected),
        )
        .await
    }

    /// Reconnect to a persisted attempt.
    ///
    /// - A terminal attempt is replayed from the ledger (its pending cleanup is
    ///   finished first).
    /// - A non-terminal row younger than the reconciler's staleness age may
    ///   still belong to a live run, so nothing is killed and nothing is
    ///   written for it unless the followed stream proves an exit; the report
    ///   carries a `note` instead.
    /// - A `Running` attempt with a session handle is reattached through the
    ///   provider; when the provider still holds the session its stream is
    ///   followed to the exit or the owner's real deadline (`deadline_ms` plus
    ///   the transfer budget) and the attempt is concluded as a normal run
    ///   would.
    /// - A definite 4xx from the provider means the session is gone: when the
    ///   row is stale the owned tree is killed and the attempt is closed as
    ///   `Indeterminate`. A timeout, 5xx or transport error proves nothing, so
    ///   the row is left untouched.
    /// - A stale `Dispatching` row (or `Running` without a handle) is killed
    ///   and closed as `Indeterminate`.
    ///
    /// Nothing is ever dispatched again.
    pub async fn reattach(&self, attempt: u64) -> Result<ReattachReport> {
        let store = self.store.clone();
        let (record, epoch) = tokio::task::spawn_blocking(move || {
            let record = store.read(attempt)?;
            let epoch = store.binding()?.map_or(1, |binding| binding.epoch.max(1));
            anyhow::Ok((record, epoch))
        })
        .await
        .map_err(|error| anyhow!("ledger task failed: {error}"))??;
        let record = record.ok_or_else(|| anyhow!("attempt {attempt} is not in the ledger"))?;
        ensure!(
            record.project == self.binding.project()
                && record.sprite == self.binding.sprite()
                && record.account == self.binding.account(),
            "attempt {attempt} belongs to another project, account or sprite"
        );
        let now = thegn_core::util::now_ms();
        if !record.is_terminal()
            && record
                .lease
                .as_ref()
                .is_some_and(|lease| lease.is_live(now))
        {
            return Ok(Self::left_alone(
                &record,
                false,
                "the attempt is owned by a live run (lease); nothing was touched",
            ));
        }
        let stale = self.is_stale(&record, now);
        match record.resume(epoch) {
            Resume::Rejected => {
                bail!("attempt {attempt} was rejected before any command ran; submit a new attempt")
            }
            Resume::Reserved | Resume::Fresh => bail!(
                "attempt {attempt} was reserved but never dispatched; there is nothing to reattach"
            ),
            Resume::StaleEpoch => bail!("attempt {attempt} predates the current binding epoch"),
            Resume::Terminal(completion) => {
                let cleanup = if record.cleanup == Cleanup::Pending {
                    match self.settle_pending(record.clone()).await {
                        ReconcileOutcome::Cleaned => Cleanup::Done,
                        ReconcileOutcome::StillPending | ReconcileOutcome::Superseded => {
                            Cleanup::Pending
                        }
                    }
                } else {
                    record.cleanup
                };
                Ok(ReattachReport {
                    attempt,
                    completion,
                    cleanup,
                    reattached: false,
                    receipt: None,
                    note: None,
                })
            }
            Resume::UnknownDispatch => {
                if !stale {
                    return Ok(Self::left_alone(
                        &record,
                        false,
                        "the dispatch outcome is unknown but the attempt may still be owned by a \
                         live run",
                    ));
                }
                Ok(self
                    .settle_indeterminate(record, "reattach: the dispatch outcome is unknown")
                    .await)
            }
            Resume::Running => {
                let Some(handle) = record.session.clone() else {
                    if !stale {
                        return Ok(Self::left_alone(
                            &record,
                            false,
                            "no session handle is recorded yet and the attempt may still be \
                             owned by a live run",
                        ));
                    }
                    return Ok(self
                        .settle_indeterminate(record, "reattach: no session handle was recorded")
                        .await);
                };
                let attached = self
                    .provider
                    .attach_exec(self.binding.sprite(), &handle, 80, 24)
                    .await;
                match attached {
                    Ok(session) => Ok(self.follow_reattached(record, session, stale).await),
                    Err(error) if is_definite_exec_rejection(&error) => {
                        if !stale {
                            return Ok(Self::left_alone(
                                &record,
                                false,
                                "the provider no longer holds the session but the attempt may \
                                 still be owned by a live run",
                            ));
                        }
                        let reason =
                            format!("reattach: the provider does not hold the session: {error:#}");
                        Ok(self.settle_indeterminate(record, &reason).await)
                    }
                    Err(error) => Ok(Self::left_alone(
                        &record,
                        false,
                        &format!("cannot reach the session ({error:#}); the row is untouched"),
                    )),
                }
            }
        }
    }

    /// A report for a row this call deliberately did not touch.
    fn left_alone(record: &AttemptRecord, reattached: bool, note: &str) -> ReattachReport {
        ReattachReport {
            attempt: record.attempt,
            completion: SelectedCompletion::Indeterminate,
            cleanup: record.cleanup,
            reattached,
            receipt: None,
            note: Some(note.to_owned()),
        }
    }

    /// Conclude as `Indeterminate` (kill, cleanup) and report it.
    async fn settle_indeterminate(&self, record: AttemptRecord, reason: &str) -> ReattachReport {
        let attempt = record.attempt;
        let expected = record.clone();
        if !self.unchanged(&expected).await {
            return Self::left_alone(&record, false, "the ledger row changed during reattach");
        }
        let mut record = record;
        record.error = Some(reason.to_owned());
        let receipt = self
            .conclude_recovered(
                record,
                expected,
                SelectedCompletion::Indeterminate,
                false,
                &Tail::new(1),
            )
            .await;
        ReattachReport {
            attempt,
            completion: receipt.completion,
            cleanup: receipt.cleanup,
            reattached: false,
            receipt: Some(receipt),
            note: None,
        }
    }

    /// Follow a reattached stream to its exit or the owner's real deadline
    /// (`deadline_ms` is reserve time plus the deadline; the owner's own clock
    /// starts after the transfer, so the transfer budget is added). Only a
    /// proven exit, or a stale row, is concluded; anything else is left to the
    /// owner.
    async fn follow_reattached(
        &self,
        record: AttemptRecord,
        mut session: crate::provider::ExecSession,
        stale: bool,
    ) -> ReattachReport {
        let attempt = record.attempt;
        let expected = record.clone();
        let owner_deadline = record
            .deadline_ms
            .saturating_add(millis_i64(self.options.transfer_budget));
        let remaining = Duration::from_millis(
            u64::try_from(owner_deadline.saturating_sub(thegn_core::util::now_ms())).unwrap_or(0),
        )
        .max(REATTACH_MIN_WAIT);
        let until = tokio::time::Instant::now() + remaining;
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
        if let Err(error) = session.control.send(ExecControl::Close).await {
            tracing::debug!(
                target: "thegn::sprites::job",
                %error,
                attempt,
                "reattached exec session was already gone"
            );
        }
        // A reattached stream is a PTY stream: stderr is merged into stdout.
        let launch_failed = !timed_out
            && exit == Some(crate::provider::EXEC_CWD_FAILED_STATUS)
            && stdout.render().contains(LAUNCH_FAILED_MARKER);
        let completion = match (timed_out, exit) {
            (true, _) => SelectedCompletion::TimedOut,
            (false, Some(_)) if launch_failed => SelectedCompletion::Indeterminate,
            (false, Some(code)) => match signal_of(code) {
                Some(signal) => SelectedCompletion::Signalled(signal),
                None => SelectedCompletion::Exited(code),
            },
            (false, None) => SelectedCompletion::Indeterminate,
        };
        let proven = matches!(
            completion,
            SelectedCompletion::Exited(_) | SelectedCompletion::Signalled(_)
        );
        if !proven && !stale {
            return Self::left_alone(
                &record,
                true,
                "the stream ended without a proven exit and the attempt may still be owned by a \
                 live run; nothing was killed",
            );
        }
        if !self.unchanged(&expected).await {
            return Self::left_alone(&record, true, "the ledger row changed during reattach");
        }
        let receipt = self
            .conclude_recovered(record, expected, completion, launch_failed, &stdout)
            .await;
        ReattachReport {
            attempt,
            completion: receipt.completion,
            cleanup: receipt.cleanup,
            reattached: true,
            receipt: Some(receipt),
            note: None,
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

pub(crate) fn millis_i64(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl SelectedRunner for SpritesSelectedRunner {
    fn submit(
        &self,
        runner_alias: &str,
        recipe: Recipe,
        source: &Digest,
        deadline: Duration,
        cancel: &SelectedCancel,
    ) -> Result<SelectedJobResult, RunnerRefusal> {
        if runner_alias != self.alias {
            return Err(RunnerRefusal::WrongRunner);
        }
        let cancelled = |this: &Self| SelectedJobResult {
            runner: this.alias.clone(),
            recipe,
            source: source.clone(),
            completion: SelectedCompletion::Cancelled,
            log_tail: String::new(),
            truncated: false,
        };
        if cancel.is_cancelled() {
            return Ok(cancelled(self));
        }
        let content = self
            .snapshot
            .capture()
            .map_err(|error| self.unavailable(format!("cannot capture the snapshot: {error:#}")))?;
        // Bridge the route's std-only token to the executor's own cancel handle.
        let token = CancelToken::new();
        let bridge = token.clone();
        cancel.on_cancel(move || bridge.cancel());
        let receipt = match self
            .handle
            .block_on(self.run_cancellable(recipe, source, &content, deadline, &token))
        {
            Ok(receipt) => receipt,
            // A cancel that closed the attempt before dispatch is recorded as
            // rejected (nothing was sent, so no command ran); the route reads it
            // back as a user cancel rather than an unavailable runner.
            Err(RunnerRefusal::RunnerUnavailable(_)) if token.is_cancelled() => {
                return Ok(cancelled(self));
            }
            Err(refusal) => return Err(refusal),
        };
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
    write!(log, "\n[stderr, last {seen} bytes]\n{}", stderr.render())
        .expect("writing to String cannot fail");
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
            "certs/server.pem",
            "keys/Deploy.KEY",
            "bundle.p12",
            ".envrc",
            "app/.envrc.local",
            "infra/prod.tfvars",
            "infra/terraform.tfstate.backup",
            "gcp/service-account-prod.json",
            "services/api/.env",
            "web/.npmrc",
            "web/.env.production",
            "pkg/.claude/settings.json",
            "vendor/sub/.git/HEAD",
            "tools/id_rsa",
            "tools/id_ed25519_sk",
            "tools/ID_ECDSA",
            "tools/id_dsa",
            "ci/.cargo/credentials.toml",
            "home/.config/gh/hosts.yml",
        ] {
            assert!(denied_path(denied), "{denied} must be denied");
            assert!(
                checked_rel_path(denied).is_err(),
                "{denied} must be refused"
            );
        }
        assert!(!denied_path("tools/id_rsa.pub"));
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
            lease: None,
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
