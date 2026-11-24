//! Selected build/test Jobs on one project-bound Sprite.
//!
//! This is the provider-side executor for the typed selected-runner route in
//! [`thegn_job_spec::selected`]: a pinned source digest plus one closed
//! [`Recipe`] go in, a bounded [`SelectedJobResult`] comes out. It is
//! deliberately not a general command transport — the only argv it ever sends
//! is [`Recipe::argv`], wrapped in the keep-awake hold
//! ([`thegn_core::sprite_hold`]), whose script drops the hold when the command
//! exits, so a hold outlives a quiet build but never the attempt.
//!
//! Accounting, in the order it happens:
//!
//! 1. **reserve** — attempt number, binding epoch and the spend cap are written
//!    to the attempt ledger *before* anything reaches the sprite, and the
//!    attempt is refused when the project's envelope is already committed.
//!    Attempt numbers are never reused, so a duplicate submission is a refusal
//!    rather than a second run.
//! 2. **materialize** — the authorized snapshot bytes are written into a
//!    per-attempt checkout and read back; the digest of what the sprite holds
//!    must equal the pinned digest, so an unverified transfer never runs.
//! 3. **dispatch** — the record moves to [`AttemptState::Dispatching`] *before*
//!    the exec socket opens. A request whose outcome was never observed is
//!    therefore never rerun as an implicit recovery: it reads back as
//!    [`Resume::UnknownDispatch`].
//! 4. **run** — stdout and stderr stream into bounded tails until the original
//!    exit code, the attempt deadline, or the end of the stream without one. A
//!    deadline closes the owned command tree and reports
//!    [`SelectedCompletion::Cancelled`] with cleanup pending.
//! 5. **report** — provider identity, session handle, checkout, exit, cleanup
//!    state and the per-attempt cost estimate land in the ledger and in the
//!    receipt, so a client that lost its channel can reconcile by attempt
//!    number.
//!
//! Nothing here destroys a sprite, checkpoints one, kills a process it did not
//! start, or reads another project's rows: the ledger directory and the
//! checkout path are both keyed by project. The ledger is a durable private
//! file store (`$XDG_STATE_HOME/thegn/sprites-jobs/<project>/`) rather than the
//! DB because both the dispatcher and a later reconciliation read it without a
//! live session. Every ledger read and write is synchronous on purpose:
//! [`SpritesSelectedRunner::submit`] runs the attempt on the caller's own
//! thread, so no ledger file I/O ever lands on a runtime worker.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thegn_cas::Digest;
use thegn_core::fsperm::{PrivateDir, ensure_private_dir, open_private_dir, owned_by_current_user};
use thegn_core::sprite_hold::{HOLD_EXPIRE_SECS, hold_name, wrap_argv};
use thegn_job_spec::selected::MAX_SELECTED_LOG_BYTES;
use thegn_job_spec::{
    Recipe, RunnerRefusal, SelectedCompletion, SelectedJobResult, SelectedRunner,
};

use crate::provider::{
    EXEC_CWD_FAILED_STATUS, ExecControl, ExecFrame, ExecSpec, ExecStderr, ProviderFiles,
    SpritesProvider, join_remote,
};

/// The provider name written into every record and receipt.
pub const PROVIDER: &str = "sprites";

/// Root of the per-attempt checkouts on the sprite.
pub const DEFAULT_CHECKOUT_ROOT: &str = "/workspace/.thegn/jobs";

/// Prefix of the job-spec alias a `runner:` selects (`sprites-<sprite>`).
pub const ALIAS_PREFIX: &str = "sprites-";

/// Default wall clock an attempt may run before it is cancelled.
pub const DEFAULT_ATTEMPT_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Default per-attempt spend cap, in micro-USD.
pub const DEFAULT_SPEND_CAP_MICROS: u64 = 60_000;

/// Most files one snapshot may carry.
pub const SNAPSHOT_MAX_FILES: usize = 20_000;

/// Most bytes one snapshot may carry.
pub const SNAPSHOT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Largest ledger record read back (a record is under a kilobyte).
const RECORD_LIMIT: u64 = 256 * 1024;

const BINDING_FILE: &str = "binding.json";
const RESERVE_LOCK: &str = ".reserve.lock";

