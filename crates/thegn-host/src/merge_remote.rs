//! Cross-host branch ingest for the merge queue: make a queued branch's tip
//! present in the **target** repo's object store when the branch's worktree
//! lives on a different machine, so the object-DB fold can merge it.
//!
//! The merge queue folds every branch into one target store (where the target
//! branch lives). A same-host branch already shares that store — git worktrees
//! share a single `.git` — so its tip OID is already present. A branch whose
//! worktree is on **another host** (ssh/provider) has its *own* object store, so
//! its tip is absent from the target and the fold would fail on an unknown OID.
//! We bridge that with a git **bundle**: create a bundle of the branch ref on the
//! branch's host, stream it to the target host, and `git fetch` it into a
//! synthetic ref scoped to the source row and ingestion generation. That ref
//! is what the fold merges and an attempt-scoped guard removes it afterward.
//!
//! Runs off the event loop (from the merge-queue drain in `spawn_blocking` / the
//! CLI). Producer pipes are drained off-loop and the consumer uses the bounded
//! Git executor.

use crate::platform::bundle_custody::{BundleTemp, kill_bundle_producer, spawn_bundle_producer};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thegn_core::remote::GitLoc;
use thegn_svc::git::{CliGit, PlumbingOps};

const MAX_BUNDLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const BUNDLE_FREE_SPACE_MARGIN: u64 = 256 * 1024 * 1024;
const BUNDLE_STDERR_TAIL: usize = 64 * 1024;
const BUNDLE_READER_GRACE: Duration = Duration::from_millis(100);

thread_local! {
    /// The merge-claim generation of the drain attempt running on this thread (None outside a drain attempt).
    static INGEST_CLAIM_GENERATION: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// Marks the current thread as running a merge-queue attempt under claim `generation` until dropped. Nested scopes restore the
/// previous value.
pub(crate) struct IngestClaimScope {
    previous: Option<i64>,
}

impl IngestClaimScope {
    pub(crate) fn enter(generation: i64) -> Self {
        let previous = INGEST_CLAIM_GENERATION.with(|cell| cell.replace(Some(generation)));
        Self { previous }
    }
}

impl Drop for IngestClaimScope {
    fn drop(&mut self) {
        INGEST_CLAIM_GENERATION.with(|cell| cell.set(self.previous));
    }
}

/// The claim generation of the drain attempt on this thread, if any.
pub(crate) fn current_ingest_claim_generation() -> Option<i64> {
    INGEST_CLAIM_GENERATION.with(std::cell::Cell::get)
}

/// The 32-bit generation field of an ingest ref name: the claim generation clamped into `0..=u32::MAX`, `0` outside a drain.
fn generation_field(generation: Option<i64>) -> u32 {
    let clamped = generation.unwrap_or(0).clamp(0, i64::from(u32::MAX));
    u32::try_from(clamped).unwrap_or(u32::MAX)
}

/// The generation field of `ref_name` when it is `<prefix><16 lower hex>`; None for any other shape.
fn ref_generation_field(ref_name: &str, prefix: &str) -> Option<u32> {
    let tail = ref_name.strip_prefix(prefix)?;
    if !is_lower_hex_16(tail) {
        return None;
    }
    u32::from_str_radix(&tail[..8], 16).ok()
}

struct BundleLimits {
    max_bytes: u64,
    /// Test injection; production measures the filesystem containing custody.
    free_bytes: Option<u64>,
    deadline: Duration,
    stderr_tail: usize,
}

impl BundleLimits {
    fn production() -> Self {
        Self {
            max_bytes: MAX_BUNDLE_BYTES,
            free_bytes: None,
            deadline: crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
            stderr_tail: BUNDLE_STDERR_TAIL,
        }
    }
}

/// Reserve `margin` bytes on the custody filesystem and cap the bundle at the
/// smaller of that remaining space and the configured limit.
fn effective_bundle_cap(max: u64, free: u64, margin: u64) -> Option<u64> {
    if free <= margin {
        return None;
    }
    Some(max.min(free - margin))
}

#[derive(Debug)]
enum IngestError {
    Cancelled,
    Timeout {
        after: Duration,
    },
    SizeLimit {
        cap: u64,
    },
    Remote {
        code: Option<i32>,
        stderr_tail: String,
    },
    LocalStorage(io::Error),
    InvalidBundle(String),
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("bundle ingestion cancelled"),
            Self::Timeout { after } => write!(
                f,
                "bundle ingestion timed out after {} seconds",
                after.as_secs()
            ),
            Self::SizeLimit { cap } => write!(f, "bundle exceeded the {cap}-byte size limit"),
            Self::Remote { code, stderr_tail } => write!(
                f,
                "bundle producer failed with status {code:?}: {stderr_tail}"
            ),
            Self::LocalStorage(error) => write!(f, "bundle local storage failed: {error}"),
            Self::InvalidBundle(detail) => write!(f, "invalid bundle: {detail}"),
        }
    }
}

impl std::error::Error for IngestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LocalStorage(error) => Some(error),
            _ => None,
        }
    }
}

/// The fenced source tip used by the fold. The ingested source ref stays owned
/// by this guard for the full integration attempt. Same-store sources have no
/// synthetic ref and no guard.
pub struct IngestedTip {
    /// Ref the fold reads from the target object store.
    pub ref_name: String,
    /// Captured remote source OID; absent for same-store branches.
    pub oid: Option<String>,
    _guard: Option<IngestRef>,
}

struct IngestRef {
    target: GitLoc,
    ref_name: String,
    oid: String,
}

impl Drop for IngestRef {
    fn drop(&mut self) {
        // best-effort: the attempt must still return its actual disposition.
        if let Err(error) = ref_update(
            &self.target,
            &["update-ref", "-d", &self.ref_name, &self.oid],
        ) {
            tracing::warn!(ref_name = %self.ref_name, error = %error, "failed to remove merge-queue ingest ref");
        }
    }
}

/// Ensure `branch`'s tip is available in the target store. Cross-host refs are
/// unique per source authority, worktree, claim generation and random nonce.
pub fn ingest_tip(target: &GitLoc, branch: &str, branch_loc: &GitLoc) -> Result<IngestedTip> {
    if !needs_ingest(target, branch_loc) {
        return Ok(IngestedTip {
            ref_name: format!("refs/heads/{branch}"),
            oid: None,
            _guard: None,
        });
    }
    ingest_tip_with_expected(
        target,
        branch,
        branch_loc,
        None,
        &BundleLimits::production(),
    )
}

fn ingest_tip_with_expected(
    target: &GitLoc,
    branch: &str,
    branch_loc: &GitLoc,
    injected_expected: Option<&str>,
    limits: &BundleLimits,
) -> Result<IngestedTip> {
    if target.is_remote() {
        anyhow::bail!(
            "remote target store: run the drain on the target host (merge daemon), \
             not by pushing a bundle over ssh"
        );
    }
    let expected = match injected_expected {
        Some(oid) => oid.to_string(),
        None => CliGit.rev_parse(branch_loc, &format!("refs/heads/{branch}"))?,
    };
    prune_refs(target, branch_loc, branch)?;
    let ref_name = generation_ref(branch_loc, branch)?;
    let mut tmp = BundleTemp::new().map_err(IngestError::LocalStorage)?;
    let src = format!("refs/heads/{branch}");
    if let Err(error) = stream_bundle(
        branch_loc.git_command(&["bundle", "create", "-", &src]),
        &mut tmp,
        limits,
    ) {
        let cleanup = tmp.finish();
        let context = if matches!(&error, IngestError::Cancelled) {
            format!("bundle ingestion of {branch} cancelled")
        } else {
            format!("branch host unreachable while bundling {branch}")
        };
        return match cleanup {
            Ok(()) => Err(anyhow::Error::new(error).context(context)),
            Err(cleanup) => {
                Err(anyhow::Error::new(error)
                    .context(format!("{context}; cleanup failed: {cleanup}")))
            }
        };
    }
    if crate::fold_cancel::requested() {
        return match tmp.finish() {
            Ok(()) => Err(anyhow::Error::new(IngestError::Cancelled)),
            Err(cleanup) => Err(anyhow::Error::new(IngestError::Cancelled).context(format!(
                "bundle ingestion cancelled; cleanup failed: {cleanup}"
            ))),
        };
    }
    fetch_from_custody(target, branch, tmp, &ref_name, &expected, limits.deadline)
        .with_context(|| format!("fetching {branch} into the target store"))?;
    let guard = IngestRef {
        target: target.clone(),
        ref_name: ref_name.clone(),
        oid: expected.clone(),
    };
    Ok(IngestedTip {
        ref_name: ref_name.clone(),
        oid: Some(expected.clone()),
        _guard: Some(guard),
    })
}

