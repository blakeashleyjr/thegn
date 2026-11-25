//! Selected build/test Jobs on a project-selected Vercel Sandbox.
//!
//! The provider-side executor for the typed selected-runner route in
//! [`thegn_job_spec::selected`], implementing the same [`SelectedRunner`]
//! contract as the Sprites executor (`crate::sprites::job`): a pinned source
//! digest, one closed [`Recipe`] and the route's deadline go in, a bounded
//! [`SelectedJobResult`] comes out. The agent, its model login and the Vercel
//! token never leave the developer's machine: the only argv sent to the sandbox
//! is [`Recipe::argv`] behind a launcher that runs it under `env -i`, and the
//! command body carries no environment at all.
//!
//! One attempt owns one fresh sandbox, so a fresh sandbox is sufficient to
//! finish and nothing outlives the attempt. Accounting, in order:
//!
//! 1. **identity** — the provider's Vercel team and project must equal the
//!    binding's before the ledger is written or any request is made, and the
//!    ledger directory is pinned to the first team/project it served (a later
//!    mismatch refuses, also on recovery).
//! 2. **reserve** — attempt number and spend cap are written to the ledger
//!    before anything reaches Vercel, under the ledger lock. The envelope
//!    refuses an attempt that would exceed it.
//! 3. **create** — the per-attempt sandbox name is persisted (cleanup pending)
//!    *before* the create request, so an ambiguous create is findable by name.
//!    The sandbox lives for the transfer budget plus the deadline plus a short
//!    slack and is never extended. Every claim re-reads the sandbox and checks
//!    the controller tag and the admitted vCPU ceiling.
//! 4. **materialize** — the authorized snapshot is written into a unique
//!    per-attempt checkout, every file is read back and hashed, and the REAPI
//!    root `Directory` digest of what the sandbox holds must equal the pinned
//!    digest. Tracked edits, deletions and untracked inputs are exactly the
//!    snapshot: a deleted file is absent from the fresh checkout.
//! 5. **dispatch** — the record moves to [`AttemptState::Dispatching`] before
//!    the detached command starts, and the sandbox and command handles are
//!    persisted ([`AttemptState::Running`]) before a result is trusted. If that
//!    write fails the attempt reads back as indeterminate, never as success.
//!    An ambiguous start is never redispatched.
//! 6. **run** — logs are followed until the exit, the deadline, or the end of
//!    the stream (then the command API is asked for the exit). At the deadline
//!    the command is killed and the sandbox stopped and deleted, and the
//!    attempt reports [`SelectedCompletion::TimedOut`] with outputs fenced.
//! 7. **report** — cleanup is [`Cleanup::Done`] only when the sandbox delete
//!    was confirmed; otherwise it is [`Cleanup::Pending`] with the sandbox name
//!    and command handle in the record for the reaper
//!    ([`VercelSelectedRunner::reconcile`] and `thegn env reap`).
//!
//! Owner lease: the row carries the shared [`OwnerLease`] from the create step
//! on, and the running owner refreshes its heartbeat every 15 s by
//! compare-and-swap while it follows the logs and polls for the exit. Recovery
//! ([`VercelSelectedRunner::reconcile`], [`VercelSelectedRunner::cancel`])
//! refuses a row whose lease is live whatever its age; an owner whose heartbeat
//! loses the compare-and-swap fences and never overwrites the settled row.
//! Rows without a lease keep the age-only staleness rule.
//!
//! The ledger is a durable private file store
//! (`$XDG_STATE_HOME/thegn/vercel-jobs/<project>/`), shaped like the Sprites
//! one and sharing its state, cleanup and resume types.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::stream::{self, StreamExt, TryStreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thegn_cas::Digest;
use thegn_core::fsperm::{PrivateDir, ensure_private_dir, open_private_dir, owned_by_current_user};
use thegn_core::vercel::admit;
use thegn_core::vercel::api::LogLine;
use thegn_job_spec::selected::MAX_SELECTED_LOG_BYTES;
use thegn_job_spec::{
    Recipe, RunnerRefusal, SelectedCancel, SelectedCompletion, SelectedJobResult, SelectedRunner,
};

use super::VercelProvider;
use super::client::WAIT_COMMAND_DEADLINE;
use crate::provider::{ProviderFiles, RemoteProvider, STDERR_TAIL_CAP, join_remote};
pub use crate::sprites::job::{
    AttemptState, CancelToken, Cleanup, DirSnapshot, OwnerLease, ReconcileOutcome, Reconciled,
    Resume, SnapshotContent, SnapshotSource, root_digest,
};
use crate::sprites::job::{
    LEASE_HEARTBEAT, Tail, alias_is_valid, attempt_file, attempt_from_file, bound_log,
    checked_component, checked_rel_path, hex, millis_i64, nonce, signal_of, state_name,
};

/// The provider name written into every record and receipt.
pub const PROVIDER: &str = "vercel";

/// Root of the per-attempt checkouts in the sandbox (under its default cwd).
pub const DEFAULT_CHECKOUT_ROOT: &str = "/vercel/sandbox/.thegn/jobs";

/// Prefix of the job-spec alias a `runner:` selects (`vercel-<project>`).
pub const ALIAS_PREFIX: &str = "vercel-";

/// Longest wall clock an attempt may run; the route's deadline is clamped to it.
pub const DEFAULT_ATTEMPT_DEADLINE: Duration = Duration::from_secs(30 * 60);

/// Default budget for moving the snapshot into the sandbox. Separate from the
/// build deadline: a slow transfer never shortens the build.
pub const DEFAULT_TRANSFER_BUDGET: Duration = Duration::from_secs(15 * 60);

/// Default per-attempt spend cap, in micro-USD.
pub const DEFAULT_SPEND_CAP_MICROS: u64 = 60_000;

/// Sandbox lifetime beyond the transfer budget and the deadline: teardown room.
const SANDBOX_SLACK: Duration = Duration::from_secs(5 * 60);

/// The default ceiling on one sandbox session's lifetime: the longest session
/// Vercel documents for its paid plans (5 hours). The Hobby plan allows 45
/// minutes; an operator on it sets [`AttemptOptions::max_sandbox_timeout`]
/// accordingly. An attempt whose transfer budget, deadline and slack do not fit
/// the effective ceiling is refused before anything is created, rather than
/// letting the create request answer 4xx.
pub const PLAN_MAX_SANDBOX_TIMEOUT: Duration = Duration::from_secs(5 * 60 * 60);

/// Time a recovery pass leaves a non-terminal row alone beyond the owner's own
/// budgets: the kill and delete budget may still be running in a live owner.
const RECONCILE_SLACK: Duration = Duration::from_secs(60);

/// Extra margin on the staleness test. The ledger deadline is wall-clock but a
/// live owner times its stream on the monotonic clock, which stops while the
/// host is suspended; the margin only absorbs small skew. A suspend longer than
/// it can still make a live row look stale unless the owner lease in the row
/// (live heartbeat or live owner process) vouches for it.
const RECONCILE_CLOCK_MARGIN: Duration = Duration::from_secs(120);

/// Consecutive command-status errors tolerated while waiting for an exit.
const WAIT_ERROR_LIMIT: u32 = 3;

/// Files in flight at once while materializing.
const TRANSFER_CONCURRENCY: usize = 8;

/// Largest ledger record read back.
const RECORD_LIMIT: u64 = 256 * 1024;

const BINDING_FILE: &str = "binding.json";
const RESERVE_LOCK: &str = ".reserve.lock";

/// How long the kill, stop and delete calls together may take before cleanup is
/// reported pending.
const CLEANUP_BUDGET: Duration = Duration::from_secs(90);

const SIGKILL: i32 = 9;