/// How long a freshly opened session may take to announce its handle.
const SESSION_HANDLE_WAIT: Duration = Duration::from_secs(10);

/// How long a closed stream's protocol verdict may take to arrive.
const VERDICT_WAIT: Duration = Duration::from_secs(2);

/// Path components a snapshot never carries: version control, thegn's own
/// state, and agent/provider credentials. Checked on the first component, so a
/// directory is pruned with its whole subtree.
const DENIED_ROOTS: [&str; 7] = [
    ".git", ".thegn", ".ssh", ".aws", ".gnupg", ".claude", ".codex",
];

/// Credential files that are never part of a build input, by first component.
const DENIED_FILES: [&str; 4] = [".env", ".netrc", ".npmrc", ".pypirc"];

/// True when `rel` is content a job snapshot must never carry.
fn denied_path(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or(rel);
    DENIED_ROOTS.contains(&first)
        || DENIED_FILES.contains(&first)
        || (first.starts_with(".env.") && first.len() > ".env.".len())
}

/// Validate a repo-relative path: relative, no traversal, no control bytes, and
/// not one of the credential trees. Every transferred path goes through here.
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

/// Canonical digest over `path`, NUL, decimal length, NUL, bytes for every entry
/// in sorted path order. Two trees that agree byte-for-byte hash equal; one
/// changed byte anywhere changes the digest.
pub fn tree_digest(files: &BTreeMap<String, Vec<u8>>) -> Digest {
    let mut canonical = Vec::new();
    for (path, bytes) in files {
        canonical.extend_from_slice(path.as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(bytes.len().to_string().as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(bytes);
    }
    Digest::of(&canonical)
}

/// The authorized input content of one attempt, captured before dispatch.
///
/// Only repo-relative paths survive [`insert`](Self::insert); the digest is the
/// canonical tree hash of exactly the bytes that will be transferred, so a
/// capture that lost or added one byte cannot match the pinned identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotContent {
    files: BTreeMap<String, Vec<u8>>,
}

impl SnapshotContent {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one repo-relative file. Refuses traversal, absolute paths and the
    /// credential trees, so a caller cannot widen the snapshot by accident.
    pub fn insert(&mut self, path: &str, bytes: Vec<u8>) -> Result<()> {
        checked_rel_path(path)?;
        ensure!(
            self.files.len() < SNAPSHOT_MAX_FILES || self.files.contains_key(path),
            "snapshot has more than {SNAPSHOT_MAX_FILES} files"
        );
        self.files.insert(path.to_owned(), bytes);
        Ok(())
    }

    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    pub fn files(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.files
            .iter()
            .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.values().fold(0u64, |total, bytes| {
            total.saturating_add(bytes.len() as u64)
        })
    }

    /// The digest the pinned source identity is compared against.
    pub fn digest(&self) -> Digest {
        tree_digest(&self.files)
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

/// A snapshot captured from a local worktree directory. Symlinks, `.git`,
/// `.thegn`, the agent homes and credential files are never captured.
#[derive(Debug, Clone)]
pub struct DirSnapshot {
    root: PathBuf,
}

impl DirSnapshot {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl SnapshotSource for DirSnapshot {
    fn capture(&self) -> Result<SnapshotContent> {
        let mut content = SnapshotContent::new();
        let mut total = 0u64;
        let mut pending = vec![PathBuf::new()];
        while let Some(rel_dir) = pending.pop() {
            let dir = if rel_dir.as_os_str().is_empty() {
                self.root.clone()
            } else {
                self.root.join(&rel_dir)
            };
            let entries = std::fs::read_dir(&dir)
                .with_context(|| format!("cannot read snapshot directory {}", dir.display()))?;
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("cannot read an entry in {}", dir.display()))?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    bail!(
                        "snapshot path {} is not valid UTF-8",
                        entry.file_name().to_string_lossy()
                    );
                };
                let rel = if rel_dir.as_os_str().is_empty() {
                    name
                } else {
                    format!("{}/{}", rel_dir.to_string_lossy(), name)
                };
                if denied_path(&rel) {
                    continue;
                }
                // `DirEntry::metadata` does not follow the link, so a symlink is
                // skipped rather than read through to where it points.
                let metadata = entry
                    .metadata()
                    .with_context(|| format!("cannot stat snapshot entry {rel}"))?;
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    pending.push(PathBuf::from(&rel));
                    continue;
                }
                if !metadata.is_file() {
                    continue;
                }
                let bytes = std::fs::read(entry.path())
                    .with_context(|| format!("cannot read snapshot file {rel}"))?;
                total = total.saturating_add(bytes.len() as u64);
                ensure!(
                    total <= SNAPSHOT_MAX_BYTES,
                    "snapshot is larger than {SNAPSHOT_MAX_BYTES} bytes"
                );
                content.insert(&rel, bytes)?;
            }
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

    /// The per-attempt checkout on the sprite: project-scoped, so a second
    /// project's attempt can never read this one's inputs or outputs.
    pub fn checkout(&self, attempt: u64, root: &str) -> String {
        format!("{}/{}/{attempt}", root.trim_end_matches('/'), self.project)
    }

    /// The project-scoped build cache, retained between attempts so a warm loop
    /// does not rebuild from scratch. It is keyed by project, so it is the only
    /// state one attempt may leave for the next: no source file, no output and
    /// no credential is ever shared across projects.
    pub fn cache(&self, root: &str) -> String {
        format!("{}/{}/cache", root.trim_end_matches('/'), self.project)
    }
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
    /// Cancelled at our request; the owned command tree was closed.
    Cancelled,
    /// The stream ended without a terminal exit.
    Indeterminate,
}

/// Cleanup state of the command tree an attempt left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cleanup {
    /// The command ended by itself; nothing is outstanding.
    NotNeeded,
    /// The tree may still be terminating; a reconciler confirms it.
    Pending,
    /// Confirmed gone ([`AttemptStore::mark_cleaned`]).
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
    pub cleanup: Cleanup,
    /// The verified digest of the materialized checkout.
    pub materialized: Option<Digest>,
    /// Declared output path → SHA-256 hex, when they were read.
    pub outputs: BTreeMap<String, String>,
    /// True when outputs were not read because the attempt did not prove an
    /// exit: a late writer must not be mistaken for a finished build.
    pub outputs_fenced: bool,
    /// Why the attempt was closed without a receipt, when that happened.
    pub error: Option<String>,
}

impl AttemptRecord {
    /// What this attempt counts against the envelope: the reported spend, or
    /// the whole reserved cap while it has not reported.
    pub fn committed_micros(&self) -> u64 {
        self.spent_micros.unwrap_or(self.reserved_micros)
    }

    /// A terminal state means no further provider call is possible.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            AttemptState::Finished | AttemptState::Cancelled | AttemptState::Indeterminate
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
            AttemptState::Finished => {
                Resume::Terminal(SelectedCompletion::Exited(self.exit.unwrap_or(1)))
            }
            AttemptState::Cancelled => Resume::Terminal(SelectedCompletion::Cancelled),
            AttemptState::Indeterminate => Resume::Terminal(SelectedCompletion::Indeterminate),
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
        AttemptState::Cancelled => "cancelled",
        AttemptState::Indeterminate => "indeterminate",
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

/// The requested reservation of one attempt number.
pub struct ReserveRequest<'a> {
    pub attempt: u64,
    pub recipe: Recipe,
    pub source: &'a Digest,
    pub checkout: String,
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
        let epoch = store_error(self.binding_epoch(&root, binding))?;
        if let Some(existing) =
            store_error(self.read_record::<AttemptRecord>(&root, &attempt_file(request.attempt)))?
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
        let record = AttemptRecord {
            provider: PROVIDER.to_owned(),
            project: binding.project().to_owned(),
            account: binding.account().to_owned(),
            sprite: binding.sprite().to_owned(),
            attempt: request.attempt,
            epoch,
            recipe: request.recipe,
            source: request.source.clone(),
            checkout: request.checkout,
            state: AttemptState::Reserved,
            session: None,
            hold: None,
            reserved_micros: request.spend_cap_micros,
            spent_micros: None,
            created_ms: thegn_core::util::now_ms(),
            deadline_ms: request.deadline_ms,
            finished_ms: None,
            exit: None,
            cleanup: Cleanup::NotNeeded,
            materialized: None,
            outputs: BTreeMap::new(),
            outputs_fenced: false,
            error: None,
        };
        store_error(self.mark(&record))?;
        Ok(record)
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
    /// Wall clock an attempt may run before it is cancelled.
    pub deadline: Duration,
    /// Spend this attempt reserves, in micro-USD.
    pub spend_cap_micros: u64,
    /// Total the project may commit across attempts (0 = unlimited).
    pub spend_envelope_micros: u64,
    /// Configured compute price, in micro-USD per second (0 = unpriced).
    pub micros_per_second: u64,
    /// Keep-awake ceiling (0 = deadline plus one hold expiry).
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

    /// The keep-awake ceiling: explicit, else the deadline plus one expiry, so
    /// a quiet build never loses its hold before its own deadline.
    pub fn effective_hold_ceiling_secs(&self) -> u64 {
        if self.hold_ceiling_secs > 0 {
            self.hold_ceiling_secs
        } else {
            self.deadline.as_secs().saturating_add(HOLD_EXPIRE_SECS)
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
    pub cleanup: Cleanup,
    pub outputs: BTreeMap<String, String>,
    pub outputs_missing: Vec<String>,
    pub outputs_fenced: bool,
    pub files_transferred: usize,
    pub files_reused: usize,
    pub bytes_transferred: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub log_tail: String,
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
}

/// Trim a log tail to the contract's bound from the front, on a char boundary.
fn bound_log(log: String) -> String {
    if log.len() <= MAX_SELECTED_LOG_BYTES {
        return log;
    }
    let mut start = log.len() - MAX_SELECTED_LOG_BYTES;
    while !log.is_char_boundary(start) {
        start += 1;
    }
    log.split_off(start)
}

/// The keep-awake task name for one attempt: `thegn-hold-<16 hex>`, derived from
/// the binding and the attempt so a retry of the same attempt reuses one hold.
fn hold_task(project: &str, sprite: &str, attempt: u64) -> Option<String> {
    let mut hasher = Sha256::new();
    hasher.update(project.as_bytes());
    hasher.update([0]);
    hasher.update(sprite.as_bytes());
    hasher.update([0]);
    hasher.update(attempt.to_le_bytes());
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

/// `Recipe::argv` wrapped in the hold, as the sprite receives it.
fn attempt_argv(recipe: Recipe, hold: &str, ceiling_secs: u64) -> Vec<String> {
    let argv: Vec<String> = recipe.argv().iter().map(|arg| (*arg).to_owned()).collect();
    wrap_argv(&argv, hold, ceiling_secs)
}

/// Sprites-side executor for the typed selected-runner route.
///
/// One runner is bound to one project and one sprite. It holds a runtime handle
/// rather than a runtime so the [`SelectedRunner`] contract stays synchronous:
/// `submit` is called from the dispatcher's own thread and enters the runtime
/// itself. Calling it *from* a runtime worker would panic inside
/// [`tokio::runtime::Handle::block_on`], which is why the async entry point
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

    /// Run one attempt now.
    ///
    /// A failure that provably happened before the sprite accepted a session
    /// (an unreadable ledger, a transfer that did not round-trip, a rejected
    /// handshake) closes the reservation and returns a refusal the caller may
    /// retry as a new attempt. Once a session exists the only outcomes are
    /// receipts: an unproven exit is [`SelectedCompletion::Indeterminate`] with
    /// cleanup pending, never an error a caller might retry as though nothing
    /// had run. A `cd` that cannot enter the checkout (the wrapper's
    /// [`EXEC_CWD_FAILED_STATUS`]) is likewise `Indeterminate`, because the
    /// recipe itself never ran there.
    pub async fn run(
        &self,
        recipe: Recipe,
        source: &Digest,
        content: &SnapshotContent,
    ) -> Result<AttemptReceipt, RunnerRefusal> {
        let observed = content.digest();
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
        let started = tokio::time::Instant::now();
        let attempt = self
            .store
            .next_attempt()
            .map_err(|error| self.unavailable(error))?;
        let checkout = self.binding.checkout(attempt, &self.options.checkout_root);
        let created_ms = thegn_core::util::now_ms();
        let mut record = self
            .store
            .reserve(
                &self.binding,
                ReserveRequest {
                    attempt,
                    recipe,
                    source,
                    checkout: checkout.clone(),
                    deadline_ms: created_ms.saturating_add(self.deadline_ms()),
                    spend_cap_micros: self.options.spend_cap_micros,
                    spend_envelope_micros: self.options.spend_envelope_micros,
                },
            )
            .map_err(|refusal| self.unavailable(refusal))?;

        let transfer = match self.materialize(&checkout, content).await {
            Ok(transfer) => transfer,
            Err(error) => return Err(self.abandon(record, error)),
        };
        record.materialized = Some(transfer.digest.clone());
        let Some(hold) = hold_task(&record.project, &record.sprite, attempt) else {
            return Err(self.abandon(record, anyhow::anyhow!("cannot derive a hold task name")));
        };
        record.hold = Some(hold.clone());

        // The fence. Written before the socket opens: a crash from here on reads
        // back as an unknown dispatch and is never rerun blindly.
        record.state = AttemptState::Dispatching;
        if let Err(error) = self.store.mark(&record) {
            return Err(self.abandon(record, error));
        }

        let spec = ExecSpec {
            argv: attempt_argv(recipe, &hold, self.options.effective_hold_ceiling_secs()),
            tty: false,
            cols: 0,
            rows: 0,
            env: vec![(
                "CARGO_TARGET_DIR".to_owned(),
                self.binding.cache(&self.options.checkout_root),
            )],
            cwd: Some(checkout),
        };
        let streaming = match self
            .provider
            .open_exec_streaming(self.binding.sprite(), &spec)
            .await
        {
            Ok(streaming) => streaming,
            Err(error) => {
                // The handshake never completed, so no session — and therefore
                // no command — ever existed on the sprite.
                tracing::warn!(
                    target: "thegn::sprites::job",
                    %error,
                    attempt = record.attempt,
                    "exec dispatch was refused before a session existed"
                );
                return Err(self.abandon(record, error));
            }
        };
        let mut session = streaming.session;
        let stderr = streaming.stderr;
        let verdict = streaming.verdict;
        record.session = session_handle(&mut session.session_id).await;
        record.state = AttemptState::Running;
        if let Err(error) = self.store.mark(&record) {
            // A failed re-mark leaves the earlier `dispatching` fence, which is
            // already enough to forbid an implicit rerun.
            tracing::warn!(
                target: "thegn::sprites::job",
                %error,
                attempt = record.attempt,
                "cannot record the exec session handle"
            );
        }

        let until = tokio::time::Instant::now() + self.remaining(started);
        let mut stdout = Tail::new(MAX_SELECTED_LOG_BYTES);
        let mut exit = None;
        let mut cancelled = false;
        loop {
            match tokio::time::timeout_at(until, session.frames.recv()).await {
                Ok(Some(ExecFrame::Stdout(bytes))) => stdout.push(&bytes),
                Ok(Some(ExecFrame::Exit(code))) => {
                    exit = Some(code);
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    cancelled = true;
                    break;
                }
            }
        }
        // Stop the owned command tree. The driver closes the socket either way;
        // the hold script's own trap then drops the hold.
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
        let completion = match (cancelled, exit) {
            (true, _) => SelectedCompletion::Cancelled,
            (false, Some(code)) if code == EXEC_CWD_FAILED_STATUS => {
                tracing::warn!(
                    target: "thegn::sprites::job",
                    attempt = record.attempt,
                    checkout = %record.checkout,
                    "the checkout directory never opened; the recipe did not run"
                );
                SelectedCompletion::Indeterminate
            }
            (false, Some(code)) => SelectedCompletion::Exited(code),
            (false, None) => SelectedCompletion::Indeterminate,
        };
        if cancelled {
            tracing::warn!(
                target: "thegn::sprites::job",
                attempt = record.attempt,
                deadline_secs = self.options.deadline.as_secs(),
                "attempt deadline reached; the owned command tree was closed"
            );
        }
        // A stream that ended without a terminal exit is worth explaining: the
        // driver's verdict distinguishes a protocol violation from a plain
        // close, and lands in the record either way. The wait is bounded so a
        // missing verdict never stalls the attempt.
        let dispatch_error = if exit.is_some() || cancelled {
            None
        } else {
            match tokio::time::timeout(VERDICT_WAIT, verdict).await {
                Ok(Ok(Some(error))) => Some(error.to_string()),
                Ok(Ok(None)) | Ok(Err(_)) | Err(_) => None,
            }
        };
        record.error = dispatch_error;
        Ok(self
            .conclude(
                record,
                transfer,
                started,
                completion,
                &stdout,
                Some(&stderr),
            )
            .await)
    }

    /// Close a reservation that provably never reached the sprite: nothing ran,
    /// so no cleanup is pending and no spend is committed.
    fn abandon(&self, mut record: AttemptRecord, error: anyhow::Error) -> RunnerRefusal {
        record.state = AttemptState::Indeterminate;
        record.cleanup = Cleanup::NotNeeded;
        record.spent_micros = Some(0);
        record.outputs_fenced = true;
        record.finished_ms = Some(thegn_core::util::now_ms());
        record.error = Some(format!("{error:#}"));
        if let Err(write_error) = self.store.mark(&record) {
            tracing::warn!(
                target: "thegn::sprites::job",
                %write_error,
                attempt = record.attempt,
                "cannot record the abandoned attempt"
            );
        }
        self.unavailable(error)
    }

    /// Write the authorized bytes into the attempt checkout and verify them by
    /// reading them back. A file whose remote copy already matches is not
    /// transferred again, but it is still covered by the verified digest.
    async fn materialize(&self, checkout: &str, content: &SnapshotContent) -> Result<Transfer> {
        let mut materialized = BTreeMap::new();
        let mut transferred = 0usize;
        let mut reused = 0usize;
        let mut bytes = 0u64;
        for (path, wanted) in content.files() {
            let remote = join_remote(checkout, path);
            // A remote copy that already matches is not transferred again, but
            // it is still covered by the verified digest. Absent, unreadable or
            // different: transfer it and let the read-back prove what landed.
            let existing = self.provider.read(self.binding.sprite(), &remote).await;
            if matches!(&existing, Ok(remote_bytes) if remote_bytes.as_slice() == wanted) {
                reused += 1;
                materialized.insert(path.to_owned(), wanted.to_vec());
                continue;
            }
            self.provider
                .write(self.binding.sprite(), &remote, wanted)
                .await
                .with_context(|| format!("sprites: cannot write {remote}"))?;
            let read_back = self
                .provider
                .read(self.binding.sprite(), &remote)
                .await
                .with_context(|| format!("sprites: cannot verify {remote}"))?;
            ensure!(
                read_back == wanted,
                "sprites: {remote} did not round-trip ({} bytes written, {} bytes read)",
                wanted.len(),
                read_back.len()
            );
            transferred += 1;
            bytes = bytes.saturating_add(wanted.len() as u64);
            materialized.insert(path.to_owned(), read_back);
        }
        Ok(Transfer {
            digest: tree_digest(&materialized),
            transferred,
            reused,
            bytes,
        })
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

    /// Record the terminal state, hash what may be hashed, and build the
    /// receipt. Every exit from [`run`](Self::run) after the dispatch fence goes
    /// through here, so no path can forget the ledger.
    async fn conclude(
        &self,
        mut record: AttemptRecord,
        transfer: Transfer,
        started: tokio::time::Instant,
        completion: SelectedCompletion,
        stdout: &Tail,
        stderr: Option<&ExecStderr>,
    ) -> AttemptReceipt {
        let (cost_micros, cost_basis) = self.cost(started.elapsed());
        record.state = match completion {
            SelectedCompletion::Exited(_) => AttemptState::Finished,
            SelectedCompletion::Cancelled => AttemptState::Cancelled,
            SelectedCompletion::Indeterminate => AttemptState::Indeterminate,
        };
        record.exit = match completion {
            SelectedCompletion::Exited(code) => Some(code),
            SelectedCompletion::Cancelled | SelectedCompletion::Indeterminate => None,
        };
        record.cleanup = match completion {
            SelectedCompletion::Exited(_) => Cleanup::NotNeeded,
            _ => Cleanup::Pending,
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
        if let Err(error) = self.store.mark(&record) {
            tracing::warn!(
                target: "thegn::sprites::job",
                %error,
                attempt = record.attempt,
                "cannot record the attempt outcome"
            );
        }
        let stdout_tail = stdout.render();
        let stderr_tail = stderr.map(ExecStderr::render).unwrap_or_default();
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
            cleanup: record.cleanup,
            outputs,
            outputs_missing,
            outputs_fenced: record.outputs_fenced,
            files_transferred: transfer.transferred,
            files_reused: transfer.reused,
            bytes_transferred: transfer.bytes,
            log_tail: bound_log(compose_log(&stdout_tail, stderr, record.exit)),
            stdout_tail,
            stderr_tail,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            cost_micros,
            cost_basis,
        }
    }

    fn deadline_ms(&self) -> i64 {
        i64::try_from(self.options.deadline.as_millis()).unwrap_or(i64::MAX)
    }

    fn remaining(&self, started: tokio::time::Instant) -> Duration {
        self.options.deadline.saturating_sub(started.elapsed())
    }

    /// The per-attempt estimate: elapsed time at the configured rate, or an
    /// explicitly unlabeled (`None`) cost when no rate is configured.
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

impl SelectedRunner for SpritesSelectedRunner {
    fn submit(
        &self,
        runner_alias: &str,
        recipe: Recipe,
        source: &Digest,
    ) -> Result<SelectedJobResult, RunnerRefusal> {
        if runner_alias != self.alias {
            return Err(RunnerRefusal::WrongRunner);
        }
        let content = self
            .snapshot
            .capture()
            .map_err(|error| self.unavailable(format!("cannot capture the snapshot: {error:#}")))?;
        let receipt = self.handle.block_on(self.run(recipe, source, &content))?;
        Ok(SelectedJobResult {
            runner: self.alias.clone(),
            recipe,
            source: source.clone(),
            completion: receipt.completion,
            log_tail: receipt.log_tail,
        })
    }
}

/// What one materialization moved.
struct Transfer {
    digest: Digest,
    transferred: usize,
    reused: usize,
    bytes: u64,
}

/// The bounded log tail: stdout, then the stderr tail for a non-zero exit — the
/// same shape the provider's one-shot exec reports.
fn compose_log(stdout: &str, stderr: Option<&ExecStderr>, exit: Option<i32>) -> String {
    let mut log = stdout.to_owned();
    if exit == Some(0) {
        return log;
    }
    let Some(stderr) = stderr else {
        return log;
    };
    let seen = stderr.total().min(crate::provider::STDERR_TAIL_CAP as u64);
    if seen == 0 {
        return log;
    }
    log.push_str(&format!(
        "\n[stderr, last {seen} bytes]\n{}",
        stderr.render()
    ));
    log
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_digest_is_order_independent_and_byte_sensitive() {
        let mut one = BTreeMap::new();
        one.insert("b.txt".to_owned(), b"two".to_vec());
        one.insert("a.txt".to_owned(), b"one".to_vec());
        let mut two = BTreeMap::new();
        two.insert("a.txt".to_owned(), b"one".to_vec());
        two.insert("b.txt".to_owned(), b"two".to_vec());
        assert_eq!(tree_digest(&one), tree_digest(&two));
        two.insert("a.txt".to_owned(), b"one!".to_vec());
        assert_ne!(tree_digest(&one), tree_digest(&two));
    }

    #[test]
    fn tree_digest_separates_paths_from_content() {
        let mut split = BTreeMap::new();
        split.insert("a".to_owned(), b"bc".to_vec());
        let mut joined = BTreeMap::new();
        joined.insert("ab".to_owned(), b"c".to_vec());
        assert_ne!(tree_digest(&split), tree_digest(&joined));
    }

    #[test]
    fn credential_and_vcs_paths_are_never_transferred() {
        for denied in [
            ".git/config",
            ".thegn/state.json",
            ".ssh/id_ed25519",
            ".aws/credentials",
            ".gnupg/secring",
            ".claude/settings.json",
            ".codex/auth.json",
            ".env",
            ".env.local",
            ".netrc",
            ".npmrc",
            ".pypirc",
        ] {
            assert!(denied_path(denied), "{denied} must be denied");
            assert!(
                checked_rel_path(denied).is_err(),
                "{denied} must be refused"
            );
        }
        // The rule keys on the first component, so a nested `.git` is ordinary
        // content and `.environment` is not `.env`.
        assert!(!denied_path("src/.git/HEAD"));
        checked_rel_path("src/.git/HEAD").unwrap();
        assert!(!denied_path(".environment"));
        checked_rel_path("src/main.rs").unwrap();
        checked_rel_path("a/b/c.txt").unwrap();
        assert!(checked_rel_path("../escape").is_err());
        assert!(checked_rel_path("/abs").is_err());
        assert!(checked_rel_path("a//b").is_err());
        assert!(checked_rel_path("a/./b").is_err());
        assert!(checked_rel_path("a\\b").is_err());
    }

    #[test]
    fn tail_keeps_the_last_cap_bytes_and_the_total() {
        let mut tail = Tail::new(8);
        tail.push(b"12345");
        tail.push(b"6789");
        assert_eq!(tail.total(), 9);
        assert_eq!(tail.render(), "23456789");
        tail.push(b"0123456789abc");
        assert_eq!(tail.render(), "56789abc");
        assert_eq!(tail.total(), 22);
    }

    #[test]
    fn log_tail_is_bounded_from_the_front() {
        let long = "x".repeat(MAX_SELECTED_LOG_BYTES + 100);
        let bounded = bound_log(long);
        assert_eq!(bounded.len(), MAX_SELECTED_LOG_BYTES);
        assert_eq!(bound_log("short".to_owned()), "short");
    }

    #[test]
    fn hold_task_names_are_shared_shape() {
        let name = hold_task("proj", "sprite-one", 2).unwrap();
        assert_eq!(name.len(), "thegn-hold-".len() + 16);
        assert!(name.starts_with("thegn-hold-"));
        assert_eq!(Some(name.clone()), hold_task("proj", "sprite-one", 2));
        assert_ne!(Some(name), hold_task("proj", "sprite-one", 3));
    }

    #[test]
    fn attempt_argv_wraps_only_the_recipe_argv() {
        let argv = attempt_argv(Recipe::CargoTest, "thegn-hold-0123456789abcdef", 900);
        assert_eq!(argv.len(), 8);
        assert_eq!(argv[0], "/bin/sh");
        assert_eq!(argv[1], "-c");
        assert_eq!(argv[2], thegn_core::sprite_hold::HOLD_SCRIPT);
        assert_eq!(argv[3], "thegn-hold");
        assert_eq!(argv[4], "thegn-hold-0123456789abcdef");
        assert_eq!(argv[5], "900");
        let recipe: Vec<&str> = argv[6..].iter().map(String::as_str).collect();
        assert_eq!(recipe, vec!["cargo", "test"]);
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
    fn checkouts_and_caches_are_project_scoped_and_never_shared() {
        let one = ProjectBinding::new("proj-one", "acct", "sprite-a").unwrap();
        let two = ProjectBinding::new("proj-two", "acct", "sprite-a").unwrap();
        let first = one.checkout(1, DEFAULT_CHECKOUT_ROOT);
        let second = two.checkout(1, DEFAULT_CHECKOUT_ROOT);
        assert_ne!(first, second);
        assert_eq!(first, "/workspace/.thegn/jobs/proj-one/1");
        assert_eq!(second, "/workspace/.thegn/jobs/proj-two/1");
        // Checkouts are per attempt as well, so a rerun never inherits a tree.
        assert_ne!(
            one.checkout(1, DEFAULT_CHECKOUT_ROOT),
            one.checkout(2, DEFAULT_CHECKOUT_ROOT)
        );
        // The cache is shared across attempts of one project and never across
        // projects: it is the only state an attempt leaves behind.
        assert_ne!(
            one.cache(DEFAULT_CHECKOUT_ROOT),
            two.cache(DEFAULT_CHECKOUT_ROOT)
        );
        assert_eq!(
            one.cache(DEFAULT_CHECKOUT_ROOT),
            "/workspace/.thegn/jobs/proj-one/cache"
        );
        assert!(!one.cache(DEFAULT_CHECKOUT_ROOT).starts_with(first.as_str()));
    }

    #[test]
    fn the_hold_ceiling_outlives_the_deadline_unless_it_is_pinned() {
        let options = AttemptOptions::default();
        assert_eq!(
            options.effective_hold_ceiling_secs(),
            DEFAULT_ATTEMPT_DEADLINE.as_secs() + HOLD_EXPIRE_SECS
        );
        let pinned = AttemptOptions {
            hold_ceiling_secs: 900,
            ..AttemptOptions::default()
        };
        assert_eq!(pinned.effective_hold_ceiling_secs(), 900);
    }
}