fn generation_ref(branch_loc: &GitLoc, branch: &str) -> Result<String> {
    let row = row_key(branch_loc, branch);
    let field = generation_field(current_ingest_claim_generation());
    // A 32-bit nonce makes a same-row, same-generation collision about 1 in 4 billion
    // while a prior ref is still live; the generation field separates different claims.
    let mut nonce = [0_u8; 4];
    getrandom::fill(&mut nonce).context("mint merge-queue ingest generation")?;
    Ok(format!(
        "refs/thegn/mq/{row}/{field:08x}{}",
        hex_prefix(&nonce)
    ))
}

fn row_key(branch_loc: &GitLoc, branch: &str) -> String {
    let mut row_hash = Sha256::new();
    row_hash.update(host_id(branch_loc).as_bytes());
    row_hash.update([0]);
    row_hash.update(loc_path(branch_loc).as_bytes());
    row_hash.update([0]);
    row_hash.update(branch.as_bytes());
    hex_prefix(&row_hash.finalize()[..8])
}

fn hex_prefix(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn loc_path(loc: &GitLoc) -> String {
    match loc {
        GitLoc::Local(path) => path.to_string_lossy().into_owned(),
        GitLoc::Remote { path, .. } | GitLoc::Provider { path, .. } => path.clone(),
    }
}

fn prune_refs(target: &GitLoc, branch_loc: &GitLoc, branch: &str) -> Result<()> {
    const PRUNE_LIMIT: usize = 64;
    let prefix = format!("refs/thegn/mq/{}/", row_key(branch_loc, branch));
    let mine: Option<u32> =
        current_ingest_claim_generation().map(|generation| generation_field(Some(generation)));
    let refs = match enumerate_stale_refs(target, &prefix) {
        Ok(refs) => refs,
        Err(error) => {
            tracing::warn!(ref_prefix = %prefix, %error, "failed to enumerate stale merge-queue ingest refs");
            Vec::new()
        }
    };
    if refs.len() == PRUNE_LIMIT {
        tracing::warn!(
            ref_prefix = %prefix,
            count = refs.len(),
            "merge-queue ingest ref prune reached its per-attempt limit"
        );
    }
    for ref_name in refs {
        if let Some(mine) = mine
            && ref_generation_field(&ref_name, &prefix).is_some_and(|theirs| theirs > mine)
        {
            // A claim-generation reset can leave a higher-generation ref behind; a scoped
            // drain preserves it, and the orphan sweep reaps it only after the row is gone.
            tracing::debug!(ref_name = %ref_name, "kept the merge-queue ingest ref of a newer claim generation");
            continue;
        }
        if let Err(error) = ref_update(target, &["update-ref", "-d", &ref_name]) {
            tracing::warn!(ref_name = %ref_name, error = %error, "failed to prune stale merge-queue ingest ref");
        }
    }
    let legacy = format!("refs/thegn/mq/{branch}");
    match ref_oid(target, &legacy) {
        Ok(Some(_)) => {
            if let Err(error) = ref_update(target, &["update-ref", "-d", &legacy]) {
                tracing::warn!(ref_name = %legacy, error = %error, "failed to prune legacy merge-queue ingest ref");
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(ref_name = %legacy, error = %error, "failed to read legacy merge-queue ingest ref before pruning");
        }
    }
    Ok(())
}

fn ref_update(target: &GitLoc, args: &[&str]) -> std::result::Result<(), String> {
    let outcome = target.git_run(
        args,
        &thegn_core::git_run::GitPolicy::read(crate::integrate::DRIVER_SUBPROCESS_TIMEOUT),
        &AtomicBool::new(false),
    );
    if outcome.is_success() {
        Ok(())
    } else {
        Err(outcome.to_string())
    }
}

fn ref_oid(target: &GitLoc, name: &str) -> std::result::Result<Option<String>, String> {
    // Both callers reject remote targets before this local-only -q argv is used.
    let outcome = target.git_run(
        &["rev-parse", "-q", "--verify", name],
        &thegn_core::git_run::GitPolicy::read(crate::integrate::DRIVER_SUBPROCESS_TIMEOUT),
        &AtomicBool::new(false),
    );
    if outcome.is_success() {
        let oid = outcome.into_text().map_err(|error| error.to_string())?;
        let oid = oid.trim();
        if oid.is_empty() {
            return Err("git rev-parse returned an empty ref OID".to_string());
        }
        return Ok(Some(oid.to_string()));
    }
    if matches!(
        &outcome,
        thegn_core::git_run::GitOutcome::Exit { code: Some(1), .. }
    ) {
        return Ok(None);
    }
    Err(outcome.to_string())
}

fn parse_ref_listing(stdout: &[u8]) -> Result<Vec<String>> {
    Ok(String::from_utf8_lossy(stdout)
        .lines()
        .map(str::to_string)
        .collect())
}

fn enumerate_stale_refs(target: &GitLoc, prefix: &str) -> Result<Vec<String>> {
    let outcome = target.git_run(
        &["for-each-ref", "--count=64", "--format=%(refname)", prefix],
        &thegn_core::git_run::GitPolicy::read(crate::integrate::DRIVER_SUBPROCESS_TIMEOUT),
        &AtomicBool::new(false),
    );
    match outcome {
        thegn_core::git_run::GitOutcome::Ok(stdout) => parse_ref_listing(&stdout),
        thegn_core::git_run::GitOutcome::EmptyOk => Ok(Vec::new()),
        other => anyhow::bail!("git for-each-ref failed: {other}"),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SweepReport {
    pub(crate) deleted: usize,
    pub(crate) failed: usize,
    pub(crate) limit_reached: bool,
}

pub(crate) const ORPHAN_SWEEP_LIMIT: usize = 64;

pub(crate) fn sweep_orphan_ingest_refs(
    target: &GitLoc,
    live_rows: impl FnOnce() -> Result<Vec<(GitLoc, String)>>,
) -> Result<SweepReport> {
    if target.is_remote() {
        anyhow::bail!("cannot sweep merge-queue ingest refs in a remote target store");
    }

    let outcome = target.git_run(
        &[
            "for-each-ref",
            "--count=65",
            "--format=%(refname)",
            "refs/thegn/mq/",
        ],
        &thegn_core::git_run::GitPolicy::read(crate::integrate::DRIVER_SUBPROCESS_TIMEOUT),
        &AtomicBool::new(false),
    );
    let refs = match outcome {
        thegn_core::git_run::GitOutcome::Ok(stdout) => parse_ref_listing(&stdout)?,
        thegn_core::git_run::GitOutcome::EmptyOk => Vec::new(),
        other => anyhow::bail!("git for-each-ref failed: {other}"),
    };

    let live_keys: HashSet<String> = live_rows()?
        .iter()
        .map(|(loc, branch)| row_key(loc, branch))
        .collect();
    let mut report = SweepReport {
        limit_reached: refs.len() > ORPHAN_SWEEP_LIMIT,
        ..SweepReport::default()
    };
    for name in refs.into_iter().take(ORPHAN_SWEEP_LIMIT) {
        let Some(rest) = name.strip_prefix("refs/thegn/mq/") else {
            continue;
        };
        let mut parts = rest.split('/');
        let generation_layout = match (parts.next(), parts.next(), parts.next()) {
            (Some(key), Some(generation), None) => {
                is_lower_hex_16(key) && is_lower_hex_16(generation)
            }
            _ => false,
        };
        if generation_layout
            && let Some(key) = rest.split('/').next()
            && live_keys.contains(key)
        {
            continue;
        }
        match ref_update(target, &["update-ref", "-d", &name]) {
            Ok(()) => report.deleted += 1,
            Err(error) => {
                report.failed += 1;
                tracing::warn!(target: "thegn::merge", ref_name = %name, %error, "failed to sweep merge-queue ingest ref");
            }
        }
    }
    Ok(report)
}

fn is_lower_hex_16(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Result of comparing the live source host branch to the ingested OID.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceCheck {
    /// The live source still names the ingested OID.
    Unchanged,
    /// The live source moved to a different OID.
    Moved { actual: String },
    /// The live source could not be read.
    Unreachable(String),
}

fn classify_source(ingested: &str, live: Result<String>) -> SourceCheck {
    match live {
        Ok(actual) if actual == ingested => SourceCheck::Unchanged,
        Ok(actual) => SourceCheck::Moved { actual },
        Err(error) => SourceCheck::Unreachable(format!("{error:#}")),
    }
}

/// Re-read a source branch on its authority and compare it with its ingested OID.
pub fn verify_source_unchanged(branch_loc: &GitLoc, branch: &str, ingested: &str) -> SourceCheck {
    classify_source(
        ingested,
        CliGit.rev_parse(branch_loc, &format!("refs/heads/{branch}")),
    )
}

/// A stable identity for the *store* a loc points at: `local` for the executing
/// host, `ssh:<host>:<port>` for an ssh remote, `prov:<prefix>` for a provider
/// env. Two locs share an object store iff their host ids match (worktrees of
/// one repo on one host share its `.git`; membership already scopes to one repo).
fn host_id(loc: &GitLoc) -> String {
    match loc {
        GitLoc::Local(_) => "local".to_string(),
        GitLoc::Remote { ssh, .. } => format!("ssh:{}:{}", ssh.host, ssh.port),
        GitLoc::Provider { control_prefix, .. } => format!("prov:{}", control_prefix.join(" ")),
    }
}

/// Whether `branch`'s tip must be fetched into `target`'s store — true exactly
/// when the two live on different hosts.
fn needs_ingest(target: &GitLoc, branch: &GitLoc) -> bool {
    host_id(target) != host_id(branch)
}

#[expect(
    clippy::disallowed_methods,
    reason = "off-loop: merge-queue drain (spawn_blocking) / CLI; the killed producer must be reaped"
)]
fn stream_bundle(
    mut producer: Command,
    tmp: &mut BundleTemp,
    limits: &BundleLimits,
) -> std::result::Result<u64, IngestError> {
    let free_bytes = limits
        .free_bytes
        .or_else(|| thegn_metrics::disk_space(tmp.path()).map(|(_, free_bytes, _)| free_bytes));
    let free_bytes = free_bytes.ok_or_else(|| {
        IngestError::LocalStorage(io::Error::other(
            "could not determine free space for bundle custody",
        ))
    })?;
    let max_bytes = effective_bundle_cap(limits.max_bytes, free_bytes, BUNDLE_FREE_SPACE_MARGIN)
        .ok_or_else(|| {
            IngestError::LocalStorage(io::Error::other("not enough free space for bundle custody"))
        })?;
    producer
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let started = Instant::now();
    let mut child = spawn_bundle_producer(&mut producer).map_err(|error| IngestError::Remote {
        code: None,
        stderr_tail: error.to_string(),
    })?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_bundle_producer(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(target: "thegn::merge", %error, "failed waiting for bundle producer without stdout pipe");
            }
            return Err(IngestError::Remote {
                code: None,
                stderr_tail: "producer stdout pipe unavailable".to_string(),
            });
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            kill_bundle_producer(&mut child);
            if let Err(error) = child.wait() {
                tracing::debug!(target: "thegn::merge", %error, "failed waiting for bundle producer without stderr pipe");
            }
            return Err(IngestError::Remote {
                code: None,
                stderr_tail: "producer stderr pipe unavailable".to_string(),
            });
        }
    };
    let (tx, rx) = mpsc::sync_channel::<io::Result<Vec<u8>>>(4);
    let (stdout_done_tx, stdout_done_rx) = mpsc::sync_channel(1);
    let stdout_thread = thegn_core::thread_name::spawn_named("thegn-bndl-out", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let mut reader = stdout;
        loop {
            let mut chunk = vec![0_u8; 64 * 1024];
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    chunk.truncate(count);
                    if let Err(unsent) = tx.send(Ok(chunk)) {
                        tracing::debug!(target: "thegn::merge", "bundle stdout receiver closed: {unsent:?}");
                        break;
                    }
                }
                Err(error) => {
                    if let Err(unsent) = tx.send(Err(error)) {
                        tracing::debug!(target: "thegn::merge", "bundle stdout receiver closed after read error: {unsent:?}");
                    }
                    break;
                }
            }
        }
        if let Err(error) = stdout_done_tx.send(()) {
            tracing::debug!(target: "thegn::merge", "bundle stdout completion receiver closed: {error}");
        }
    });
    let stderr_limit = limits.stderr_tail;
    let stderr_tail = Arc::new(Mutex::new((
        Vec::with_capacity(stderr_limit.min(64 * 1024)),
        false,
    )));
    let stderr_tail_reader = Arc::clone(&stderr_tail);
    let (stderr_done_tx, stderr_done_rx) = mpsc::sync_channel(1);
    let stderr_thread = thegn_core::thread_name::spawn_named("thegn-bndl-err", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Utility);
        let mut reader = stderr;
        let mut chunk = [0_u8; 8192];
        let read_result = loop {
            match reader.read(&mut chunk) {
                Ok(0) => break Ok(()),
                Ok(count) => {
                    let incoming = &chunk[..count];
                    let (tail, truncated) = &mut *stderr_tail_reader
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if stderr_limit == 0 {
                        *truncated |= !incoming.is_empty();
                    } else if incoming.len() >= stderr_limit {
                        *truncated |= !tail.is_empty() || incoming.len() > stderr_limit;
                        tail.clear();
                        tail.extend_from_slice(&incoming[incoming.len() - stderr_limit..]);
                    } else {
                        let excess = tail
                            .len()
                            .saturating_add(incoming.len())
                            .saturating_sub(stderr_limit);
                        if excess > 0 {
                            tail.drain(..excess);
                            *truncated = true;
                        }
                        tail.extend_from_slice(incoming);
                    }
                }
                Err(error) => break Err(error),
            }
        };
        if let Err(error) = stderr_done_tx.send(read_result) {
            tracing::debug!(target: "thegn::merge", "bundle stderr completion receiver closed: {error}");
        }
    });

    let mut bytes = 0_u64;
    let mut terminal_error = None;
    let mut status = None;
    loop {
        if crate::fold_cancel::requested() {
            terminal_error = Some(IngestError::Cancelled);
            kill_bundle_producer(&mut child);
            break;
        }
        let remaining = limits.deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            terminal_error = Some(IngestError::Timeout {
                after: limits.deadline,
            });
            kill_bundle_producer(&mut child);
            break;
        }
        let poll_wait = remaining.min(Duration::from_millis(10));
        match rx.recv_timeout(poll_wait) {
            Ok(Ok(chunk)) => {
                bytes = bytes.saturating_add(chunk.len() as u64);
                if bytes > max_bytes {
                    terminal_error = Some(IngestError::SizeLimit { cap: max_bytes });
                    kill_bundle_producer(&mut child);
                    break;
                }
                if let Err(error) = tmp.write(&chunk) {
                    terminal_error = Some(IngestError::LocalStorage(error));
                    kill_bundle_producer(&mut child);
                    break;
                }
            }
            Ok(Err(error)) => {
                terminal_error = Some(IngestError::Remote {
                    code: None,
                    stderr_tail: error.to_string(),
                });
                kill_bundle_producer(&mut child);
                break;
            }
            Err(RecvTimeoutError::Timeout) => {
                match crate::platform::gate_child_exited(&mut child) {
                    Ok(true) => {
                        kill_bundle_producer(&mut child);
                        match child.wait() {
                            Ok(exited) => status = Some(exited),
                            Err(error) => {
                                terminal_error = Some(IngestError::Remote {
                                    code: None,
                                    stderr_tail: error.to_string(),
                                });
                            }
                        }
                        break;
                    }
                    Ok(false) => continue,
                    Err(error) => {
                        terminal_error = Some(IngestError::Remote {
                            code: None,
                            stderr_tail: error.to_string(),
                        });
                        kill_bundle_producer(&mut child);
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                match crate::platform::gate_child_exited(&mut child) {
                    Ok(true) => {
                        kill_bundle_producer(&mut child);
                        match child.wait() {
                            Ok(exited) => status = Some(exited),
                            Err(error) => {
                                terminal_error = Some(IngestError::Remote {
                                    code: None,
                                    stderr_tail: error.to_string(),
                                });
                            }
                        }
                        break;
                    }
                    Ok(false) => std::thread::sleep(thegn_proc::next_backoff(
                        Duration::from_millis(10),
                        remaining,
                    )),
                    Err(error) => {
                        terminal_error = Some(IngestError::Remote {
                            code: None,
                            stderr_tail: error.to_string(),
                        });
                        kill_bundle_producer(&mut child);
                        break;
                    }
                }
            }
        }
    }

    drop(rx);
    if status.is_none()
        && let Err(error) = child.wait()
    {
        tracing::debug!(target: "thegn::merge", %error, "failed waiting for bundle producer after termination");
    }
    let reader_wait = limits
        .deadline
        .saturating_sub(started.elapsed())
        .min(BUNDLE_READER_GRACE);
    let stdout_finished = stdout_done_rx.recv_timeout(reader_wait).is_ok();
    let stderr_wait = limits
        .deadline
        .saturating_sub(started.elapsed())
        .min(BUNDLE_READER_GRACE);
    let stderr_result = Result::ok(stderr_done_rx.recv_timeout(stderr_wait));
    drop(stdout_thread);
    drop(stderr_thread);

    let (tail, truncated) = stderr_tail
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let mut stderr_text = String::new();
    if truncated {
        stderr_text.push_str("[... stderr truncated ...]");
    }
    stderr_text.push_str(&String::from_utf8_lossy(&tail));
    if !stdout_finished {
        tracing::debug!(target: "thegn::merge", "bundle stdout reader did not finish during grace period");
    }
    let stderr_reader_missing = stderr_result.is_none();
    match stderr_result {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            if !stderr_text.is_empty() {
                stderr_text.push_str("; ");
            }
            stderr_text.push_str(&format!("stderr read failed: {error}"));
        }
        None => {
            if !stderr_text.is_empty() {
                stderr_text.push_str("; ");
            }
            stderr_text.push_str("[... stderr reader did not finish ...]");
        }
    }
    let max_text_bytes = limits.stderr_tail.saturating_add(96);
    if stderr_text.len() > max_text_bytes {
        let mut end = max_text_bytes;
        while !stderr_text.is_char_boundary(end) {
            end -= 1;
        }
        stderr_text.truncate(end);
    }

    if let Some(error) = terminal_error {
        return Err(match error {
            IngestError::Remote {
                code: None,
                stderr_tail: read_error,
            } if !stderr_text.is_empty() => IngestError::Remote {
                code: None,
                stderr_tail: format!("{read_error}; stderr: {stderr_text}"),
            },
            IngestError::Remote {
                code: None,
                stderr_tail: read_error,
            } if stderr_reader_missing => IngestError::Remote {
                code: None,
                stderr_tail: format!("{read_error}; [... stderr reader did not finish ...]"),
            },
            other => other,
        });
    }
    let status =
        status.unwrap_or_else(|| unreachable!("producer status or terminal error is required"));
    if !status.success() {
        return Err(IngestError::Remote {
            code: status.code(),
            stderr_tail: stderr_text,
        });
    }
    if bytes == 0 {
        return Err(IngestError::InvalidBundle(
            "git bundle create produced no data".to_string(),
        ));
    }
    Ok(bytes)
}