/// Written to stderr by the launcher when it could not start the recipe, so a
/// launcher failure is never confused with a recipe that exits 125.
pub const LAUNCH_FAILED_MARKER: &str = "thegn-job-launch-failed";

/// The launcher the recipe runs behind. Positional arguments: the checkout, the
/// per-attempt `HOME`, then the recipe argv. It enters the checkout and `exec`s
/// the recipe under `env -i`, keeping only proxy and CA variables from the
/// sandbox environment, with a per-attempt `HOME`, `CARGO_HOME` and
/// `CARGO_TARGET_DIR`. When the recipe is `cargo` and the image has none, it
/// first installs a minimal rustup toolchain into the per-attempt home. Any failure to get that far prints
/// [`LAUNCH_FAILED_MARKER`] on stderr and exits 125.
pub const LAUNCH_SCRIPT: &str = r#"fail() { echo "thegn-job-launch-failed: $1" >&2; exit 125; }
cwd=$1
home=$2
shift 2
rustup_home=${RUSTUP_HOME:-${HOME:-}/.rustup}
[ -n "${PATH:-}" ] || fail path
mkdir -p -- "$home/tmp" "$home/cargo" "$home/target" || fail dirs
cd -- "$cwd" || fail cwd
if [ "$1" = cargo ] && ! command -v cargo >/dev/null 2>&1; then
    # The default sandbox image has no Rust toolchain: install a minimal one
    # into the per-attempt home before the recipe starts.
    command -v curl >/dev/null 2>&1 || fail "no-cargo-and-no-curl: the sandbox image has no Rust toolchain and cannot fetch one; use a runtime image that provides cargo"
    rustup_home="$home/rustup"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$home/tmp/rustup-init.sh" \
        || fail "rustup-download: the sandbox image has no Rust toolchain and rustup could not be downloaded"
    RUSTUP_HOME="$rustup_home" CARGO_HOME="$home/cargo" sh "$home/tmp/rustup-init.sh" -y \
        --profile minimal --default-toolchain stable --no-modify-path >&2 \
        || fail "rustup-install: installing a minimal Rust toolchain failed"
    PATH="$home/cargo/bin:$PATH"
    command -v cargo >/dev/null 2>&1 || fail "no-cargo: cargo is missing after the rustup install"
fi
for v in HTTP_PROXY HTTPS_PROXY NO_PROXY ALL_PROXY http_proxy https_proxy no_proxy all_proxy \
    SSL_CERT_FILE SSL_CERT_DIR NIX_SSL_CERT_FILE; do
    eval "val=\${$v:-}"
    [ -z "$val" ] || set -- "$v=$val" "$@"
done
exec env -i PATH="$PATH" HOME="$home" TMPDIR="$home/tmp" CARGO_HOME="$home/cargo" \
    CARGO_TARGET_DIR="$home/target" RUSTUP_HOME="$rustup_home" LANG=C.UTF-8 \
    THEGN_SELECTED_JOB=1 "$@"
"#;

/// Which project may use which Vercel team and project for selected Jobs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobBinding {
    project: String,
    team_id: String,
    project_id: String,
}

impl JobBinding {
    pub fn new(project: &str, team_id: &str, project_id: &str) -> Result<Self> {
        checked_component(project, "project", 64)?;
        ensure!(
            admit::valid_team_id(team_id),
            "vercel: the team id must look like team_<alphanumeric>"
        );
        ensure!(
            admit::valid_project_id(project_id),
            "vercel: the project id must look like prj_<alphanumeric>"
        );
        let binding = Self {
            project: project.to_owned(),
            team_id: team_id.to_owned(),
            project_id: project_id.to_owned(),
        };
        ensure!(
            alias_is_valid(&binding.alias()),
            "project {project:?} cannot be a selected runner: an alias is `{ALIAS_PREFIX}<project>` \
             and must be lowercase alphanumeric with interior dashes"
        );
        Ok(binding)
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// The alias a `runner:` selects for this binding.
    pub fn alias(&self) -> String {
        format!("{ALIAS_PREFIX}{}", self.project)
    }

    /// The per-attempt checkout: unique per attempt key.
    pub fn checkout(&self, key: &str, root: &str) -> String {
        format!("{}/co/{key}", root.trim_end_matches('/'))
    }

    /// The per-attempt `HOME` (never the sandbox user's).
    pub fn home(&self, key: &str, root: &str) -> String {
        format!("{}/home/{key}", root.trim_end_matches('/'))
    }

    /// The per-attempt sandbox name: `tg-job-<16 hex>`, derived from the project
    /// and the attempt key (attempt number plus nonce), so a ledger reset never
    /// reuses the name of an older, possibly still present sandbox.
    pub fn sandbox_name(&self, key: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.project.as_bytes());
        hasher.update([0]);
        hasher.update(key.as_bytes());
        format!("tg-job-{}", hex(&hasher.finalize().as_slice()[..8]))
    }
}

/// The team/project a ledger directory is pinned to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BindingRecord {
    pub project: String,
    pub team_id: String,
    pub project_id: String,
}

/// Durable record of one attempt: who ran, where, under which handles, how it
/// ended, what it cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptRecord {
    pub provider: String,
    pub project: String,
    pub team_id: String,
    pub project_id: String,
    pub attempt: u64,
    /// `<attempt>-<nonce>`: names the attempt's sandbox, checkout and home.
    pub key: String,
    pub recipe: Recipe,
    pub source: Digest,
    /// The sandbox name, set (with cleanup pending) before the create request.
    pub sandbox: String,
    pub checkout: String,
    pub state: AttemptState,
    /// The sandbox session the command ran in.
    pub session: Option<String>,
    /// The detached command id, persisted before success is trusted.
    pub command: Option<String>,
    pub reserved_micros: u64,
    pub spent_micros: Option<u64>,
    pub created_ms: i64,
    pub deadline_ms: i64,
    pub finished_ms: Option<i64>,
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    pub cleanup: Cleanup,
    /// The verified REAPI root digest of the materialized checkout.
    pub materialized: Option<Digest>,
    /// Declared output path to SHA-256 hex, when they were read.
    pub outputs: BTreeMap<String, String>,
    /// True when outputs were not read because no exit was proven.
    pub outputs_fenced: bool,
    /// True once the sandbox delete was confirmed.
    pub sandbox_deleted: bool,
    pub error: Option<String>,
    /// The running owner's lease; absent on rows written before leases existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<OwnerLease>,
}

impl AttemptRecord {
    /// What this attempt counts against the envelope: the reported spend, or the
    /// whole reserved cap while it has not reported.
    pub fn committed_micros(&self) -> u64 {
        self.spent_micros.unwrap_or(self.reserved_micros)
    }

    /// A terminal state means no further provider call can change the verdict.
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self.state,
            AttemptState::Reserved | AttemptState::Dispatching | AttemptState::Running
        )
    }

    /// What a later submission of this attempt number may do.
    pub fn resume(&self) -> Resume {
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

/// Why a reservation was refused. Nothing was sent to Vercel in any case.
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
    /// The ledger is pinned to another Vercel team or project.
    IdentityMismatch { pinned: String, requested: String },
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
            Self::IdentityMismatch { pinned, requested } => write!(
                f,
                "this ledger is pinned to Vercel {pinned}, not {requested}; refusing to claim"
            ),
            Self::Store(error) => write!(f, "attempt ledger: {error:#}"),
        }
    }
}

/// The requested reservation of one attempt.
pub struct ReserveRequest<'a> {
    /// An explicit attempt number, or `None` for the next free one.
    pub attempt: Option<u64>,
    pub recipe: Recipe,
    pub source: &'a Digest,
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