/// Materialize the bundle on the target host and fetch its branch ref into the
/// supplied generation-scoped ref. Under the co-location model the drain runs on the
/// target host, so `target` is `Local`; a remote target means the caller should
/// have dispatched to that host's drain daemon (see Milestone B), so we fail
/// loudly rather than trying to push a bundle the wrong way.
fn fetch_from_custody(
    target: &GitLoc,
    branch: &str,
    tmp: BundleTemp,
    target_ref: &str,
    expected: &str,
    deadline: Duration,
) -> Result<()> {
    if target.is_remote() {
        anyhow::bail!(
            "remote target store: run the drain on the target host (merge daemon), not by pushing a bundle over ssh"
        );
    }
    if let Err(error) = tmp.verify() {
        let cleanup = tmp.finish();
        return match cleanup {
            Ok(()) => Err(anyhow::Error::new(IngestError::LocalStorage(error))
                .context("verify temp bundle custody before fetch")),
            Err(cleanup) => Err(
                anyhow::Error::new(IngestError::LocalStorage(error)).context(format!(
                    "verify temp bundle custody before fetch; cleanup failed: {cleanup}"
                )),
            ),
        };
    }
    let tmp_s = tmp.path().to_string_lossy().to_string();
    let staging_ref = format!("{target_ref}-staging");
    // The random generation makes this an isolated fetch destination. The final
    // ref is published separately with an all-zero old OID, making it create-only.
    let refspec = format!("refs/heads/{branch}:{staging_ref}");
    let outcome = run_fetch_cancellable(target.git_command(&["fetch", &tmp_s, &refspec]), deadline);
    if !outcome.is_success() {
        let outcome_text = outcome.to_string();
        if let Err(error) = remove_staging_ref(target, &staging_ref) {
            tracing::warn!(ref_name = %staging_ref, error = %error, "failed to remove staging ref after bundle fetch failure");
        }
        let error = match outcome {
            thegn_core::git_run::GitOutcome::Timeout => IngestError::Timeout { after: deadline },
            thegn_core::git_run::GitOutcome::Cancelled => IngestError::Cancelled,
            thegn_core::git_run::GitOutcome::Exit { .. } => {
                IngestError::InvalidBundle(outcome_text)
            }
            other => IngestError::LocalStorage(io::Error::other(other.to_string())),
        };
        match tmp.finish() {
            Ok(()) => return Err(anyhow::Error::new(error)),
            Err(cleanup) => {
                return Err(anyhow::Error::new(error)
                    .context(format!("bundle custody cleanup failed: {cleanup}")));
            }
        };
    }
    let fetch_head = match CliGit.rev_parse(target, &staging_ref) {
        Ok(oid) => oid,
        Err(error) => {
            if let Err(cleanup_error) = remove_staging_ref(target, &staging_ref) {
                tracing::warn!(ref_name = %staging_ref, error = %cleanup_error, "failed to remove unreadable bundle staging ref");
            }
            let cleanup = tmp.finish();
            return match cleanup {
                Ok(()) => Err(error.context("resolve fetched bundle tip")),
                Err(cleanup) => Err(error.context(format!(
                    "resolve fetched bundle tip; cleanup failed: {cleanup}"
                ))),
            };
        }
    };
    if fetch_head != expected {
        if let Err(error) = ref_update(target, &["update-ref", "-d", &staging_ref, &fetch_head]) {
            tracing::warn!(ref_name = %staging_ref, error = %error, "failed to remove mismatched bundle staging ref");
        }
        tmp.finish().context("cleanup mismatched fetched bundle")?;
        anyhow::bail!("bundled tip differs from the branch-host tip; retry");
    }
    let zero_oid = "0".repeat(expected.len());
    if let Err(publication_error) =
        ref_update(target, &["update-ref", target_ref, expected, &zero_oid])
    {
        if let Err(error) = ref_update(target, &["update-ref", "-d", &staging_ref, expected]) {
            tracing::warn!(ref_name = %staging_ref, error = %error, "failed to remove staging ref after create-only publication failure");
        }
        if let Err(cleanup) = tmp.finish() {
            anyhow::bail!(
                "create-only merge-queue ingest ref publication failed: {publication_error}; bundle custody cleanup failed: {cleanup}"
            );
        }
        anyhow::bail!("create-only merge-queue ingest ref publication failed: {publication_error}");
    }
    if let Err(error) = ref_update(target, &["update-ref", "-d", &staging_ref, expected]) {
        tracing::warn!(ref_name = %staging_ref, error = %error, "failed to remove merge-queue bundle staging ref");
    }
    if let Err(error) = tmp.finish() {
        if let Err(cleanup_error) = ref_update(target, &["update-ref", "-d", target_ref, expected])
        {
            tracing::warn!(ref_name = %target_ref, error = %cleanup_error, "failed to remove ingest ref after bundle cleanup failure");
        }
        return Err(error).context("cleanup fetched bundle");
    }
    Ok(())
}