fn store_error<T>(result: Result<T>) -> Result<T, ReserveRefusal> {
    result.map_err(ReserveRefusal::Store)
}

/// Take the ledger's reservation lock without waiting: a second writer is a
/// retryable refusal, never an unbounded block.
fn lock_ledger(root: &PrivateDir) -> Result<std::fs::File> {
    let file = root
        .open_or_create_regular_nofollow(RESERVE_LOCK)
        .context("vercel job ledger: cannot open the ledger lock")?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("vercel job ledger: another attempt is reserving; retry")
        }
        Err(std::fs::TryLockError::Error(error)) => {
            Err(error).context("vercel job ledger: cannot take the ledger lock")
        }
    }
}

impl AttemptStore {
    /// `<state home>/thegn/vercel-jobs/<project>`.
    pub fn for_project(project: &str) -> Self {
        Self {
            dir: thegn_core::util::xdg_state_home()
                .join("thegn")
                .join("vercel-jobs")
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
                .context("vercel job ledger: not an owned private directory")?;
            return open_private_dir(&self.dir)
                .map(Some)
                .context("vercel job ledger: not an owned private directory");
        }
        let metadata = match std::fs::symlink_metadata(&self.dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("vercel job ledger: cannot stat the ledger"),
        };
        ensure!(
            !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && owned_by_current_user(&metadata),
            "vercel job ledger: {} is not an owned real directory",
            self.dir.display()
        );
        open_private_dir(&self.dir)
            .map(Some)
            .context("vercel job ledger: not an owned private directory")
    }

    fn read_record<T: DeserializeOwned>(&self, root: &PrivateDir, name: &str) -> Result<Option<T>> {
        let Some(bytes) = root
            .read_regular(name, RECORD_LIMIT)
            .with_context(|| format!("vercel job ledger: cannot read {name}"))?
        else {
            return Ok(None);
        };
        let record = serde_json::from_slice(&bytes)
            .with_context(|| format!("vercel job ledger: {name} is not a valid record"))?;
        Ok(Some(record))
    }

    /// The team/project this ledger directory is pinned to.
    pub fn binding(&self) -> Result<Option<BindingRecord>> {
        let Some(root) = self.open(false)? else {
            return Ok(None);
        };
        self.read_record(&root, BINDING_FILE)
    }