fn remove_staging_ref(target: &GitLoc, staging_ref: &str) -> std::result::Result<(), String> {
    match ref_oid(target, staging_ref)? {
        Some(actual) => ref_update(target, &["update-ref", "-d", staging_ref, &actual]),
        None => Ok(()),
    }
}

fn run_fetch_cancellable(mut cmd: Command, deadline: Duration) -> thegn_core::git_run::GitOutcome {
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (tx, rx) = mpsc::channel();
    let worker = match thegn_core::thread_name::try_spawn_named("thegn-mq-fetch", move || {
        crate::platform::qos::set_self(crate::platform::qos::Qos::Interactive);
        let outcome = thegn_core::git_run::run_command(
            &mut cmd,
            &thegn_core::git_run::GitPolicy::read(deadline),
            &worker_cancel,
        );
        if let Err(error) = tx.send(outcome) {
            tracing::debug!(error = %error, "consumer fetch result receiver disconnected");
        }
    }) {
        Ok(worker) => worker,
        Err(error) => return thegn_core::git_run::GitOutcome::Spawn(error),
    };

    let mut cancellation_requested = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(outcome) => {
                return match worker.join() {
                    Ok(()) => outcome,
                    Err(_) => thegn_core::git_run::GitOutcome::Supervise(
                        "consumer fetch worker panicked after returning a result".to_string(),
                    ),
                };
            }
            Err(RecvTimeoutError::Timeout) => {
                if !cancellation_requested && crate::fold_cancel::requested() {
                    cancel.store(true, std::sync::atomic::Ordering::Release);
                    cancellation_requested = true;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return match worker.join() {
                    Ok(()) => thegn_core::git_run::GitOutcome::Supervise(
                        "consumer fetch worker exited without a result".to_string(),
                    ),
                    Err(_) => thegn_core::git_run::GitOutcome::Supervise(
                        "consumer fetch worker panicked before returning a result".to_string(),
                    ),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use thegn_core::remote::{GitLoc, SshTarget};

    fn remote(host: &str) -> GitLoc {
        GitLoc::Remote {
            ssh: SshTarget::plain(host.to_string(), 22, false),
            path: "/wt".to_string(),
        }
    }

    #[test]
    fn mq_ref_is_namespaced() {
        let first = generation_ref(&GitLoc::Local(PathBuf::from("/src")), "feat/x").unwrap();
        let second = generation_ref(&GitLoc::Local(PathBuf::from("/src")), "feat/x").unwrap();
        assert!(first.starts_with("refs/thegn/mq/"));
        assert_ne!(first, second);
        let authority_a = generation_ref(&remote("a"), "feat").unwrap();
        let authority_b = generation_ref(&remote("b"), "feat").unwrap();
        assert_ne!(authority_a, authority_b);
    }

    #[test]
    fn empty_stale_ref_listing_is_successful_and_empty() {
        assert_eq!(parse_ref_listing(b"").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn ref_oid_tells_absent_from_a_failed_read() {
        let fixture = stores("source");
        let target = GitLoc::Local(fixture.target.clone());
        assert_eq!(ref_oid(&target, "refs/thegn/mq/none"), Ok(None));
        assert!(matches!(
            ref_oid(&target, "HEAD"),
            Ok(Some(oid)) if (oid.len() == 40 || oid.len() == 64)
                && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
        ));
        let missing = GitLoc::Local(fixture._root.path().join("missing"));
        assert!(ref_oid(&missing, "HEAD").is_err());
    }

    #[test]
    fn ref_update_reports_git_refusal() {
        let fixture = stores("source");
        let target = GitLoc::Local(fixture.target.clone());
        let result = ref_update(
            &target,
            &[
                "update-ref",
                "-d",
                "refs/heads/main",
                "0123456789012345678901234567890123456789",
            ],
        );
        assert!(matches!(result, Err(reason) if reason.contains("Git exited")));
        assert!(ref_oid(&target, "refs/heads/main").unwrap().is_some());
    }

    #[test]
    fn same_store_needs_no_ingest() {
        let local = GitLoc::Local(PathBuf::from("/repo"));
        assert!(!needs_ingest(&local, &GitLoc::Local(PathBuf::from("/wt"))));
        assert!(!needs_ingest(&remote("box"), &remote("box")));
    }

    #[test]
    fn different_host_needs_ingest() {
        let local = GitLoc::Local(PathBuf::from("/repo"));
        assert!(needs_ingest(&local, &remote("box")));
        assert!(needs_ingest(&remote("a"), &remote("b")));
    }

    #[test]
    fn ensure_tip_returns_heads_ref_when_same_store() {
        let local = GitLoc::Local(PathBuf::from("/repo"));
        let r = ingest_tip(&local, "feat", &GitLoc::Local(PathBuf::from("/wt"))).unwrap();
        assert_eq!(r.ref_name, "refs/heads/feat");
        assert!(r.oid.is_none());
    }

    #[expect(clippy::disallowed_methods)] // test-only git plumbing, never on the loop
    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = thegn_core::util::git_cmd(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        assert!(ok, "git {} failed in {}", args.join(" "), dir.display());
    }

    struct Stores {
        _root: tempfile::TempDir,
        target: PathBuf,
        source: PathBuf,
        oid: String,
    }

    fn stores(source_name: &str) -> Stores {
        use thegn_core::util;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let source = root.path().join(source_name);
        std::fs::create_dir_all(&target).unwrap();
        git(&target, &["init", "-q", "-b", "main"]);
        git(&target, &["config", "user.name", "t"]);
        git(&target, &["config", "user.email", "t@e"]);
        git(&target, &["config", "commit.gpgsign", "false"]);
        std::fs::write(target.join("base.txt"), "base\n").unwrap();
        git(&target, &["add", "-A"]);
        git(&target, &["commit", "-q", "-m", "base"]);
        git(
            root.path(),
            &[
                "clone",
                "-q",
                &target.to_string_lossy(),
                &source.to_string_lossy(),
            ],
        );
        git(&source, &["config", "user.name", "t"]);
        git(&source, &["config", "user.email", "t@e"]);
        git(&source, &["config", "commit.gpgsign", "false"]);
        git(&source, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(source.join("feat.txt"), "feature\n").unwrap();
        git(&source, &["add", "-A"]);
        git(&source, &["commit", "-q", "-m", "feature"]);
        let oid = util::git_out(&source, &["rev-parse", "refs/heads/feat"]).unwrap();
        Stores {
            _root: root,
            target,
            source,
            oid,
        }
    }

    fn ingest(fixture: &Stores, expected: Option<&str>) -> Result<IngestedTip> {
        ingest_with_limits(
            fixture,
            expected,
            test_limits(
                MAX_BUNDLE_BYTES,
                crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
                BUNDLE_STDERR_TAIL,
            ),
        )
    }

    fn ingest_with_limits(
        fixture: &Stores,
        expected: Option<&str>,
        limits: BundleLimits,
    ) -> Result<IngestedTip> {
        ingest_tip_with_expected(
            &GitLoc::Local(fixture.target.clone()),
            "feat",
            &GitLoc::Local(fixture.source.clone()),
            expected,
            &limits,
        )
    }

    fn test_limits(max_bytes: u64, deadline: Duration, stderr_tail: usize) -> BundleLimits {
        BundleLimits {
            max_bytes,
            free_bytes: Some(u64::MAX),
            deadline,
            stderr_tail,
        }
    }

    fn custody_for_fixture(fixture: &Stores, temp_root: &std::path::Path) -> (BundleTemp, String) {
        let generated = generation_ref(&GitLoc::Local(fixture.source.clone()), "feat").unwrap();
        let mut custody = BundleTemp::new_in(temp_root).unwrap();
        let source = GitLoc::Local(fixture.source.clone());
        let bundle_command = source.git_command(&["bundle", "create", "-", "refs/heads/feat"]);
        stream_bundle(
            bundle_command,
            &mut custody,
            &test_limits(
                MAX_BUNDLE_BYTES,
                crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
                BUNDLE_STDERR_TAIL,
            ),
        )
        .unwrap();
        (custody, generated)
    }

    fn install_reference_transaction_hook(fixture: &Stores, script: &[u8]) -> std::io::Result<()> {
        let hooks = fixture.target.join(".git/hooks");
        std::fs::create_dir_all(&hooks)?;
        crate::platform::publish_private_executable(
            &hooks.join("reference-transaction.staged"),
            &hooks.join("reference-transaction"),
            script,
        )
    }

    fn list_refs(target: &std::path::Path) -> Vec<String> {
        thegn_core::util::git_out_allow_empty(
            target,
            &["for-each-ref", "--format=%(refname)", "refs/thegn/mq/"],
        )
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
    }

    fn shell(script: &str) -> Command {
        let shell = thegn_core::util::which_path("sh").expect("sh checked by caller");
        let mut command = Command::new(shell);
        command.args(["-c", script]);
        command
    }

    fn small_limits(max_bytes: u64, deadline: Duration, stderr_tail: usize) -> BundleLimits {
        test_limits(max_bytes, deadline, stderr_tail)
    }

    #[test]
    fn effective_bundle_cap_reserves_free_space_margin() {
        let margin = 256;
        assert_eq!(
            effective_bundle_cap(1_000, margin + 2_000, margin),
            Some(1_000)
        );
        assert_eq!(effective_bundle_cap(1_000, margin + 200, margin), Some(200));
        assert_eq!(effective_bundle_cap(1_000, margin, margin), None);
        assert_eq!(effective_bundle_cap(1_000, 0, margin), None);
    }

    #[test]
    fn bundle_stream_refuses_custody_below_free_space_margin() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let mut limits = small_limits(1 << 20, Duration::from_secs(30), 4096);
        limits.free_bytes = Some(BUNDLE_FREE_SPACE_MARGIN);
        let result = stream_bundle(shell("printf x"), &mut tmp, &limits);
        assert!(matches!(
            result,
            Err(IngestError::LocalStorage(error))
                if error.to_string() == "not enough free space for bundle custody"
        ));
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn ingest_refuses_custody_below_free_space_margin() {
        let fixture = stores("source");
        let mut limits = test_limits(
            MAX_BUNDLE_BYTES,
            crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
            BUNDLE_STDERR_TAIL,
        );
        limits.free_bytes = Some(BUNDLE_FREE_SPACE_MARGIN);
        let error = match ingest_with_limits(&fixture, None, limits) {
            Ok(_) => panic!("ingest must refuse custody without the reserved margin"),
            Err(error) => error,
        };
        assert!(matches!(
            error.downcast_ref::<IngestError>(),
            Some(IngestError::LocalStorage(error))
                if error.to_string() == "not enough free space for bundle custody"
        ));
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn bundle_stream_size_limit_uses_reduced_free_space_cap() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let mut limits = small_limits(1 << 20, Duration::from_secs(30), 4096);
        limits.free_bytes = Some(BUNDLE_FREE_SPACE_MARGIN + 128);
        let result = stream_bundle(shell("yes thegn-bundle"), &mut tmp, &limits);
        let error = result.expect_err("producer must hit the reduced cap");
        assert!(matches!(&error, IngestError::SizeLimit { cap: 128 }));
        assert_eq!(error.to_string(), "bundle exceeded the 128-byte size limit");
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_stops_an_infinite_producer_at_the_cap() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let started = Instant::now();
        let result = stream_bundle(
            shell("yes thegn-bundle"),
            &mut tmp,
            &small_limits(1 << 20, Duration::from_secs(30), 64 * 1024),
        );
        assert!(matches!(
            result,
            Err(IngestError::SizeLimit { cap }) if cap == 1 << 20
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_times_out_a_silent_producer() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let started = Instant::now();
        let result = stream_bundle(
            shell("sleep 30"),
            &mut tmp,
            &small_limits(1 << 20, Duration::from_secs(1), 4096),
        );
        assert!(matches!(result, Err(IngestError::Timeout { .. })));
        assert!(started.elapsed() < Duration::from_secs(10));
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_kills_and_reaps_a_producer_holding_pipe_open() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let started = Instant::now();
        let result = stream_bundle(
            shell("printf x; exec sleep 30"),
            &mut tmp,
            &small_limits(1 << 20, Duration::from_secs(1), 4096),
        );
        assert!(matches!(result, Err(IngestError::Timeout { .. })));
        assert!(started.elapsed() < Duration::from_secs(10));
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_stops_when_the_fold_is_cancelled() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let _scope = crate::fold_cancel::scope(|| true);
        let started = Instant::now();
        let result = stream_bundle(
            shell("sleep 30"),
            &mut tmp,
            &small_limits(1 << 20, Duration::from_secs(30), 4096),
        );
        assert!(matches!(result, Err(IngestError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(10));
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_quiesces_background_pipe_inheritor() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let started = Instant::now();
        let result = stream_bundle(
            shell("sleep 30 >&2 & printf x"),
            &mut tmp,
            &small_limits(1 << 20, Duration::from_secs(5), 4096),
        );
        assert!(matches!(result, Ok(1)));
        assert!(started.elapsed() < Duration::from_secs(3));
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_keeps_only_a_bounded_stderr_tail() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let result = stream_bundle(
            shell("yes err-line | head -c 300000 >&2; exit 3"),
            &mut tmp,
            &small_limits(1 << 20, Duration::from_secs(30), 4096),
        );
        assert!(
            matches!(result, Err(IngestError::Remote { code: Some(3), stderr_tail }) if stderr_tail.len() <= 4096 + 64 && stderr_tail.starts_with("[... stderr truncated ...]"))
        );
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_refuses_an_empty_bundle() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let mut tmp = BundleTemp::new_in(root.path()).unwrap();
        let result = stream_bundle(
            shell("exit 0"),
            &mut tmp,
            &small_limits(1 << 20, Duration::from_secs(30), 4096),
        );
        assert!(matches!(result, Err(IngestError::InvalidBundle(_))));
        tmp.finish().unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn bundle_stream_round_trip_moves_a_tip() {
        use thegn_core::util;
        let fixture = stores("source");
        let temp_root = fixture._root.path().join("bundle-temp");
        std::fs::create_dir(&temp_root).unwrap();
        let generated = generation_ref(&GitLoc::Local(fixture.source.clone()), "feat").unwrap();
        let mut garbage = BundleTemp::new_in(&temp_root).unwrap();
        garbage.write(b"not a bundle").unwrap();
        assert!(
            fetch_from_custody(
                &GitLoc::Local(fixture.target.clone()),
                "feat",
                garbage,
                &generated,
                &fixture.oid,
                crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
            )
            .is_err()
        );
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
        assert!(!util::git_ok(
            &fixture.target,
            &["cat-file", "-e", &fixture.oid]
        ));
        assert!(list_refs(&fixture.target).is_empty());
        let mut custody = BundleTemp::new_in(&temp_root).unwrap();
        let source = GitLoc::Local(fixture.source.clone());
        let bundle_command = source.git_command(&["bundle", "create", "-", "refs/heads/feat"]);
        stream_bundle(
            bundle_command,
            &mut custody,
            &test_limits(
                MAX_BUNDLE_BYTES,
                crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
                BUNDLE_STDERR_TAIL,
            ),
        )
        .unwrap();
        fetch_from_custody(
            &GitLoc::Local(fixture.target.clone()),
            "feat",
            custody,
            &generated,
            &fixture.oid,
            crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
        assert_eq!(
            util::git_out(&fixture.target, &["rev-parse", &generated]).as_deref(),
            Some(fixture.oid.as_str())
        );
        let ingested = ingest(&fixture, None).unwrap();
        assert_eq!(ingested.oid.as_deref(), Some(fixture.oid.as_str()));
        assert_eq!(
            util::git_out(&fixture.target, &["rev-parse", &ingested.ref_name]).as_deref(),
            Some(fixture.oid.as_str())
        );
        drop(ingested);
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn create_only_publication_failure_names_git_reason() {
        use thegn_core::util;

        let fixture = stores("source");
        let temp_root = tempfile::tempdir().unwrap();
        let generated = generation_ref(&GitLoc::Local(fixture.source.clone()), "feat").unwrap();
        let target_head = util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();
        git(&fixture.target, &["update-ref", &generated, &target_head]);
        let mut custody = BundleTemp::new_in(temp_root.path()).unwrap();
        let source = GitLoc::Local(fixture.source.clone());
        let bundle_command = source.git_command(&["bundle", "create", "-", "refs/heads/feat"]);
        stream_bundle(
            bundle_command,
            &mut custody,
            &test_limits(
                MAX_BUNDLE_BYTES,
                crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
                BUNDLE_STDERR_TAIL,
            ),
        )
        .unwrap();

        let result = fetch_from_custody(
            &GitLoc::Local(fixture.target.clone()),
            "feat",
            custody,
            &generated,
            &fixture.oid,
            crate::integrate::DRIVER_SUBPROCESS_TIMEOUT,
        );
        assert!(matches!(result, Err(error) if format!("{error:#}").contains("Git exited")));
        assert_eq!(list_refs(&fixture.target), vec![generated]);
        assert_eq!(std::fs::read_dir(temp_root.path()).unwrap().count(), 0);
    }

    #[test]
    fn cancelled_ingest_publishes_no_ref() {
        let fixture = stores("source");
        let _scope = crate::fold_cancel::scope(|| true);
        let result = ingest(&fixture, None);
        assert!(result.is_err());
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("cancelled ingestion unexpectedly succeeded"),
        };
        assert!(format!("{error:#}").contains("cancelled"));
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn fetch_consumer_hang_times_out_under_the_ingest_deadline() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let fixture = stores("source");
        let temp_root = fixture._root.path().join("bundle-temp");
        std::fs::create_dir(&temp_root).unwrap();
        let (custody, generated) = custody_for_fixture(&fixture, &temp_root);
        if let Err(error) =
            install_reference_transaction_hook(&fixture, b"#!/bin/sh\nexec sleep 30\n")
        {
            if error.kind() == std::io::ErrorKind::Unsupported {
                return;
            }
            panic!("install hanging reference-transaction hook: {error}");
        }
        let started = Instant::now();
        let result = fetch_from_custody(
            &GitLoc::Local(fixture.target.clone()),
            "feat",
            custody,
            &generated,
            &fixture.oid,
            Duration::from_secs(1),
        );
        let error = match result {
            Err(error) => error,
            Ok(()) => panic!("hung consumer unexpectedly succeeded"),
        };
        assert!(matches!(
            error.downcast_ref::<IngestError>(),
            Some(IngestError::Timeout { after }) if *after == Duration::from_secs(1)
        ));
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(list_refs(&fixture.target).is_empty());
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
    }

    #[test]
    fn fetch_consumer_failure_leaves_no_ref() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let fixture = stores("source");
        let temp_root = fixture._root.path().join("bundle-temp");
        std::fs::create_dir(&temp_root).unwrap();
        let (custody, generated) = custody_for_fixture(&fixture, &temp_root);
        if let Err(error) = install_reference_transaction_hook(&fixture, b"#!/bin/sh\nexit 1\n") {
            if error.kind() == std::io::ErrorKind::Unsupported {
                return;
            }
            panic!("install failing reference-transaction hook: {error}");
        }
        assert!(
            fetch_from_custody(
                &GitLoc::Local(fixture.target.clone()),
                "feat",
                custody,
                &generated,
                &fixture.oid,
                Duration::from_secs(10),
            )
            .is_err()
        );
        assert!(list_refs(&fixture.target).is_empty());
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
    }

    #[test]
    fn fetch_consumer_cancel_kills_a_hung_fetch_promptly() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let fixture = stores("source");
        let temp_root = fixture._root.path().join("bundle-temp");
        std::fs::create_dir(&temp_root).unwrap();
        let (custody, generated) = custody_for_fixture(&fixture, &temp_root);
        if let Err(error) =
            install_reference_transaction_hook(&fixture, b"#!/bin/sh\nexec sleep 30\n")
        {
            if error.kind() == std::io::ErrorKind::Unsupported {
                return;
            }
            panic!("install hanging reference-transaction hook: {error}");
        }

        let polls = std::rc::Rc::new(std::cell::Cell::new(0_u32));
        let probe = polls.clone();
        let _scope = crate::fold_cancel::scope(move || {
            probe.set(probe.get() + 1);
            probe.get() > 5
        });
        let started = Instant::now();
        let result = fetch_from_custody(
            &GitLoc::Local(fixture.target.clone()),
            "feat",
            custody,
            &generated,
            &fixture.oid,
            Duration::from_secs(60),
        );
        let elapsed = started.elapsed();
        let error = match result {
            Err(error) => error,
            Ok(()) => panic!("cancelled consumer unexpectedly succeeded"),
        };
        assert!(matches!(
            error.downcast_ref::<IngestError>(),
            Some(IngestError::Cancelled)
        ));
        assert!(
            elapsed < Duration::from_secs(25),
            "cancelled fetch took {elapsed:?}"
        );
        assert!(list_refs(&fixture.target).is_empty());
        assert_eq!(std::fs::read_dir(&temp_root).unwrap().count(), 0);
    }

    #[test]
    fn fetch_consumer_cancel_flag_reaches_the_executor() {
        if thegn_core::util::which_path("sh").is_none() {
            return;
        }
        let _scope = crate::fold_cancel::scope(|| true);
        let started = Instant::now();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
        let outcome = run_fetch_cancellable(cmd, Duration::from_secs(60));
        assert!(matches!(
            outcome,
            thegn_core::git_run::GitOutcome::Cancelled
        ));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "cancelled executor took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn ingest_ref_is_row_scoped_and_unique_per_generation() {
        let first = stores("source-one");
        let other = stores("source-two");
        let one = ingest(&first, None).unwrap();
        let one_ref = one.ref_name.clone();
        let two = ingest(&other, None).unwrap();
        let two_ref = two.ref_name.clone();
        assert_ne!(one_ref, two_ref);
        drop(one);
        drop(two);
        let again = ingest(&first, None).unwrap();
        assert_ne!(one_ref, again.ref_name);
    }

    #[test]
    fn ingest_force_pushed_tip_ingests_as_new_generation() {
        use thegn_core::util;
        let fixture = stores("source");
        let first = ingest(&fixture, None).unwrap();
        let old_ref = first.ref_name.clone();
        std::mem::forget(first);
        git(
            &fixture.source,
            &["commit", "--amend", "-m", "rewritten feature"],
        );
        let new_oid = util::git_out(&fixture.source, &["rev-parse", "refs/heads/feat"]).unwrap();
        let next = ingest(&fixture, None).unwrap();
        assert_ne!(old_ref, next.ref_name);
        assert_eq!(
            util::git_out(&fixture.target, &["rev-parse", &next.ref_name]).as_deref(),
            Some(new_oid.as_str())
        );
        assert!(!list_refs(&fixture.target).contains(&old_ref));
    }

    #[test]
    fn ingest_ref_is_deleted_when_the_guard_drops() {
        let fixture = stores("source");
        let ingested = ingest(&fixture, None).unwrap();
        assert_eq!(list_refs(&fixture.target), vec![ingested.ref_name.clone()]);
        drop(ingested);
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn ingest_prunes_legacy_branch_ref() {
        let fixture = stores("source");
        let target_oid =
            thegn_core::util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();
        let branch_loc = GitLoc::Local(fixture.source.clone());
        let stale_generation = format!(
            "refs/thegn/mq/{}/0000000000000000",
            row_key(&branch_loc, "feat")
        );
        let legacy_ref = "refs/thegn/mq/feat";
        // Both layouts can coexist: the generation namespace is below its
        // hexadecimal row key, while legacy refs remain direct children.
        git(
            &fixture.target,
            &["update-ref", &stale_generation, &target_oid],
        );
        git(&fixture.target, &["update-ref", legacy_ref, &target_oid]);
        let refs_before_ingest = list_refs(&fixture.target);
        assert!(refs_before_ingest.contains(&stale_generation));
        assert!(refs_before_ingest.contains(&legacy_ref.to_string()));

        let ingested = ingest(&fixture, None).unwrap();
        let refs_after_ingest = list_refs(&fixture.target);
        assert!(!refs_after_ingest.contains(&legacy_ref.to_string()));
        assert!(!refs_after_ingest.contains(&stale_generation));
        assert_eq!(refs_after_ingest, vec![ingested.ref_name.clone()]);
        drop(ingested);
    }

    #[test]
    fn ingest_claim_ref_embeds_the_generation() {
        let scope = IngestClaimScope::enter(7);
        let name = generation_ref(&GitLoc::Local(PathBuf::from("/src")), "feat").unwrap();
        let parts: Vec<_> = name.split('/').collect();
        assert_eq!(parts.len(), 5);
        assert!(is_lower_hex_16(parts[3]));
        assert!(is_lower_hex_16(parts[4]));
        assert!(parts[4].starts_with("00000007"));
        assert_eq!(current_ingest_claim_generation(), Some(7));
        drop(scope);

        let outside = generation_ref(&GitLoc::Local(PathBuf::from("/src")), "feat").unwrap();
        assert!(outside.rsplit('/').next().unwrap().starts_with("00000000"));
        assert_eq!(current_ingest_claim_generation(), None);
    }

    #[test]
    fn ingest_claim_scope_nests_and_restores() {
        let outer = IngestClaimScope::enter(1);
        assert_eq!(current_ingest_claim_generation(), Some(1));
        let inner = IngestClaimScope::enter(2);
        assert_eq!(current_ingest_claim_generation(), Some(2));
        drop(inner);
        assert_eq!(current_ingest_claim_generation(), Some(1));
        drop(outer);
        assert_eq!(current_ingest_claim_generation(), None);
    }

    #[test]
    fn ingest_claim_late_owner_keeps_the_newer_claimants_ref() {
        let fixture = stores("source");
        let newer = {
            let _scope = IngestClaimScope::enter(2);
            ingest(&fixture, None).unwrap()
        };
        let late = {
            let _scope = IngestClaimScope::enter(1);
            ingest(&fixture, None).unwrap()
        };
        let refs = list_refs(&fixture.target);
        assert!(refs.contains(&newer.ref_name));
        assert!(refs.contains(&late.ref_name));
        assert!(
            newer
                .ref_name
                .rsplit('/')
                .next()
                .unwrap()
                .starts_with("00000002")
        );
        assert!(
            late.ref_name
                .rsplit('/')
                .next()
                .unwrap()
                .starts_with("00000001")
        );

        drop(late);
        // IngestRef::drop deletes by ref name and OID; both refs share this OID, so only
        // `late.ref_name` disappears and the newer ref remains.
        assert_eq!(list_refs(&fixture.target), vec![newer.ref_name.clone()]);
        drop(newer);
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn ingest_claim_newer_claimant_prunes_lower_and_equal_generations() {
        let fixture = stores("source");
        let target_oid =
            thegn_core::util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();
        let prefix = format!(
            "refs/thegn/mq/{}/",
            row_key(&GitLoc::Local(fixture.source.clone()), "feat")
        );
        let lower = format!("{prefix}00000001deadbeef");
        let equal = format!("{prefix}00000002deadbeef");
        let newer = format!("{prefix}00000003deadbeef");
        let legacy = "refs/thegn/mq/feat".to_string();
        for ref_name in [&lower, &equal, &newer, &legacy] {
            git(&fixture.target, &["update-ref", ref_name, &target_oid]);
        }

        let _scope = IngestClaimScope::enter(2);
        let ingested = ingest(&fixture, None).unwrap();
        let refs = list_refs(&fixture.target);
        assert!(!refs.contains(&lower));
        assert!(!refs.contains(&equal));
        assert!(!refs.contains(&legacy));
        assert!(refs.contains(&newer));
        assert!(refs.contains(&ingested.ref_name));
        drop(ingested);
    }

    #[test]
    fn ingest_without_a_claim_scope_prunes_every_generation() {
        let fixture = stores("source");
        let target_oid =
            thegn_core::util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();
        let stale = format!(
            "refs/thegn/mq/{}/0000000500000000",
            row_key(&GitLoc::Local(fixture.source.clone()), "feat")
        );
        git(&fixture.target, &["update-ref", &stale, &target_oid]);

        let ingested = ingest(&fixture, None).unwrap();
        let refs = list_refs(&fixture.target);
        assert!(!refs.contains(&stale));
        assert!(refs.contains(&ingested.ref_name));
        drop(ingested);
    }

    #[test]
    fn ingest_claim_prune_keeps_unrelated_row_refs_and_orphan_sweep_layout() {
        let fixture = stores("source");
        let target_oid =
            thegn_core::util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();
        let other_source = GitLoc::Local(PathBuf::from("/other-source"));
        let unrelated = format!(
            "refs/thegn/mq/{}/0000000400000000",
            row_key(&other_source, "feat")
        );
        git(&fixture.target, &["update-ref", &unrelated, &target_oid]);
        let ingested = {
            let _scope = IngestClaimScope::enter(3);
            ingest(&fixture, None).unwrap()
        };
        let report = sweep_orphan_ingest_refs(&GitLoc::Local(fixture.target.clone()), || {
            Ok(vec![
                (GitLoc::Local(fixture.source.clone()), "feat".to_string()),
                (other_source, "feat".to_string()),
            ])
        })
        .unwrap();
        assert_eq!(report.deleted, 0);
        assert!(is_lower_hex_16(
            ingested.ref_name.rsplit('/').next().unwrap()
        ));
        assert!(list_refs(&fixture.target).contains(&ingested.ref_name));
        assert!(list_refs(&fixture.target).contains(&unrelated));
        drop(ingested);
    }

    #[test]
    fn ingest_refuses_bundle_that_differs_from_expected_tip() {
        let fixture = stores("source");
        let result = ingest(&fixture, Some("0000000000000000000000000000000000000000"));
        assert!(result.is_err());
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn classify_source_table() {
        let cases = [
            ("abc", Ok("abc".to_string()), SourceCheck::Unchanged),
            (
                "abc",
                Ok("def".to_string()),
                SourceCheck::Moved {
                    actual: "def".to_string(),
                },
            ),
            (
                "abc",
                Err(anyhow::anyhow!("offline")),
                SourceCheck::Unreachable("offline".to_string()),
            ),
        ];
        for (ingested, live, expected) in cases {
            assert_eq!(classify_source(ingested, live), expected);
        }
    }

    #[test]
    fn verify_source_unchanged_reads_the_live_branch_tip() {
        let fixture = stores("source");
        assert_eq!(
            verify_source_unchanged(&GitLoc::Local(fixture.source.clone()), "feat", &fixture.oid),
            SourceCheck::Unchanged
        );
        git(
            &fixture.source,
            &["commit", "--amend", "-m", "moved source"],
        );
        let actual =
            thegn_core::util::git_out(&fixture.source, &["rev-parse", "refs/heads/feat"]).unwrap();
        assert_eq!(
            verify_source_unchanged(&GitLoc::Local(fixture.source.clone()), "feat", &fixture.oid),
            SourceCheck::Moved { actual }
        );
    }

    #[test]
    fn sweep_deletes_orphaned_and_legacy_refs_and_keeps_live_rows() {
        let fixture = stores("source");
        let target = GitLoc::Local(fixture.target.clone());
        let live_loc = GitLoc::Local(fixture.source.clone());
        let live_ref = format!(
            "refs/thegn/mq/{}/0000000000000001",
            row_key(&live_loc, "feat")
        );
        let orphan_ref = format!(
            "refs/thegn/mq/{}/0000000000000002",
            row_key(&GitLoc::Local(PathBuf::from("/gone")), "gone")
        );
        let legacy_ref = "refs/thegn/mq/old-branch";
        let target_oid =
            thegn_core::util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();
        git(&fixture.target, &["update-ref", &live_ref, &target_oid]);
        git(&fixture.target, &["update-ref", &orphan_ref, &target_oid]);
        git(&fixture.target, &["update-ref", legacy_ref, &target_oid]);

        let report =
            sweep_orphan_ingest_refs(&target, || Ok(vec![(live_loc, "feat".to_string())])).unwrap();

        assert_eq!(report.deleted, 2);
        assert_eq!(report.failed, 0);
        assert!(!report.limit_reached);
        assert_eq!(list_refs(&fixture.target), vec![live_ref]);
    }

    #[test]
    fn sweep_with_no_refs_is_a_noop() {
        let fixture = stores("source");
        let report =
            sweep_orphan_ingest_refs(&GitLoc::Local(fixture.target.clone()), || Ok(Vec::new()))
                .unwrap();

        assert_eq!(report, SweepReport::default());
        assert!(list_refs(&fixture.target).is_empty());
    }

    #[test]
    fn sweep_reports_the_limit() {
        let fixture = stores("source");
        let target = GitLoc::Local(fixture.target.clone());
        let target_oid =
            thegn_core::util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();
        let row = row_key(&GitLoc::Local(PathBuf::from("/gone")), "gone");
        for generation in 0..(ORPHAN_SWEEP_LIMIT + 2) {
            let name = format!("refs/thegn/mq/{row}/{generation:016x}");
            git(&fixture.target, &["update-ref", &name, &target_oid]);
        }

        let report = sweep_orphan_ingest_refs(&target, || Ok(Vec::new())).unwrap();

        assert!(report.limit_reached);
        assert_eq!(report.deleted, ORPHAN_SWEEP_LIMIT);
        assert_eq!(report.failed, 0);
        assert_eq!(list_refs(&fixture.target).len(), 2);
    }

    #[test]
    fn sweep_does_not_include_refs_created_after_listing() {
        let fixture = stores("source");
        let target = GitLoc::Local(fixture.target.clone());
        let later_ref = "refs/thegn/mq/created-during-snapshot";
        let target_oid =
            thegn_core::util::git_out(&fixture.target, &["rev-parse", "HEAD"]).unwrap();

        // Model another drain writing a ref after this pass has taken its ref
        // listing but before it reads the queue snapshot.
        let report = sweep_orphan_ingest_refs(&target, || {
            git(&fixture.target, &["update-ref", later_ref, &target_oid]);
            Ok(Vec::new())
        })
        .unwrap();

        assert_eq!(report.deleted, 0);
        assert_eq!(list_refs(&fixture.target), vec![later_ref.to_string()]);
    }
}