    /// Every attempt record, oldest first. An unreadable record is an error,
    /// not a skip.
    pub fn records(&self) -> Result<Vec<AttemptRecord>> {
        let Some(root) = self.open(false)? else {
            return Ok(Vec::new());
        };
        let mut records: Vec<AttemptRecord> = Vec::new();
        for name in root
            .file_names()
            .context("vercel job ledger: cannot list")?
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

    /// Publish a record atomically (tmp + fsync + rename).
    pub fn mark(&self, record: &AttemptRecord) -> Result<()> {
        let root = self
            .open(true)?
            .context("vercel job ledger: the ledger root vanished")?;
        let bytes = serde_json::to_vec_pretty(record).context("vercel job ledger: encode")?;
        root.write_owner_only_atomic(&attempt_file(record.attempt), &bytes)
            .with_context(|| {
                format!(
                    "vercel job ledger: cannot publish attempt {}",
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

    /// Confirm the sandbox of a cancelled or indeterminate attempt is gone.
    /// Idempotent; a record that is not pending is left alone. The re-read and
    /// the write happen under the ledger lock.
    pub fn mark_cleaned(&self, attempt: u64) -> Result<bool> {
        let Some(root) = self.open(false)? else {
            return Ok(false);
        };
        let _lock = lock_ledger(&root)?;
        let Some(mut record) = self.read_record::<AttemptRecord>(&root, &attempt_file(attempt))?
        else {
            return Ok(false);
        };
        if record.cleanup != Cleanup::Pending {
            return Ok(false);
        }
        record.cleanup = Cleanup::Done;
        record.sandbox_deleted = true;
        self.mark(&record)?;
        Ok(true)
    }

    /// What a later submission of `attempt` may do.
    pub fn resume_attempt(&self, attempt: u64) -> Result<Resume> {
        Ok(self
            .read(attempt)?
            .map_or(Resume::Fresh, |record| record.resume()))
    }

    /// Pin the ledger to `binding`'s team/project. `Some(existing)` is a
    /// different pin, which refuses.
    fn pin_binding(
        &self,
        root: &PrivateDir,
        binding: &JobBinding,
    ) -> Result<Option<BindingRecord>> {
        let intended = BindingRecord {
            project: binding.project().to_owned(),
            team_id: binding.team_id().to_owned(),
            project_id: binding.project_id().to_owned(),
        };
        match self.read_record::<BindingRecord>(root, BINDING_FILE)? {
            None => {
                let bytes =
                    serde_json::to_vec_pretty(&intended).context("vercel job ledger: encode")?;
                root.write_owner_only_atomic(BINDING_FILE, &bytes)
                    .context("vercel job ledger: cannot publish the binding")?;
                Ok(None)
            }
            Some(existing) if existing == intended => Ok(None),
            Some(existing) => Ok(Some(existing)),
        }
    }

    /// Count-and-reserve under the ledger lock. Everything that can refuse does
    /// so before any provider call.
    pub fn reserve(
        &self,
        binding: &JobBinding,
        request: ReserveRequest<'_>,
    ) -> Result<AttemptRecord, ReserveRefusal> {
        let root = store_error(self.open(true))?
            .ok_or_else(|| ReserveRefusal::Store(anyhow!("the ledger root vanished")))?;
        let _lock = store_error(lock_ledger(&root))?;
        if let Some(existing) = store_error(self.pin_binding(&root, binding))? {
            return Err(ReserveRefusal::IdentityMismatch {
                pinned: format!("{}/{}", existing.team_id, existing.project_id),
                requested: format!("{}/{}", binding.team_id(), binding.project_id()),
            });
        }
        let mut max = 0u64;
        let mut committed = 0u64;
        let mut duplicate = None;
        for name in store_error(root.file_names().context("vercel job ledger: cannot list"))? {
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some(number) = attempt_from_file(name) else {
                continue;
            };
            max = max.max(number);
            if let Some(record) = store_error(self.read_record::<AttemptRecord>(&root, name))? {
                committed = committed.saturating_add(record.committed_micros());
                if Some(number) == request.attempt {
                    duplicate = Some(record.state);
                }
            }
        }
        if let Some(state) = duplicate {
            return Err(ReserveRefusal::Duplicate(state));
        }
        let attempt = request.attempt.unwrap_or_else(|| max.saturating_add(1));
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
        let key = format!("{attempt}-{}", nonce());
        let record = AttemptRecord {
            provider: PROVIDER.to_owned(),
            project: binding.project().to_owned(),
            team_id: binding.team_id().to_owned(),
            project_id: binding.project_id().to_owned(),
            attempt,
            checkout: binding.checkout(&key, request.checkout_root),
            sandbox: String::new(),
            key,
            recipe: request.recipe,
            source: request.source.clone(),
            state: AttemptState::Reserved,
            session: None,
            command: None,
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
            sandbox_deleted: false,
            error: None,
            lease: None,
        };
        store_error(self.mark(&record))?;
        Ok(record)
    }
}

/// Fixed execution parameters for one binding.
#[derive(Debug, Clone)]
pub struct AttemptOptions {
    /// The longest wall clock an attempt may run; the route's deadline is
    /// clamped to this.
    pub deadline: Duration,
    /// Budget for moving the snapshot in; it never counts against the deadline.
    pub transfer_budget: Duration,
    /// Spend this attempt reserves, in micro-USD.
    pub spend_cap_micros: u64,
    /// Total the project may commit across attempts (0 = unlimited).
    pub spend_envelope_micros: u64,
    /// Configured compute price, in micro-USD per second (0 = unpriced).
    pub micros_per_second: u64,
    /// Declared outputs, relative to the checkout, hashed after a terminal exit.
    pub outputs: Vec<String>,
    /// Where per-attempt checkouts live in the sandbox.
    pub checkout_root: String,
    /// The longest sandbox session the Vercel plan allows
    /// ([`PLAN_MAX_SANDBOX_TIMEOUT`] by default); the effective ceiling is the
    /// smaller of this and the env's `max_lifetime_secs`.
    pub max_sandbox_timeout: Duration,
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
            outputs: Vec::new(),
            checkout_root: DEFAULT_CHECKOUT_ROOT.to_owned(),
            max_sandbox_timeout: PLAN_MAX_SANDBOX_TIMEOUT,
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
            !self.max_sandbox_timeout.is_zero(),
            "the sandbox timeout ceiling must be nonzero"
        );
        ensure!(
            self.checkout_root.starts_with('/')
                && !self.checkout_root.ends_with('/')
                && !self.checkout_root.contains(".."),
            "checkout root must be an absolute path without a trailing slash or traversal"
        );
        for output in &self.outputs {
            checked_rel_path(output)?;
        }
        Ok(())
    }

    /// The sandbox session lifetime for an attempt that may run for `deadline`.
    fn sandbox_timeout_ms(&self, deadline: Duration) -> u64 {
        let total = self
            .transfer_budget
            .saturating_add(deadline)
            .saturating_add(SANDBOX_SLACK);
        u64::try_from(total.as_millis()).unwrap_or(u64::MAX)
    }

    /// The longest deadline whose sandbox fits a lifetime `ceiling`; zero when
    /// the transfer budget and slack alone do not fit it.
    fn longest_deadline_within(&self, ceiling: Duration) -> Duration {
        ceiling.saturating_sub(self.transfer_budget.saturating_add(SANDBOX_SLACK))
    }
}

/// One attempt's outcome, including the accounting a report needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptReceipt {
    pub provider: String,
    pub project: String,
    pub team_id: String,
    pub project_id: String,
    pub sandbox: String,
    pub attempt: u64,
    pub checkout: String,
    pub source: Digest,
    pub materialized: Digest,
    pub session: Option<String>,
    pub command: Option<String>,
    pub completion: SelectedCompletion,
    pub exit: Option<i32>,
    pub signal: Option<i32>,
    pub cleanup: Cleanup,
    pub sandbox_deleted: bool,
    pub outputs: BTreeMap<String, String>,
    pub outputs_missing: Vec<String>,
    pub outputs_fenced: bool,
    pub files_transferred: usize,
    pub bytes_transferred: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub log_tail: String,
    /// Earlier output was discarded by this runner.
    pub truncated: bool,
    pub elapsed_ms: u64,
    pub cost_micros: Option<u64>,
    pub cost_basis: String,
}

/// What one materialization moved.
struct Transfer {
    digest: Digest,
    transferred: usize,
    bytes: u64,
}

/// `Recipe::argv` behind the launcher, as the sandbox receives it.
fn attempt_argv(recipe: Recipe, checkout: &str, home: &str) -> Vec<String> {
    let mut argv: Vec<String> = ["/bin/sh", "-c", LAUNCH_SCRIPT, "thegn-job", checkout, home]
        .iter()
        .map(|part| (*part).to_owned())
        .collect();
    argv.extend(recipe.argv().iter().map(|arg| (*arg).to_owned()));
    argv
}

/// The bounded log tail: stdout, then the stderr tail for a non-zero exit. The
/// flag says the included stderr tail already dropped earlier bytes.
fn compose_log(stdout: &str, stderr: &Tail, exit: Option<i32>) -> (String, bool) {
    let mut log = stdout.to_owned();
    if exit == Some(0) || stderr.total() == 0 {
        return (log, false);
    }
    let seen = stderr.total().min(STDERR_TAIL_CAP as u64);
    write!(log, "\n[stderr, last {seen} bytes]\n{}", stderr.render())
        .expect("writing to String cannot fail");
    (log, stderr.truncated())
}

/// Vercel-side executor for the typed selected-runner route.
///
/// One runner is bound to one project and one Vercel team/project. It holds a
/// runtime handle rather than a runtime so the [`SelectedRunner`] contract stays
/// synchronous: `submit` is called from the dispatcher's own thread and enters
/// the runtime itself (never call it from a runtime worker); in-runtime callers
/// use [`run`](Self::run).
pub struct VercelSelectedRunner {
    provider: VercelProvider,
    binding: JobBinding,
    store: AttemptStore,
    snapshot: Box<dyn SnapshotSource>,
    handle: tokio::runtime::Handle,
    options: AttemptOptions,
    alias: String,
}

impl VercelSelectedRunner {
    /// Bind a provider to a project and its attempt ledger.
    pub fn new(
        provider: VercelProvider,
        binding: JobBinding,
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
        provider: VercelProvider,
        binding: JobBinding,
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

    /// The provider's Vercel team and project must equal `team`/`project`. Every
    /// request the provider makes is scoped by that pair, so this is the check
    /// that a claim or recovery cannot reach another team's or project's
    /// sandbox.
    fn verify_scope(&self, team: &str, project: &str) -> Result<()> {
        let scope = &self.provider.spec.scope;
        ensure!(
            scope.team_id.trim() == team && scope.project_id.trim() == project,
            "vercel: the provider is bound to {}/{} but the attempt requires {team}/{project}; \
             refusing to claim",
            scope.team_id.trim(),
            scope.project_id.trim()
        );
        Ok(())
    }

    /// Verify identity, then that `name` is a sandbox this controller created and
    /// that it is within the admitted vCPU ceiling. Returns its running session.
    async fn claim(&self, team: &str, project: &str, name: &str) -> Result<String> {
        self.verify_scope(team, project)?;
        let view = self
            .provider
            .reaper_view(name)
            .await?
            .ok_or_else(|| anyhow!("vercel: sandbox {name} is not present"))?;
        let controller = self.provider.controller().await?;
        ensure!(
            super::is_ours(&view, &controller),
            "vercel: sandbox {name} does not carry this controller's tag"
        );
        ensure!(
            view.vcpus
                .is_none_or(|vcpus| vcpus <= self.provider.spec.vcpus),
            "vercel: sandbox {name} exceeds the admitted {} vCPU ceiling",
            self.provider.spec.vcpus
        );
        self.provider.session_for(name, super::Wake::Never).await
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

    /// The longest a job sandbox may live: the env's `max_lifetime_secs` (its
    /// default when 0) capped by the plan's session maximum.
    fn sandbox_ceiling(&self) -> Duration {
        let lifetime = Duration::from_secs(thegn_core::vercel::policy::max_lifetime_secs(
            self.provider.spec.max_lifetime_secs,
        ));
        lifetime.min(self.options.max_sandbox_timeout)
    }

    /// Run one attempt now, for at most `deadline` (clamped to the binding's
    /// supported maximum).
    ///
    /// An error here means the attempt was refused or closed *before any command
    /// could have run* (identity, ledger, create or transfer failure). The
    /// selected route reads every error from [`SelectedRunner::submit`] back as
    /// indeterminate regardless: the proof of "never ran" lives in this module's
    /// ledger (state `rejected`). Once dispatch began, an unproven outcome is a
    /// receipt with [`SelectedCompletion::Indeterminate`], never an error a
    /// caller might retry as though nothing had run.
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
    /// the command runs kills the command and stops the sandbox through the same
    /// path as the deadline, fences outputs and reports
    /// [`SelectedCompletion::Cancelled`] (never `TimedOut`); cleanup is `Done`
    /// on a confirmed teardown and `Pending` otherwise. A cancel that races an
    /// exit already queued reports the natural exit. A token cancelled before
    /// dispatch closes the attempt as rejected: no command was started.
    pub async fn run_cancellable(
        &self,
        recipe: Recipe,
        source: &Digest,
        content: &SnapshotContent,
        deadline: Duration,
        cancel: &CancelToken,
    ) -> Result<AttemptReceipt, RunnerRefusal> {
        // Identity first: no ledger write and no request for another team.
        self.verify_scope(self.binding.team_id(), self.binding.project_id())
            .map_err(|error| self.unavailable(error))?;
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
        // The sandbox lives for transfer + deadline + slack and is never extended:
        // refuse up front when that cannot fit the operator's lifetime ceiling or
        // the plan's session maximum, instead of letting the create answer 4xx.
        let timeout_ms = self.options.sandbox_timeout_ms(deadline);
        let ceiling = self.sandbox_ceiling();
        if Duration::from_millis(timeout_ms) > ceiling {
            let fitting = self.options.longest_deadline_within(ceiling);
            let remedy = if fitting.is_zero() {
                "the transfer budget and slack alone exceed it: raise max_lifetime_secs".to_owned()
            } else {
                format!(
                    "lower --deadline to {}s or less, or raise max_lifetime_secs",
                    fitting.as_secs()
                )
            };
            return Err(RunnerRefusal::InvalidSpec(format!(
                "the attempt needs a {}s sandbox (transfer budget {}s + deadline {}s + {}s \
                 slack) but the lifetime ceiling is {}s (the smaller of max_lifetime_secs and \
                 the plan's session maximum); {remedy}",
                timeout_ms / 1000,
                self.options.transfer_budget.as_secs(),
                deadline.as_secs(),
                SANDBOX_SLACK.as_secs(),
                ceiling.as_secs()
            )));
        }
        let begun = tokio::time::Instant::now();
        let mut record = self
            .reserve(recipe, source, deadline)
            .await
            .map_err(|refusal| self.unavailable(refusal))?;

        // The sandbox name is durable before the create request: an ambiguous
        // create can be found (and reaped) by name.
        let name = self.binding.sandbox_name(&record.key);
        record.lease = Some(OwnerLease::current(thegn_core::util::now_ms()));
        record.sandbox = name.clone();
        record.cleanup = Cleanup::Pending;
        if let Err(error) = self.persist(&record).await {
            return Err(self.reject(record, error, Cleanup::NotNeeded, None).await);
        }
        if let Err(error) = self.provider.create_named(&name, timeout_ms).await {
            let cleanup = if super::definite_create_rejection(&error) {
                Cleanup::NotNeeded
            } else {
                match self.provider.reaper_view(&name).await {
                    Ok(None) => Cleanup::NotNeeded,
                    Ok(Some(_)) | Err(_) => Cleanup::Pending,
                }
            };
            // A definite rejection or a sandbox that is not there never billed;
            // an ambiguous one keeps its cap committed (`Pending`).
            return Err(self.reject(record, error, cleanup, None).await);
        }
        let (team, project) = (self.binding.team_id(), self.binding.project_id());
        if let Err(error) = self.claim(team, project, &name).await {
            return Err(self.reject_and_teardown(record, error, begun).await);
        }

        let transferred = tokio::time::timeout(
            self.options.transfer_budget,
            self.materialize(&name, &record.checkout, content),
        )
        .await;
        let transfer = match transferred {
            Ok(Ok(transfer)) => transfer,
            Ok(Err(error)) => return Err(self.reject_and_teardown(record, error, begun).await),
            Err(_) => {
                let error = anyhow!(
                    "the transfer exceeded its {}s budget",
                    self.options.transfer_budget.as_secs()
                );
                return Err(self.reject_and_teardown(record, error, begun).await);
            }
        };
        if &transfer.digest != source {
            let error = anyhow!(
                "the materialized checkout digest {} does not match the pinned source {}",
                transfer.digest.hash(),
                source.hash()
            );
            return Err(self.reject_and_teardown(record, error, begun).await);
        }
        record.materialized = Some(transfer.digest.clone());

        // The command body carries no environment: the launcher builds the
        // recipe's (`env -i`), and nothing of this host's rides the request.
        let root = &self.options.checkout_root;
        let argv = attempt_argv(
            recipe,
            &record.checkout,
            &self.binding.home(&record.key, root),
        );
        let body = match self.provider.command_body(&argv, None, &[], false) {
            Ok(body) => body,
            Err(error) => return Err(self.reject_and_teardown(record, error, begun).await),
        };
        let session = match self.claim(team, project, &name).await {
            Ok(session) => session,
            Err(error) => return Err(self.reject_and_teardown(record, error, begun).await),
        };
        record.session = Some(session.clone());

        if cancel.is_cancelled() {
            let error = anyhow!("cancelled before dispatch");
            return Err(self.reject_and_teardown(record, error, begun).await);
        }

        // The fence. Written before the command starts: a crash from here on
        // reads back as an unknown dispatch and is never rerun blindly.
        record.state = AttemptState::Dispatching;
        if let Err(error) = self.persist(&record).await {
            return Err(self.reject_and_teardown(record, error, begun).await);
        }
        // The build clock starts at dispatch, after the transfer.
        let started = tokio::time::Instant::now();
        let command = match self.provider.api().run_detached(&session, &body).await {
            Ok(command) => command,
            Err(error)
                if super::definite_create_rejection(&error)
                    || VercelProvider::is_session_stopped(&error) =>
            {
                return Err(self.reject_and_teardown(record, error, begun).await);
            }
            Err(error) => {
                // A timeout or 5xx can follow an accepted start, so the command
                // may be running. Leave the outcome unknown and never redispatch.
                record.error = Some(format!("command start outcome unknown: {error:#}"));
                let empty = Tail::new(1);
                return Ok(self
                    .conclude(
                        record,
                        transfer,
                        begun,
                        SelectedCompletion::Indeterminate,
                        false,
                        (&empty, &empty),
                        None,
                    )
                    .await);
            }
        };
        record.command = Some(command.clone());
        record.state = AttemptState::Running;
        if let Err(error) = self.persist(&record).await {
            // The handles are not durable, so a success would be unreconcilable:
            // stop the command and report indeterminate.
            record.error = Some(format!("cannot persist the command handle: {error:#}"));
            let empty = Tail::new(1);
            return Ok(self
                .conclude(
                    record,
                    transfer,
                    begun,
                    SelectedCompletion::Indeterminate,
                    false,
                    (&empty, &empty),
                    None,
                )
                .await);
        }

        let until = started + deadline;
        let mut stdout = Tail::new(MAX_SELECTED_LOG_BYTES);
        let mut stderr = Tail::new(STDERR_TAIL_CAP);
        let mut exit = None;
        let mut timed_out = false;
        let mut cancelled = false;
        let mut fenced = false;
        match self.provider.api().logs(&session, &command).await {
            Ok(mut logs) => {
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
                        _ = beat.tick() => {
                            if !self.refresh_lease(&mut record).await {
                                fenced = true;
                                break;
                            }
                            continue;
                        }
                        next = tokio::time::timeout_at(until, logs.receiver_mut().recv()) => next,
                    };
                    match next {
                        Ok(Some(LogLine::Stdout(bytes))) => stdout.push(&bytes),
                        Ok(Some(LogLine::Stderr(bytes))) => stderr.push(&bytes),
                        Ok(Some(LogLine::Command {
                            exit_code: Some(code),
                            ..
                        })) => {
                            exit = Some(code);
                            break;
                        }
                        Ok(Some(LogLine::Command { .. })) => {}
                        Ok(Some(LogLine::Error { code, .. })) => {
                            record.error = Some(format!("the log stream reported {code:?}"));
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
                    // A line already queued when the cancel fired is not lost:
                    // when it is the exit, the attempt finished by itself.
                    while let Ok(line) = logs.receiver_mut().try_recv() {
                        match line {
                            LogLine::Stdout(bytes) => stdout.push(&bytes),
                            LogLine::Stderr(bytes) => stderr.push(&bytes),
                            LogLine::Command {
                                exit_code: Some(code),
                                ..
                            } => {
                                exit = Some(code);
                                cancelled = false;
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
            Err(error) => record.error = Some(format!("cannot follow the logs: {error:#}")),
        }
        // The stream ended without an exit (a dropped connection or an early
        // EOF says nothing about the build): poll the command API until the
        // exit arrives or the deadline passes, tolerating a few transient errors.
        if exit.is_none() && !timed_out && !cancelled && !fenced {
            let mut errors = 0u32;
            loop {
                let left = until.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    timed_out = true;
                    break;
                }
                // Each wait is capped at the heartbeat period so the lease keeps
                // beating while the exit is polled.
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
                if !self.refresh_lease(&mut record).await {
                    fenced = true;
                    break;
                }
                let waited = tokio::select! {
                    biased;
                    () = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    waited = self.provider.client.wait_command_exact(
                        &session,
                        &command,
                        left.min(WAIT_COMMAND_DEADLINE)
                            .min(self.options.lease_heartbeat),
                    ) => waited,
                };
                match waited {
                    Ok(Some(Some(code))) => {
                        exit = Some(code);
                        break;
                    }
                    Ok(Some(None)) => {
                        record.error =
                            Some("the command exit code is outside the 32-bit range".to_owned());
                        break;
                    }
                    Ok(None) => errors = 0,
                    Err(error) => {
                        errors += 1;
                        record.error = Some(format!("cannot read the command exit: {error:#}"));
                        if errors >= WAIT_ERROR_LIMIT {
                            break;
                        }
                        let left = until.saturating_duration_since(tokio::time::Instant::now());
                        tokio::time::sleep(left.min(Duration::from_secs(2))).await;
                    }
                }
            }
        }
        if timed_out {
            tracing::warn!(
                target: "thegn::vercel::job",
                attempt = record.attempt,
                deadline_secs = deadline.as_secs(),
                "attempt deadline reached; killing the command and stopping the sandbox"
            );
        }
        if cancelled {
            tracing::warn!(
                target: "thegn::vercel::job",
                attempt = record.attempt,
                "attempt cancelled; killing the command and stopping the sandbox"
            );
        }
        if fenced {
            tracing::warn!(
                target: "thegn::vercel::job",
                attempt = record.attempt,
                "the ledger row was settled by another process; this run is fenced"
            );
            record.error =
                Some("owner lease lost: the ledger row was settled by another process".to_owned());
        }
        let launch_failed = !timed_out
            && !cancelled
            && exit == Some(125)
            && stderr.render().contains(LAUNCH_FAILED_MARKER);
        let completion = match (timed_out, exit) {
            _ if cancelled => SelectedCompletion::Cancelled,
            (true, _) => SelectedCompletion::TimedOut,
            (false, Some(_)) if launch_failed => SelectedCompletion::Indeterminate,
            (false, Some(code)) => match signal_of(code) {
                Some(signal) => SelectedCompletion::Signalled(signal),
                None => SelectedCompletion::Exited(code),
            },
            (false, None) => SelectedCompletion::Indeterminate,
        };
        // A fenced run never overwrites the row whoever settled it wrote.
        let guard = fenced.then(|| record.clone());
        Ok(self
            .conclude(
                record,
                transfer,
                begun,
                completion,
                launch_failed,
                (&stdout, &stderr),
                guard.as_ref(),
            )
            .await)
    }

    /// Close a reservation that provably never ran a command.
    ///
    /// `billed_since` is when the attempt's sandbox was created, when it was:
    /// every Vercel attempt bills its own sandbox, so that elapsed time is
    /// charged at the configured rate. Unpriced, the whole cap stays committed
    /// so repeated failures still exhaust the envelope.
    async fn reject(
        &self,
        mut record: AttemptRecord,
        error: anyhow::Error,
        cleanup: Cleanup,
        billed_since: Option<tokio::time::Instant>,
    ) -> RunnerRefusal {
        record.state = AttemptState::Rejected;
        record.cleanup = cleanup;
        record.sandbox_deleted = cleanup == Cleanup::Done;
        // A sandbox that may still exist keeps its whole cap committed.
        record.spent_micros = if cleanup == Cleanup::Pending {
            None
        } else {
            match billed_since {
                None => Some(0),
                Some(since) => self.cost(since.elapsed()).0,
            }
        };
        record.outputs_fenced = true;
        record.finished_ms = Some(thegn_core::util::now_ms());
        record.lease = None;
        record.error = Some(format!("{error:#}"));
        if let Err(write_error) = self.persist(&record).await {
            tracing::warn!(
                target: "thegn::vercel::job",
                %write_error,
                attempt = record.attempt,
                "cannot record the rejected attempt"
            );
        }
        self.unavailable(error)
    }

    /// [`reject`](Self::reject) after tearing the sandbox down.
    async fn reject_and_teardown(
        &self,
        record: AttemptRecord,
        error: anyhow::Error,
        begun: tokio::time::Instant,
    ) -> RunnerRefusal {
        let cleanup = if self.teardown(&record.sandbox).await {
            Cleanup::Done
        } else {
            Cleanup::Pending
        };
        self.reject(record, error, cleanup, Some(begun)).await
    }

    /// Write the authorized bytes into the attempt checkout, read every file
    /// back and hash it, and return the REAPI root digest of what the sandbox
    /// holds. Files go through bounded parallel writes; each read-back is hashed
    /// and dropped, so memory holds the snapshot once.
    async fn materialize(
        &self,
        name: &str,
        checkout: &str,
        content: &SnapshotContent,
    ) -> Result<Transfer> {
        let provider = &self.provider;
        let verified: Vec<(&str, Digest, bool)> = stream::iter(content.files())
            .map(|(path, wanted)| async move {
                let remote = join_remote(checkout, path);
                let executable = content.is_executable(path);
                let expected = Digest::of(wanted);
                let written = if executable {
                    provider.write_exec(name, &remote, wanted).await
                } else {
                    provider.write(name, &remote, wanted).await
                };
                written.with_context(|| format!("vercel: cannot write {remote}"))?;
                let read_back = provider
                    .read(name, &remote)
                    .await
                    .with_context(|| format!("vercel: cannot verify {remote}"))?;
                let observed = Digest::of(&read_back);
                ensure!(
                    observed == expected,
                    "vercel: {remote} did not round-trip ({} bytes written, {} bytes read)",
                    wanted.len(),
                    read_back.len()
                );
                Ok::<_, anyhow::Error>((path, observed, executable))
            })
            .buffer_unordered(TRANSFER_CONCURRENCY)
            .try_collect()
            .await?;
        let transferred = verified.len();
        let digest = root_digest(verified)?;
        Ok(Transfer {
            digest,
            transferred,
            bytes: content.total_bytes(),
        })
    }

    /// Read and hash the declared outputs. Only ever called for an attempt that
    /// proved a terminal exit.
    async fn hash_outputs(
        &self,
        name: &str,
        checkout: &str,
    ) -> (BTreeMap<String, String>, Vec<String>) {
        let mut outputs = BTreeMap::new();
        let mut missing = Vec::new();
        for path in &self.options.outputs {
            match self.provider.read(name, &join_remote(checkout, path)).await {
                Ok(bytes) => {
                    outputs.insert(path.clone(), hex(Sha256::digest(&bytes)));
                }
                Err(_) => missing.push(path.clone()),
            }
        }
        (outputs, missing)
    }

    /// Kill the recorded command (best effort; the teardown that follows is what
    /// confirms cleanup). True when the kill was accepted.
    async fn kill_command(&self, record: &AttemptRecord) -> bool {
        let (Some(session), Some(command)) = (record.session.as_deref(), record.command.as_deref())
        else {
            return false;
        };
        match tokio::time::timeout(
            CLEANUP_BUDGET,
            self.provider.client.kill(session, command, SIGKILL),
        )
        .await
        {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "thegn::vercel::job",
                    %error,
                    attempt = record.attempt,
                    "cannot kill the owned command"
                );
                false
            }
            Err(_) => false,
        }
    }

    /// Stop the sandbox session and delete the sandbox. True only when the
    /// delete is confirmed, so cleanup is `Done` only for a sandbox that is gone.
    async fn teardown(&self, name: &str) -> bool {
        if name.is_empty() {
            return true;
        }
        let work = async {
            if let Err(error) = self.provider.stop_session(name).await {
                tracing::warn!(
                    target: "thegn::vercel::job",
                    %error,
                    sandbox = name,
                    "cannot stop the sandbox session before deleting it"
                );
            }
            self.provider.destroy(name).await
        };
        match tokio::time::timeout(CLEANUP_BUDGET, work).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::warn!(
                    target: "thegn::vercel::job",
                    %error,
                    sandbox = name,
                    "sandbox cleanup incomplete; it stays pending for the reaper"
                );
                false
            }
            Err(_) => false,
        }
    }

    /// Record the terminal state, kill what may still run, hash what may be
    /// hashed, delete the sandbox, and build the receipt. Every exit from
    /// [`run`](Self::run) after the dispatch fence goes through here, so no path
    /// can forget the ledger.
    #[expect(
        clippy::too_many_arguments,
        reason = "the single terminal path takes the whole attempt context; bundling it would only rename these"
    )]
    async fn conclude(
        &self,
        mut record: AttemptRecord,
        transfer: Transfer,
        begun: tokio::time::Instant,
        completion: SelectedCompletion,
        launch_failed: bool,
        (stdout, stderr): (&Tail, &Tail),
        guard: Option<&AttemptRecord>,
    ) -> AttemptReceipt {
        let proven = matches!(
            completion,
            SelectedCompletion::Exited(_) | SelectedCompletion::Signalled(_)
        );
        record.outputs_fenced = !matches!(completion, SelectedCompletion::Exited(_));
        let (outputs, outputs_missing) = if record.outputs_fenced {
            (BTreeMap::new(), Vec::new())
        } else {
            self.hash_outputs(&record.sandbox, &record.checkout).await
        };
        // Anything short of a proven exit may still be running: stop it now
        // rather than letting it bill until the sandbox timeout.
        if !proven && !launch_failed {
            self.kill_command(&record).await;
        }
        let deleted = self.teardown(&record.sandbox).await;
        record.cleanup = if deleted {
            Cleanup::Done
        } else {
            Cleanup::Pending
        };
        record.sandbox_deleted = deleted;
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
        // A sandbox that may still bill keeps its whole cap committed.
        record.spent_micros = if deleted { cost_micros } else { None };
        record.outputs = outputs.clone();
        let written = match guard {
            None => self.persist(&record).await.map(|()| true),
            Some(expected) => self.persist_if_unchanged(expected, &record).await,
        };
        match written {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                target: "thegn::vercel::job",
                attempt = record.attempt,
                "the ledger row changed under this run; its newer state is kept"
            ),
            Err(error) => tracing::warn!(
                target: "thegn::vercel::job",
                %error,
                attempt = record.attempt,
                "cannot record the attempt outcome"
            ),
        }
        let stdout_tail = stdout.render();
        let stderr_tail = stderr.render();
        let (composed, stderr_truncated) = compose_log(&stdout_tail, stderr, record.exit);
        let (log_tail, trimmed) = bound_log(composed);
        AttemptReceipt {
            provider: PROVIDER.to_owned(),
            project: record.project.clone(),
            team_id: record.team_id.clone(),
            project_id: record.project_id.clone(),
            sandbox: record.sandbox.clone(),
            attempt: record.attempt,
            checkout: record.checkout.clone(),
            source: record.source.clone(),
            materialized: transfer.digest,
            session: record.session.clone(),
            command: record.command.clone(),
            completion,
            exit: record.exit,
            signal: record.signal,
            cleanup: record.cleanup,
            sandbox_deleted: record.sandbox_deleted,
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
        // `{:#}` keeps the cause chain: a refusal is the operator's only clue.
        RunnerRefusal::RunnerUnavailable(format!("{}: {error:#}", self.alias))
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

    async fn read_attempt(&self, attempt: u64) -> Result<Option<AttemptRecord>> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.read(attempt))
            .await
            .map_err(|error| anyhow!("ledger task failed: {error}"))?
    }

    /// The age past which a non-terminal row can no longer belong to a live run:
    /// its deadline plus the transfer budget, kill budget, slack and clock margin.
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
    /// run last wrote. False means a recovery settled the row: the owner must
    /// stop trusting its own state. A failed write is retried on the next tick.
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
                    target: "thegn::vercel::job",
                    %error,
                    attempt = record.attempt,
                    "cannot refresh the owner lease heartbeat"
                );
                true
            }
        }
    }

    /// Cancel the owned work of `attempt` from outside the run that owns it:
    /// verify the recorded team/project, kill the command, stop and delete the
    /// sandbox, and record the result. A terminal attempt keeps its verdict (only
    /// its cleanup is settled); a live one becomes [`AttemptState::Cancelled`].
    /// Subject to the same rule as [`reconcile`](Self::reconcile): a row whose
    /// owner lease is live, or that is younger than its staleness age, is
    /// refused rather than killed under a run that may still be driving it.
    pub async fn cancel(&self, attempt: u64) -> Result<AttemptRecord> {
        Ok(self.settle_outcome(attempt, true).await?.0)
    }

    /// Reconcile an attempt left ambiguous or with cleanup pending (a crashed
    /// client, an ambiguous create or start, a failed delete): verify the
    /// recorded team/project and ownership, settle the sandbox, and never rerun.
    /// A non-terminal row becomes [`AttemptState::Indeterminate`] only once it is
    /// stale (deadline plus transfer budget, kill budget, slack and a clock
    /// margin have passed); a younger one is refused. Every write is
    /// compare-and-swap under the ledger lock: a row its owner or another
    /// recovery changed in the meantime is left as it is (superseded) and
    /// returned as found, so a terminal `Finished` record is never overwritten.
    pub async fn reconcile(&self, attempt: u64) -> Result<AttemptRecord> {
        Ok(self.settle_outcome(attempt, false).await?.0)
    }

    /// [`reconcile`](Self::reconcile) every attempt of this binding that a crash
    /// left unsettled: terminal rows whose cleanup is still pending, and
    /// non-terminal rows that are stale (a live owner lease or a younger row is
    /// skipped here rather than refused). Never dispatches. A row whose
    /// reconcile fails is logged and reported as still pending, so one broken
    /// row never hides the others.
    pub async fn reconcile_pending(&self) -> Result<Vec<Reconciled>> {
        self.reconcile_pending_at(thegn_core::util::now_ms()).await
    }

    /// One [`reconcile_pending`](Self::reconcile_pending) pass at `now_ms`.
    pub async fn reconcile_pending_at(&self, now_ms: i64) -> Result<Vec<Reconciled>> {
        let store = self.store.clone();
        let records = tokio::task::spawn_blocking(move || store.records())
            .await
            .map_err(|error| anyhow!("ledger task failed: {error}"))??;
        let mut touched = Vec::new();
        for record in records {
            if record.project != self.binding.project()
                || record.team_id != self.binding.team_id()
                || record.project_id != self.binding.project_id()
            {
                continue;
            }
            if record.is_terminal() {
                if record.cleanup != Cleanup::Pending {
                    continue;
                }
            } else if record.state == AttemptState::Reserved || !self.is_stale(&record, now_ms) {
                continue;
            }
            let attempt = record.attempt;
            let outcome = match self.settle_outcome(attempt, false).await {
                Ok((_, true)) => ReconcileOutcome::Superseded,
                Ok((settled, false)) if settled.cleanup == Cleanup::Pending => {
                    ReconcileOutcome::StillPending
                }
                Ok((_, false)) => ReconcileOutcome::Cleaned,
                Err(error) => {
                    tracing::warn!(
                        target: "thegn::vercel::job",
                        attempt,
                        %error,
                        "cannot reconcile the pending attempt"
                    );
                    ReconcileOutcome::StillPending
                }
            };
            touched.push(Reconciled { attempt, outcome });
        }
        Ok(touched)
    }

    /// Settle `attempt` and report whether the compare-and-swap was lost, so a
    /// caller that scanned the row (the pending pass) can report the outcome as
    /// superseded rather than cleaned. The flag is also set when the row is
    /// already settled on arrival: the scan found it outstanding, so a newer
    /// state was kept. Callers that only return the record discard it.
    async fn settle_outcome(&self, attempt: u64, cancel: bool) -> Result<(AttemptRecord, bool)> {
        let record = self
            .read_attempt(attempt)
            .await?
            .ok_or_else(|| anyhow!("vercel job ledger: attempt {attempt} is not recorded"))?;
        // Recovery checks the recorded identity before any provider call.
        self.verify_scope(&record.team_id, &record.project_id)?;
        if record.is_terminal() && record.cleanup != Cleanup::Pending {
            return Ok((record, true));
        }
        if !record.is_terminal() && !self.is_stale(&record, thegn_core::util::now_ms()) {
            bail!(
                "vercel job ledger: attempt {attempt} is still {} and may belong to a live run; \
                 refusing to kill it before it is stale",
                state_name(record.state)
            );
        }
        let expected = record.clone();
        let mut record = record;
        let name = record.sandbox.clone();
        let gone = if name.is_empty() {
            true
        } else {
            match self.provider.reaper_view(&name).await? {
                None => true,
                Some(view) => {
                    let controller = self.provider.controller().await?;
                    ensure!(
                        super::is_ours(&view, &controller),
                        "vercel: sandbox {name} does not carry this controller's tag; \
                         refusing to touch it"
                    );
                    // Re-read right before the kill: a row its owner moved on is
                    // never killed.
                    if self.read_attempt(attempt).await?.as_ref() != Some(&expected) {
                        return Ok((self.superseded(attempt).await?, true));
                    }
                    self.kill_command(&record).await;
                    self.teardown(&name).await
                }
            }
        };
        if !record.is_terminal() {
            record.state = if cancel {
                AttemptState::Cancelled
            } else {
                AttemptState::Indeterminate
            };
            record.outputs_fenced = true;
            record.finished_ms = Some(thegn_core::util::now_ms());
            record.lease = None;
        }
        record.cleanup = if gone {
            Cleanup::Done
        } else {
            Cleanup::Pending
        };
        record.sandbox_deleted = gone;
        if self.persist_if_unchanged(&expected, &record).await? {
            Ok((record, false))
        } else {
            Ok((self.superseded(attempt).await?, true))
        }
    }

    /// The row as it stands after a recovery pass lost the compare-and-swap.
    async fn superseded(&self, attempt: u64) -> Result<AttemptRecord> {
        tracing::warn!(
            target: "thegn::vercel::job",
            attempt,
            "the ledger row changed during recovery; its newer state is kept"
        );
        self.read_attempt(attempt)
            .await?
            .ok_or_else(|| anyhow!("vercel job ledger: attempt {attempt} vanished"))
    }
}

impl SelectedRunner for VercelSelectedRunner {
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
            // rejected; the route reads it back as a user cancel.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_scrubs_the_environment_and_marks_its_own_failures() {
        assert!(LAUNCH_SCRIPT.contains("env -i"));
        assert!(LAUNCH_SCRIPT.contains(LAUNCH_FAILED_MARKER));
        assert!(!LAUNCH_SCRIPT.contains("VERCEL"));
        assert!(!LAUNCH_SCRIPT.contains(".claude"));
        assert!(LAUNCH_SCRIPT.contains(crate::sprites::job::SELECTED_JOB_ENV));
    }

    #[test]
    fn the_launcher_provisions_a_minimal_toolchain_when_cargo_is_missing() {
        assert!(LAUNCH_SCRIPT.contains("sh.rustup.rs"));
        assert!(LAUNCH_SCRIPT.contains("--profile minimal"));
        assert!(LAUNCH_SCRIPT.contains("no-cargo"));
    }

    #[test]
    fn attempt_argv_runs_only_the_recipe_behind_the_launcher() {
        let argv = attempt_argv(Recipe::CargoTest, "/r/co/k", "/r/home/k");
        assert_eq!(argv[..2], ["/bin/sh", "-c"]);
        assert_eq!(argv[2], LAUNCH_SCRIPT);
        assert_eq!(argv[4..6], ["/r/co/k", "/r/home/k"]);
        assert_eq!(argv[6..], ["cargo", "test"]);
    }

    #[test]
    fn binding_names_are_valid_and_unique_per_attempt_key() {
        let binding = JobBinding::new("proj", "team_abc1", "prj_abc1").unwrap();
        assert_eq!(binding.alias(), "vercel-proj");
        let name = binding.sandbox_name("1-aaaa");
        assert!(admit::valid_sandbox_name(&name));
        assert_ne!(name, binding.sandbox_name("1-bbbb"));
        assert_ne!(
            binding.checkout("1-aaaa", DEFAULT_CHECKOUT_ROOT),
            binding.checkout("1-bbbb", DEFAULT_CHECKOUT_ROOT)
        );
        assert!(JobBinding::new("Proj", "team_abc1", "prj_abc1").is_err());
        assert!(JobBinding::new("proj", "abc", "prj_abc1").is_err());
        assert!(JobBinding::new("proj", "team_abc1", "abc").is_err());
    }

    #[test]
    fn a_finished_record_without_an_exit_is_indeterminate_never_exit_one() {
        let mut record = AttemptRecord {
            provider: PROVIDER.to_owned(),
            project: "p".to_owned(),
            team_id: "team_a".to_owned(),
            project_id: "prj_a".to_owned(),
            attempt: 1,
            key: "1-aaaa".to_owned(),
            recipe: Recipe::CargoBuild,
            source: Digest::of(b"x"),
            sandbox: "tg-job-0".to_owned(),
            checkout: "/c".to_owned(),
            state: AttemptState::Finished,
            session: None,
            command: None,
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
            sandbox_deleted: false,
            error: None,
            lease: None,
        };
        assert_eq!(
            record.resume(),
            Resume::Terminal(SelectedCompletion::Indeterminate)
        );
        record.exit = Some(3);
        assert_eq!(
            record.resume(),
            Resume::Terminal(SelectedCompletion::Exited(3))
        );
        record.state = AttemptState::Dispatching;
        assert_eq!(record.resume(), Resume::UnknownDispatch);
    }
}
