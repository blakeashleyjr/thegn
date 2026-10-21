//! CI run-history refresh machinery (AV group): the ttl-guarded refresh of the
//! active worktree, a round-robin background-worktree sweep that keeps the
//! cross-worktree Across excerpts honest, exponential backoff after fetch
//! failures, and the per-worktree fetch-health note the panel surfaces
//! ("CI provider API rate limited", …). The ticker cadence and the
//! `RefreshKind` plumbing stay in [`crate::hydrate`]; everything here runs off
//! the event loop.

use std::collections::HashMap;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use termwiz::terminal::TerminalWaker;
use thegn_core::store::WorkspaceStore;

const MAX_LOG_INGEST_CALLS: usize = 32;
const MAX_LOG_INGEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOG_INGEST_WALL: Duration = Duration::from_secs(120);
const MAX_INGEST_MEMO_ENTRIES: usize = 512;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct IngestMemoKey {
    worktree: String,
    run: String,
    job: String,
    sha: String,
    started_at: Option<String>,
    finished_at: Option<String>,
}

#[derive(Clone, Debug)]
struct IngestMemoEntry {
    failures: u32,
    backoff_until: i64,
    touched: u64,
    complete: bool,
}

#[derive(Default)]
struct IngestMemo {
    entries: HashMap<IngestMemoKey, IngestMemoEntry>,
    clock: u64,
}

fn ingest_memo() -> &'static Mutex<IngestMemo> {
    static MEMO: std::sync::OnceLock<Mutex<IngestMemo>> = std::sync::OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(IngestMemo::default()))
}

impl IngestMemo {
    fn touch(&mut self, key: IngestMemoKey, now: i64, poll_secs: u64, failed: bool) {
        self.clock = self.clock.wrapping_add(1);
        let touched = self.clock;
        let entry = self.entries.entry(key).or_insert(IngestMemoEntry {
            failures: 0,
            backoff_until: 0,
            touched,
            complete: false,
        });
        entry.touched = touched;
        if failed {
            entry.failures = entry.failures.saturating_add(1);
            entry.backoff_until = thegn_core::time_policy::deadline_seconds(
                now,
                backoff_secs(entry.failures, poll_secs),
            )
            .unwrap_or(i64::MAX);
        } else {
            entry.failures = 0;
            entry.backoff_until = 0;
        }
        if self.entries.len() > MAX_INGEST_MEMO_ENTRIES
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
    }

    fn blocked(&self, key: &IngestMemoKey, now: i64) -> bool {
        self.entries
            .get(key)
            .is_some_and(|entry| !entry.complete && now < entry.backoff_until)
    }

    fn complete(&self, key: &IngestMemoKey) -> bool {
        self.entries.get(key).is_some_and(|entry| entry.complete)
    }

    fn mark_complete(&mut self, key: IngestMemoKey) {
        self.clock = self.clock.wrapping_add(1);
        self.entries.insert(
            key,
            IngestMemoEntry {
                failures: 0,
                backoff_until: 0,
                touched: self.clock,
                complete: true,
            },
        );
        if self.entries.len() > MAX_INGEST_MEMO_ENTRIES
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
        {
            self.entries.remove(&oldest);
        }
    }
}

#[derive(Debug)]
struct CallBudget {
    remaining: usize,
    bytes_remaining: usize,
    deadline: Option<Instant>,
}

impl CallBudget {
    fn take(&mut self) -> bool {
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            tracing::debug!(target: "thegn::ci", "CI log ingestion wall-clock budget exhausted");
            false
        } else if self.remaining == 0 {
            false
        } else {
            self.remaining -= 1;
            true
        }
    }

    fn charge_bytes(&mut self, bytes: usize) -> bool {
        if bytes > self.bytes_remaining {
            self.bytes_remaining = 0;
            return false;
        }
        self.bytes_remaining -= bytes;
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FlightKey {
    active_path: Option<String>,
    generation: Option<u64>,
}

#[derive(Clone)]
struct FlightToken {
    key: FlightKey,
    force: bool,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct CiFlight {
    running: Option<FlightToken>,
    queued: Option<FlightToken>,
}

impl CiFlight {
    fn submit(&mut self, request: FlightToken) -> bool {
        let Some(running) = self.running.as_ref() else {
            self.running = Some(request);
            return true;
        };
        let different_key = running.key != request.key;
        if different_key {
            running.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(queued) = self.queued.as_ref()
            && queued.key != request.key
        {
            queued.cancel.store(true, Ordering::Relaxed);
        }
        let force = self
            .queued
            .as_ref()
            .filter(|queued| queued.key == request.key)
            .is_some_and(|queued| queued.force)
            || request.force;
        self.queued = Some(FlightToken { force, ..request });
        different_key
    }

    fn finish(&mut self) -> Option<FlightToken> {
        let next = self.queued.take();
        self.running = next.as_ref().map(|token| FlightToken {
            key: token.key.clone(),
            force: token.force,
            cancel: Arc::clone(&token.cancel),
        });
        next
    }

    fn clear(&mut self) {
        self.running = None;
        self.queued = None;
    }
}

#[derive(Clone)]
struct RefreshRequest {
    session: crate::session::Session,
    full: thegn_core::config::Config,
    waker: Option<TerminalWaker>,
    token: FlightToken,
    generation: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
}

struct RefreshContext {
    generation: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct CiRefreshCoordinator {
    flight: CiFlight,
    queued: Option<RefreshRequest>,
    shut_down: bool,
    mutations: Vec<Arc<AtomicBool>>,
}

impl CiRefreshCoordinator {
    fn busy(&self) -> bool {
        self.flight.running.is_some() || !self.mutations.is_empty()
    }

    /// Rejects submissions after shutdown; this latch check is authoritative.
    /// Callers may precheck to log a refusal, but this guard remains authoritative.
    fn submit(&mut self, request: RefreshRequest) -> bool {
        if self.shut_down {
            return false;
        }
        let token = FlightToken {
            key: request.token.key.clone(),
            force: request.token.force,
            cancel: Arc::clone(&request.token.cancel),
        };
        let start = self.flight.running.is_none();
        self.flight.submit(token);
        if !start {
            self.queued = Some(request);
        }
        start
    }
}

fn register_mutation(coordinator: &mut CiRefreshCoordinator) -> Option<Arc<AtomicBool>> {
    if coordinator.shut_down {
        return None;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    coordinator.mutations.push(Arc::clone(&cancel));
    Some(cancel)
}

fn finish_mutation(coordinator: &mut CiRefreshCoordinator, cancel: &Arc<AtomicBool>) {
    coordinator
        .mutations
        .retain(|registered| !Arc::ptr_eq(registered, cancel));
}

fn ci_refresh_coordinator() -> &'static Mutex<CiRefreshCoordinator> {
    static COORDINATOR: std::sync::OnceLock<Mutex<CiRefreshCoordinator>> =
        std::sync::OnceLock::new();
    COORDINATOR.get_or_init(|| Mutex::new(CiRefreshCoordinator::default()))
}

fn ci_refresh_idle() -> &'static Condvar {
    static IDLE: Condvar = Condvar::new();
    &IDLE
}

fn cancel_coordinator_for_shutdown(coordinator: &mut CiRefreshCoordinator) {
    coordinator.shut_down = true;
    for cancel in &coordinator.mutations {
        cancel.store(true, Ordering::Relaxed);
    }
    if let Some(running) = coordinator.flight.running.as_ref() {
        running.cancel.store(true, Ordering::Relaxed);
    }
    if let Some(queued) = coordinator.flight.queued.as_ref() {
        queued.cancel.store(true, Ordering::Relaxed);
    }
    coordinator.flight.queued = None;
    coordinator.queued = None;
}

fn wait_coordinator_idle(
    lock: &Mutex<CiRefreshCoordinator>,
    idle: &Condvar,
    deadline: Instant,
) -> bool {
    let mut coordinator = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while coordinator.busy() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let (next, result) = idle
            .wait_timeout(coordinator, remaining)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        coordinator = next;
        if result.timed_out() && coordinator.busy() {
            return false;
        }
    }
    true
}

pub(crate) fn cancel_for_shutdown() {
    let mut coordinator = ci_refresh_coordinator()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cancel_coordinator_for_shutdown(&mut coordinator);
}

pub(crate) fn wait_ci_idle(deadline: Instant) -> bool {
    wait_coordinator_idle(ci_refresh_coordinator(), ci_refresh_idle(), deadline)
}

pub(crate) struct CiMutationGuard {
    lock: &'static Mutex<CiRefreshCoordinator>,
    idle: &'static Condvar,
    cancel: Arc<AtomicBool>,
}

impl CiMutationGuard {
    pub(crate) fn cancel(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }
}

impl Drop for CiMutationGuard {
    fn drop(&mut self) {
        let mut coordinator = self
            .lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        finish_mutation(&mut coordinator, &self.cancel);
        self.idle.notify_all();
    }
}

fn begin_ci_mutation_in(
    lock: &'static Mutex<CiRefreshCoordinator>,
    idle: &'static Condvar,
) -> Option<CiMutationGuard> {
    let cancel = register_mutation(
        &mut lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )?;
    Some(CiMutationGuard { lock, idle, cancel })
}

pub(crate) fn begin_ci_mutation() -> Option<CiMutationGuard> {
    begin_ci_mutation_in(ci_refresh_coordinator(), ci_refresh_idle())
}

#[cfg(test)]
fn clear_ci_refresh_flight() {
    let mut coordinator = ci_refresh_coordinator()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    coordinator.flight.clear();
    coordinator.queued = None;
    ci_refresh_idle().notify_all();
}

struct FlightGuard {
    cancel: Arc<AtomicBool>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        let mut coordinator = ci_refresh_coordinator()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if coordinator
            .flight
            .running
            .as_ref()
            .is_some_and(|running| Arc::ptr_eq(&running.cancel, &self.cancel))
        {
            coordinator.flight.clear();
            coordinator.queued = None;
        }
        ci_refresh_idle().notify_all();
    }
}

#[path = "ci_autofix.rs"]
pub(crate) mod ci_autofix;

/// The CI ticker cadence in 500ms slots, from `[ci] poll_interval_secs`.
/// Clamped to ≥ 5s — every tick is a provider subprocess (`gh run list`), so a
/// faster cadence would thrash. Pure, so it's unit-tested.
pub(crate) fn ci_every_slots(poll_interval_secs: u64) -> u64 {
    thegn_core::time_policy::cadence_slots(poll_interval_secs, 5, 500).get()
}

/// Freshness guard for the CI run-history cache: a non-forced refresh
/// (ticker / on-switch backstop / sweep) is skipped while the cached row is
/// younger than `[ci] ttl_secs`. Forced refreshes (the `g` key, post-mutation)
/// bypass it. `ttl_secs == 0` disables the guard. Pure, so it's unit-tested.
pub(crate) fn ci_cache_is_fresh(fetched_at: Option<i64>, now: i64, ttl_secs: u64) -> bool {
    thegn_core::time_policy::is_fresh(now, fetched_at, ttl_secs)
}

/// Whether the daemon may serve a cached CI run row. The row must be fresh
/// under `[ci] ttl_secs` and belong to a known current branch; legacy rows with
/// an empty cached branch and unknown current branches are never matches.
pub(crate) fn daemon_ci_cache_usable(
    cached_branch: &str,
    current_branch: Option<&str>,
    fetched_at: i64,
    now: i64,
    ttl_secs: u64,
) -> bool {
    ci_cache_is_fresh(Some(fetched_at), now, ttl_secs)
        && thegn_core::ci::cache_branch_matches(cached_branch, current_branch)
}

/// Whether a cached row can suppress the local refresh for the current
/// branch. An unverifiable branch keeps the historical age-only behavior.
pub(crate) fn cache_row_current(
    fetched_at: Option<i64>,
    cached_branch: Option<&str>,
    current_branch: Option<&str>,
    now: i64,
    ttl_secs: u64,
) -> bool {
    ci_cache_is_fresh(fetched_at, now, ttl_secs)
        && match current_branch.filter(|branch| !branch.is_empty() && *branch != "HEAD") {
            None => true,
            Some(branch) => cached_branch
                .is_some_and(|cached| thegn_core::ci::cache_branch_matches(cached, Some(branch))),
        }
}

/// Resolve the local cache identity through gix regardless of the configured
/// read backend. This is used on the refresh tick and when writing local rows,
/// so neither path spawns a process and both agree for unborn or ambiguous refs.
fn local_ci_cache_branch(loc: &thegn_core::remote::GitLoc) -> Option<String> {
    if loc.is_remote() {
        return None;
    }
    let branch = crate::git_handle::get().current_branch(loc).ok()?;
    (!branch.is_empty() && branch != "HEAD").then_some(branch)
}

fn cached_ci_row_is_current(
    loc: &thegn_core::remote::GitLoc,
    cached: Option<&(String, String, i64)>,
    now: i64,
    ttl_secs: u64,
) -> bool {
    let current_branch = if cached
        .is_some_and(|(_, _, fetched_at)| ci_cache_is_fresh(Some(*fetched_at), now, ttl_secs))
    {
        local_ci_cache_branch(loc)
    } else {
        None
    };
    cached.is_some_and(|(_, branch, fetched_at)| {
        cache_row_current(
            Some(*fetched_at),
            Some(branch),
            current_branch.as_deref(),
            now,
            ttl_secs,
        )
    })
}

// --- fetch health: the panel note + failure backoff ------------------------

/// Per-worktree CI fetch health. Process-global (same pattern as hydrate's
/// glyph cache) so it needs no threading through the hydration call sites; the
/// DB stays a cache of provider *data*, not of transient fetch health.
#[derive(Clone, Default)]
struct CiHealth {
    /// Human-readable fetch problem, surfaced as the panel's `ci_note`.
    note: Option<String>,
    /// Consecutive failed fetches (reset on success).
    failures: u32,
    /// Epoch seconds before which non-forced refetches are skipped.
    backoff_until: i64,
}

fn health() -> &'static Mutex<HashMap<String, CiHealth>> {
    static HEALTH: std::sync::OnceLock<Mutex<HashMap<String, CiHealth>>> =
        std::sync::OnceLock::new();
    HEALTH.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The panel-visible fetch note for a worktree ("CI provider API rate
/// limited", "CI provider CLI not installed", …). `None` while healthy.
pub(crate) fn note_for(worktree: &str) -> Option<String> {
    health().lock().ok()?.get(worktree)?.note.clone()
}

/// Exponential refetch backoff after `failures` consecutive errors: the poll
/// interval doubled per extra failure, capped at 5 minutes. Zero failures =
/// no backoff. Pure, so it's unit-tested.
pub(crate) fn backoff_secs(failures: u32, poll_secs: u64) -> u64 {
    if failures == 0 {
        return 0;
    }
    let base = poll_secs.clamp(5, 300);
    base.saturating_mul(1u64 << (failures - 1).min(6)).min(300)
}

/// Record a failed fetch: bump the failure count, arm the backoff window, and
/// set the panel note (with a retry hint once the provider is backing off).
fn record_failure(worktree: &str, message: &str, now: i64, poll_secs: u64) {
    if let Ok(mut map) = health().lock() {
        let h = map.entry(worktree.to_string()).or_default();
        h.failures = h.failures.saturating_add(1);
        let backoff = backoff_secs(h.failures, poll_secs);
        h.backoff_until =
            thegn_core::time_policy::deadline_seconds(now, backoff).unwrap_or(i64::MAX);
        h.note = Some(if h.failures > 1 {
            format!("{message} — retrying in {backoff}s")
        } else {
            message.to_string()
        });
    }
}

/// Record a successful fetch: clear the note and the backoff.
fn record_success(worktree: &str) {
    if let Ok(mut map) = health().lock() {
        map.remove(worktree);
    }
}

fn backoff_active(worktree: &str, now: i64) -> bool {
    health()
        .lock()
        .ok()
        .and_then(|m| m.get(worktree).map(|h| now < h.backoff_until))
        .unwrap_or(false)
}

// --- the refresh itself -----------------------------------------------------

/// Everything the loop does on a CI tick, in one call (keeps `run.rs` lean):
/// kick the off-loop cache refresh (+ the background sweep it carries) and
/// re-poll an open live drill on a still-running run so it updates in place.
pub(crate) fn on_ci_tick(
    session: &crate::session::Session,
    full: &thegn_core::config::Config,
    refresh_tx: &tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
    waker: &TerminalWaker,
    force: bool,
    bar_detail: &mut Option<crate::detail::DetailOverlay>,
    generation: Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>,
) {
    spawn_ci_cache_refresh_with_generation(
        session.clone(),
        full.clone(),
        Some(waker.clone()),
        force,
        generation.clone(),
    );
    if let Some(run) = bar_detail.as_mut().and_then(|ov| ov.live_ci_repoll()) {
        crate::actions::spawn_ci_detail(session, &full.ci, refresh_tx, waker, run, generation);
    }
}

/// Refresh the CI run-history cache for the active worktree (AV group), then
/// sweep one stale background worktree (round-robin) so the Across section
/// converges instead of freezing at last-visit state. Off the event loop:
/// resolves the provider from `[ci]` config + the git remote, fetches recent
/// runs for the current branch via the async `CiProvider`, writes
/// `ci_runs_cache`, and pulses the waker so the panel rehydrates. Non-`force`
/// calls are coalesced by the `[ci] ttl_secs` freshness guard and by the
/// failure backoff; fetch errors surface via [`note_for`].
pub(crate) fn spawn_ci_cache_refresh(
    session: crate::session::Session,
    full: thegn_core::config::Config,
    waker: Option<TerminalWaker>,
    force: bool,
) {
    spawn_ci_cache_refresh_with_generation(session, full, waker, force, None);
}

pub(crate) fn spawn_ci_cache_refresh_with_generation(
    session: crate::session::Session,
    full: thegn_core::config::Config,
    waker: Option<TerminalWaker>,
    force: bool,
    generation: Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>,
) {
    let key = FlightKey {
        active_path: session.active_group().map(|group| group.path.clone()),
        generation: generation.as_ref().map(|(_, value)| *value),
    };
    let token = FlightToken {
        key,
        force,
        cancel: Arc::new(AtomicBool::new(false)),
    };
    let request = RefreshRequest {
        session,
        full,
        waker,
        token,
        generation,
    };
    let admission = {
        let mut coordinator = ci_refresh_coordinator()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if coordinator.shut_down {
            tracing::debug!(target: "thegn::ci", "CI cache refresh refused during shutdown");
            return;
        }
        if !coordinator.submit(request.clone()) {
            return;
        }
        let admission = crate::sched::spawn_bg(move || run_ci_refresh_worker(request));
        if admission != crate::sched::Admission::Admitted {
            coordinator.flight.clear();
            coordinator.queued = None;
            ci_refresh_idle().notify_all();
        }
        admission
    };
    if admission != crate::sched::Admission::Admitted {
        tracing::debug!(target: "thegn::ci", ?admission, "CI cache refresh admission refused");
    }
}

fn run_ci_refresh_worker(mut request: RefreshRequest) {
    let mut guard = FlightGuard {
        cancel: Arc::clone(&request.token.cancel),
    };
    loop {
        guard.cancel = Arc::clone(&request.token.cancel);
        run_ci_refresh_request(&request);
        let next = {
            let mut coordinator = ci_refresh_coordinator()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let token = coordinator.flight.finish();
            match token {
                Some(token) => coordinator.queued.take().map(|mut next| {
                    next.token = token;
                    next
                }),
                None => None,
            }
        };
        let Some(next) = next else {
            return;
        };
        request = next;
    }
}

fn run_ci_refresh_request(request: &RefreshRequest) {
    let cfg = request.full.ci.clone();
    let cwd = request
        .token
        .key
        .active_path
        .as_deref()
        .map(std::path::Path::new)
        .unwrap_or_else(|| std::path::Path::new(""));
    if cwd.is_dir() {
        let loc = thegn_core::remote::GitLoc::for_worktree(cwd);
        refresh_ci_cache_for(
            cwd,
            &loc,
            &cfg,
            &request.full,
            request.waker.as_ref(),
            request.token.force,
            RefreshContext {
                generation: request.generation.clone(),
                cancel: Arc::clone(&request.token.cancel),
            },
        );
    }
    sweep_one_background(
        &request.session,
        cwd,
        &cfg,
        &request.full,
        request.waker.as_ref(),
        request.generation.clone(),
        Arc::clone(&request.token.cancel),
    );
}

/// Round-robin cursor over background worktrees, advanced once per sweep so
/// every worktree gets a turn even when several are stale.
static SWEEP_CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Refresh at most ONE stale background worktree's CI cache (the ttl guard
/// decides staleness), starting from the rotating cursor. Blocking; must run
/// on the bg lane.
fn sweep_one_background(
    session: &crate::session::Session,
    active: &std::path::Path,
    cfg: &thegn_core::config::CiConfig,
    full: &thegn_core::config::Config,
    waker: Option<&TerminalWaker>,
    generation: Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>,
    cancel: Arc<AtomicBool>,
) {
    let others: Vec<&str> = session
        .worktrees
        .iter()
        .map(|g| g.path.as_str())
        .filter(|p| std::path::Path::new(p) != active)
        .collect();
    if others.is_empty() {
        return;
    }
    let locations = thegn_core::db::Db::open()
        .and_then(|db| db.worktrees())
        .map(|rows| thegn_core::remote::WorktreeLocations::from_rows(&rows))
        .unwrap_or_default();
    let start = SWEEP_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for i in 0..others.len() {
        let p = std::path::Path::new(others[(start + i) % others.len()]);
        if !p.is_dir() {
            continue;
        }
        let loc = locations.loc(p);
        // First worktree that actually fetched (fresh ones are skipped by the
        // guard) ends the sweep — one provider subprocess per tick, max.
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if refresh_ci_cache_for(
            p,
            &loc,
            cfg,
            full,
            waker,
            false,
            RefreshContext {
                generation: generation.clone(),
                cancel: Arc::clone(&cancel),
            },
        ) {
            return;
        }
    }
}

/// The blocking body of a CI cache refresh for one worktree location. Returns
/// whether a fetch was actually attempted (`false` when skipped by the
/// freshness/backoff guards or when no provider resolves).
///
/// `host_path` keys the cache + backoff rows — the HOST worktree path, never
/// `loc.path()`: for a provider worktree that is the in-sandbox path (usually
/// `/workspace` for every worktree of an env), so keying by it collides
/// sibling worktrees AND diverges from the host-path readers
/// (`attention_status`, the panel, `pr_clean`).
fn refresh_ci_cache_for(
    host_path: &std::path::Path,
    loc: &thegn_core::remote::GitLoc,
    cfg: &thegn_core::config::CiConfig,
    full: &thegn_core::config::Config,
    waker: Option<&TerminalWaker>,
    force: bool,
    context: RefreshContext,
) -> bool {
    let RefreshContext { generation, cancel } = context;
    use thegn_core::store::CacheStore;
    let Ok(db) = thegn_core::db::Db::open() else {
        return false;
    };
    // Keyed by the HOST worktree path — `loc.path()` collides across sandboxed
    // worktrees, and the Across builder reads by the registry's worktree path.
    let key = thegn_core::remote::GitLoc::worktree_cache_key(host_path);
    let now = thegn_core::util::now();
    if !force {
        let cached = db.get_ci_cache_entry(&key).ok().flatten();
        let row_is_current = cached_ci_row_is_current(loc, cached.as_ref(), now, cfg.ttl_secs);
        if row_is_current || backoff_active(&key, now) {
            return false;
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    let Some(client) = thegn_svc::ci::provider_for_cancellable(loc, cfg, Arc::clone(&cancel))
    else {
        return false;
    };
    let branch = if loc.is_remote() {
        loc.git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .filter(|branch| !branch.is_empty())
    } else {
        local_ci_cache_branch(loc)
    };
    match client.runs(
        loc,
        branch.as_deref(),
        cfg.max_runs.min(thegn_core::config_ci::MAX_CI_RUNS),
    ) {
        Ok(_) if cancel.load(Ordering::Relaxed) => return false,
        Ok(result) => {
            let runs = result.runs;
            if result.discarded_rows > 0 {
                let message = format!(
                    "CI provider returned an incomplete run list ({} malformed row(s)); keeping stale cache",
                    result.discarded_rows
                );
                tracing::warn!(target: "thegn::ci", discarded_rows = result.discarded_rows, "{message}");
                if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
                    return false;
                }
                record_failure(&key, &message, now, cfg.poll_interval_secs);
            } else {
                if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
                    return false;
                }
                record_success(&key);
                // A CI round trip got through — online evidence for the app-wide holder.
                if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
                    return false;
                }
                thegn_core::connectivity::report_success();
                if let Ok(json) = serde_json::to_string(&thegn_core::ci::CiRunCache {
                    runs: runs.clone(),
                    discarded_rows: result.discarded_rows,
                }) && crate::hydrate_schedule::generation_is_current(generation.as_ref())
                    && let Err(error) =
                        db.put_ci_cache(&key, branch.as_deref().unwrap_or(""), &json)
                {
                    tracing::debug!(target: "thegn::ci", %error, "could not update cached CI runs");
                }
                let mut ingest_budget = CallBudget {
                    remaining: MAX_LOG_INGEST_CALLS,
                    bytes_remaining: MAX_LOG_INGEST_BYTES,
                    deadline: Some(Instant::now() + MAX_LOG_INGEST_WALL),
                };
                ingest_failed_logs_with(IngestArgs {
                    host_path,
                    loc,
                    client: client.as_ref(),
                    cfg,
                    full,
                    db: &db,
                    runs: &runs,
                    budget: &mut ingest_budget,
                    waker,
                    generation: generation.clone(),
                    now: thegn_core::util::now(),
                    cancel: Arc::clone(&cancel),
                });
            }
        }
        Err(_) if cancel.load(Ordering::Relaxed) => return false,
        Err(e) => {
            if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
                return false;
            }
            // The stale cache stays (better than blank), but the panel gets an
            // honest note instead of silently rendering old data as current.
            // Only a transient (network) failure is offline evidence — an
            // auth/rate-limit error means we reached the provider fine.
            if e.is_transient()
                && crate::hydrate_schedule::generation_is_current(generation.as_ref())
            {
                thegn_core::connectivity::report_failure();
            }
            if crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
                record_failure(&key, &e.message(), now, cfg.poll_interval_secs);
            }
        }
    }
    if crate::hydrate_schedule::generation_is_current(generation.as_ref())
        && let Some(w) = waker
        && let Err(error) = w.wake()
    {
        tracing::debug!(target: "thegn::ci", %error, "could not wake after CI refresh");
    }
    true
}

/// Inputs for one bounded log-ingestion pass, grouped so tests exercise the
/// same provider, cache and budget path used by refreshes.
struct IngestArgs<'a> {
    host_path: &'a std::path::Path,
    loc: &'a thegn_core::remote::GitLoc,
    client: &'a dyn thegn_svc::ci::CiProvider,
    cfg: &'a thegn_core::config::CiConfig,
    full: &'a thegn_core::config::Config,
    db: &'a thegn_core::db::Db,
    runs: &'a [thegn_core::ci::CiRun],
    budget: &'a mut CallBudget,
    waker: Option<&'a TerminalWaker>,
    generation: Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>,
    now: i64,
    cancel: Arc<AtomicBool>,
}

fn ingest_failed_logs_with(args: IngestArgs<'_>) {
    use thegn_core::ci::CiState;
    use thegn_core::store::CacheStore;
    let IngestArgs {
        host_path,
        loc,
        client,
        cfg,
        full,
        db,
        runs,
        budget,
        waker,
        generation,
        now,
        cancel,
    } = args;
    let log_cache_runs = cfg
        .log_cache_runs
        .min(thegn_core::config_ci::MAX_CI_LOG_CACHE_RUNS);
    if log_cache_runs == 0 {
        return;
    }
    let key = thegn_core::remote::GitLoc::worktree_cache_key(host_path);
    let terminal_ids: Vec<String> = runs
        .iter()
        .filter(|r| r.state.is_terminal())
        .map(|r| r.id.clone())
        .collect();
    let retained = thegn_core::ci_log::retained_run_ids(&terminal_ids, log_cache_runs);
    let call_budget = budget;
    let mut ordered: Vec<&thegn_core::ci::CiRun> = runs
        .iter()
        .filter(|r| retained.contains(&r.id) && r.state == CiState::Fail)
        .collect();
    ordered.sort_by_key(|run| {
        let run_key = IngestMemoKey {
            worktree: key.clone(),
            run: run.id.clone(),
            job: String::new(),
            sha: run.sha.clone(),
            started_at: run.started_at.clone(),
            finished_at: run.finished_at.clone(),
        };
        let memo = match ingest_memo().lock() {
            Ok(memo) => Some(memo),
            Err(error) => {
                tracing::debug!(target: "thegn::ci", %error, "could not inspect CI ingestion memo");
                None
            }
        };
        if memo.as_ref().is_some_and(|m| m.complete(&run_key)) {
            (2_u8, 0_i64)
        } else if memo.as_ref().is_some_and(|m| {
            m.entries.keys().any(|k| {
                k.worktree == key
                    && k.run == run.id
                    && k.sha == run.sha
                    && k.started_at == run.started_at
                    && k.finished_at == run.finished_at
            })
        }) {
            (1_u8, 0_i64)
        } else {
            (0_u8, 0_i64)
        }
    });
    'runs: for run in ordered {
        if cancel.load(Ordering::Relaxed)
            || !crate::hydrate_schedule::generation_is_current(generation.as_ref())
        {
            return;
        }
        let run_key = IngestMemoKey {
            worktree: key.clone(),
            run: run.id.clone(),
            job: String::new(),
            sha: run.sha.clone(),
            started_at: run.started_at.clone(),
            finished_at: run.finished_at.clone(),
        };
        if ingest_memo()
            .lock()
            .ok()
            .is_some_and(|memo| memo.complete(&run_key))
        {
            continue;
        }
        if ingest_memo().lock().ok().is_some_and(|memo| {
            memo.entries.iter().any(|(memo_key, entry)| {
                memo_key.worktree == key
                    && memo_key.run == run.id
                    && memo_key.sha == run.sha
                    && memo_key.started_at == run.started_at
                    && memo_key.finished_at == run.finished_at
                    && !entry.complete
                    && now < entry.backoff_until
            })
        }) {
            continue;
        }
        if !call_budget.take() {
            tracing::debug!(target: "thegn::ci", budget = MAX_LOG_INGEST_CALLS, "CI log ingestion provider-call budget exhausted");
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let detail = match client.run_detail(loc, &run.id) {
            Ok(detail) => detail,
            Err(_) => {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                if let Ok(mut memo) = ingest_memo().lock() {
                    memo.touch(run_key, now, cfg.poll_interval_secs, true);
                }
                continue;
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if detail.discarded_jobs > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows = detail.discarded_jobs, "CI detail omitted malformed job rows");
        }
        if let Some(incomplete) = detail.incomplete {
            tracing::warn!(target: "thegn::ci", reason = %incomplete.reason(), "CI job list is incomplete");
        }
        if detail.discarded_jobs > 0 || !detail.is_complete() {
            // A partial or truncated job list is not enough evidence for log ingestion or an autofix handoff.
            if let Ok(mut memo) = ingest_memo().lock() {
                memo.touch(run_key, now, cfg.poll_interval_secs, true);
            }
            continue;
        }
        // A large matrix must not turn one refresh into an unbounded provider
        // fan-out. Four-at-a-time was the old drill policy; the cache worker
        // keeps the same finite work shape with a hard per-run ceiling.
        let failed_jobs: Vec<&thegn_core::ci::CiJob> = detail
            .run
            .jobs
            .iter()
            .filter(|j| j.state == CiState::Fail)
            .take(16)
            .collect();
        let mut run_complete = true;
        for job in failed_jobs {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let job_key = IngestMemoKey {
                worktree: key.clone(),
                run: run.id.clone(),
                job: job.id.clone(),
                sha: run.sha.clone(),
                started_at: run.started_at.clone(),
                finished_at: run.finished_at.clone(),
            };
            if ingest_memo()
                .lock()
                .ok()
                .is_some_and(|memo| memo.blocked(&job_key, now))
            {
                run_complete = false;
                continue;
            }
            let cached = db.get_ci_log(&key, &run.id, &job.id).ok().flatten();
            if cached.as_ref().is_some_and(|old| old.head_sha == run.sha) {
                if let Ok(mut memo) = ingest_memo().lock() {
                    memo.touch(job_key, now, cfg.poll_interval_secs, false);
                }
                continue;
            }
            if call_budget.bytes_remaining == 0 {
                tracing::debug!(target: "thegn::ci", budget = MAX_LOG_INGEST_BYTES, "CI log ingestion byte budget exhausted");
                break 'runs;
            }
            if !call_budget.take() {
                tracing::debug!(target: "thegn::ci", budget = MAX_LOG_INGEST_CALLS, "CI log ingestion provider-call budget exhausted");
                break 'runs;
            }
            let log = match client.logs(loc, &run.id, &job.id) {
                Ok(log) => log,
                Err(_) => {
                    if cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    run_complete = false;
                    if let Ok(mut memo) = ingest_memo().lock() {
                        memo.touch(job_key, now, cfg.poll_interval_secs, true);
                    }
                    continue;
                }
            };
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let within_byte_budget = call_budget.charge_bytes(log.text.len());
            if !within_byte_budget {
                tracing::debug!(target: "thegn::ci", budget = MAX_LOG_INGEST_BYTES, "CI log ingestion byte budget exhausted");
            }
            let mut entry = thegn_core::ci_log::CiLogEntry::new(
                &key,
                &run.id,
                &job.id,
                &job.name,
                &log.text,
                cfg.log_tail_lines
                    .min(thegn_core::ci_log::HARD_MAX_LOG_LINES),
                cfg.log_max_bytes
                    .min(thegn_core::ci_log::HARD_MAX_LOG_BYTES),
                thegn_core::util::now(),
            );
            entry.truncated |= log.truncated;
            entry.head_sha = run.sha.clone();
            if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
                run_complete = false;
                continue;
            }
            match db.put_ci_log(&entry) {
                Ok(()) if crate::hydrate_schedule::generation_is_current(generation.as_ref()) => {
                    ci_autofix::consider(full, db, &entry);
                    if let Ok(mut memo) = ingest_memo().lock() {
                        memo.touch(job_key, now, cfg.poll_interval_secs, false);
                    }
                }
                Ok(()) => run_complete = false,
                Err(error) => {
                    tracing::debug!(target: "thegn::ci", %error, "could not cache fetched CI log");
                    run_complete = false;
                }
            }
            if !within_byte_budget {
                break 'runs;
            }
        }
        if run_complete
            && !cancel.load(Ordering::Relaxed)
            && let Ok(mut memo) = ingest_memo().lock()
        {
            memo.mark_complete(run_key);
        }
    }
    if cancel.load(Ordering::Relaxed)
        || !crate::hydrate_schedule::generation_is_current(generation.as_ref())
    {
        return;
    }
    let keep: Vec<String> = retained.into_iter().collect();
    if let Err(error) = db.retain_ci_logs(&key, &keep) {
        tracing::debug!(target: "thegn::ci", %error, "could not prune cached CI logs");
    }
    if let Some(w) = waker
        && let Err(error) = w.wake()
    {
        tracing::debug!(target: "thegn::ci", %error, "could not wake after CI log ingestion");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static FLIGHT_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct FakeCi {
        log_failures: HashSet<String>,
        detail_failures: HashSet<String>,
        details: HashMap<String, thegn_svc::ci::CiRunDetail>,
        detail_calls: AtomicUsize,
        log_calls: Mutex<Vec<(String, String)>>,
        log_text: String,
    }

    impl FakeCi {
        fn new(log_failures: &[&str]) -> Self {
            Self {
                log_failures: log_failures.iter().map(|id| (*id).to_string()).collect(),
                detail_failures: HashSet::new(),
                details: HashMap::new(),
                detail_calls: AtomicUsize::new(0),
                log_calls: Mutex::new(Vec::new()),
                log_text: String::new(),
            }
        }

        fn with_detail(mut self, run_id: &str, sha: &str) -> Self {
            self.details
                .insert(run_id.to_string(), ci_detail(run_id, sha));
            self
        }

        fn with_incomplete_detail(mut self, run_id: &str, sha: &str) -> Self {
            let mut detail = ci_detail(run_id, sha);
            detail.incomplete = Some(thegn_svc::ci::CiDetailIncomplete::PageBudget { pages: 50 });
            self.details.insert(run_id.to_string(), detail);
            self
        }

        fn with_log_text(mut self, text: String) -> Self {
            self.log_text = text;
            self
        }

        fn with_detail_failure(mut self, run_id: &str) -> Self {
            self.detail_failures.insert(run_id.to_string());
            self
        }
    }

    impl thegn_svc::ci::CiProvider for FakeCi {
        fn system(&self) -> thegn_core::ci::CiSystem {
            thegn_core::ci::CiSystem::GithubActions
        }

        fn runs(
            &self,
            _: &thegn_core::remote::GitLoc,
            _: Option<&str>,
            _: usize,
        ) -> Result<thegn_svc::ci::CiRunList, thegn_core::ci::CiError> {
            Ok(thegn_svc::ci::CiRunList {
                runs: Vec::new(),
                discarded_rows: 0,
            })
        }

        fn run_detail(
            &self,
            _: &thegn_core::remote::GitLoc,
            run_id: &str,
        ) -> Result<thegn_svc::ci::CiRunDetail, thegn_core::ci::CiError> {
            self.detail_calls.fetch_add(1, Ordering::Relaxed);
            if self.detail_failures.contains(run_id) {
                return Err(thegn_core::ci::CiError::Offline);
            }
            self.details
                .get(run_id)
                .cloned()
                .ok_or(thegn_core::ci::CiError::NotFound)
        }

        fn logs(
            &self,
            _: &thegn_core::remote::GitLoc,
            run_id: &str,
            job_id: &str,
        ) -> Result<thegn_core::ci::CiLog, thegn_core::ci::CiError> {
            self.log_calls
                .lock()
                .expect("log-call lock")
                .push((run_id.to_string(), job_id.to_string()));
            if self.log_failures.contains(run_id) {
                Err(thegn_core::ci::CiError::Offline)
            } else {
                Ok(thegn_core::ci::CiLog {
                    text: self.log_text.clone(),
                    truncated: false,
                })
            }
        }

        fn workflows(
            &self,
            _: &thegn_core::remote::GitLoc,
        ) -> Result<Vec<thegn_core::ci::CiWorkflow>, thegn_core::ci::CiError> {
            Ok(Vec::new())
        }

        fn trigger(
            &self,
            _: &thegn_core::remote::GitLoc,
            _: &str,
            _: &[(String, String)],
        ) -> Result<(), thegn_core::ci::CiError> {
            Err(thegn_core::ci::CiError::NotConfigured)
        }

        fn rerun(
            &self,
            _: &thegn_core::remote::GitLoc,
            _: &str,
            _: thegn_core::ci::RerunScope,
        ) -> Result<(), thegn_core::ci::CiError> {
            Err(thegn_core::ci::CiError::NotConfigured)
        }

        fn cancel(
            &self,
            _: &thegn_core::remote::GitLoc,
            _: &str,
        ) -> Result<(), thegn_core::ci::CiError> {
            Err(thegn_core::ci::CiError::NotConfigured)
        }

        fn caps(&self) -> thegn_core::ci::CiCaps {
            thegn_core::ci::CiCaps::default()
        }
    }

    impl thegn_core::seam::Probe for FakeCi {
        fn probe(&self) -> thegn_core::seam::ProbeReport {
            thegn_core::seam::ProbeReport::new("ci", "fake", thegn_core::seam::Availability::Ready)
                .with_caps(&thegn_core::ci::CiCaps::default())
        }
    }

    fn ci_run(id: &str, sha: &str) -> thegn_core::ci::CiRun {
        thegn_core::ci::CiRun {
            id: id.to_string(),
            sha: sha.to_string(),
            state: thegn_core::ci::CiState::Fail,
            ..thegn_core::ci::CiRun::default()
        }
    }

    fn ci_detail(run_id: &str, sha: &str) -> thegn_svc::ci::CiRunDetail {
        let mut run = ci_run(run_id, sha);
        run.jobs.push(thegn_core::ci::CiJob {
            id: format!("job-{run_id}"),
            name: "failing job".to_string(),
            state: thegn_core::ci::CiState::Fail,
            ..thegn_core::ci::CiJob::default()
        });
        thegn_svc::ci::CiRunDetail {
            run,
            discarded_jobs: 0,
            incomplete: None,
        }
    }

    fn ci_detail_with_job(run_id: &str, sha: &str, job_id: &str) -> thegn_svc::ci::CiRunDetail {
        let mut detail = ci_detail(run_id, sha);
        detail.run.jobs[0].id = job_id.to_string();
        detail
    }

    fn ingest_test(
        temp: &tempfile::TempDir,
        client: &FakeCi,
        runs: &[thegn_core::ci::CiRun],
        budget_size: usize,
        now: i64,
    ) {
        ingest_test_with_deadline(temp, client, runs, budget_size, now, None);
    }

    fn ingest_test_with_deadline(
        temp: &tempfile::TempDir,
        client: &FakeCi,
        runs: &[thegn_core::ci::CiRun],
        budget_size: usize,
        now: i64,
        deadline: Option<Instant>,
    ) {
        let db = thegn_core::db::Db::open_at(&temp.path().join("ci.db")).expect("temp DB");
        let loc = thegn_core::remote::GitLoc::for_worktree(temp.path());
        let cfg = thegn_core::config::CiConfig::default();
        let full = thegn_core::config::Config::default();
        let mut budget = CallBudget {
            remaining: budget_size,
            bytes_remaining: MAX_LOG_INGEST_BYTES,
            deadline,
        };
        ingest_failed_logs_with(IngestArgs {
            host_path: temp.path(),
            loc: &loc,
            client,
            cfg: &cfg,
            full: &full,
            db: &db,
            runs,
            budget: &mut budget,
            waker: None,
            generation: None,
            now,
            cancel: Arc::new(AtomicBool::new(false)),
        });
    }

    fn private_tempdir() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        thegn_core::fsperm::restrict_dir_to_owner(temp.path()).expect("restrict tempdir");
        temp
    }

    #[test]
    fn failed_log_is_backed_off_across_same_time_refreshes() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&["run"]).with_detail("run", "sha");
        let runs = [ci_run("run", "sha")];
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 10_000);
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 10_000);
        assert_eq!(provider.log_calls.lock().expect("calls").len(), 1);
    }

    #[test]
    fn failed_detail_is_backed_off_across_same_time_refreshes() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&[]).with_detail_failure("run");
        let runs = [ci_run("run", "sha")];
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 15_000);
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 15_000);
        assert_eq!(provider.detail_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn incomplete_detail_skips_log_ingestion() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&[]).with_incomplete_detail("run", "sha");
        let runs = [ci_run("run", "sha")];
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 15_500);
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 15_500);
        assert!(provider.log_calls.lock().expect("calls").is_empty());
        assert_eq!(provider.detail_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn complete_cached_run_skips_detail_on_next_refresh() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&[]).with_detail("run", "sha");
        let runs = [ci_run("run", "sha")];
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 20_000);
        let details = provider.detail_calls.load(Ordering::Relaxed);
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 20_000);
        assert_eq!(provider.detail_calls.load(Ordering::Relaxed), details);
    }

    #[test]
    fn completed_run_attempt_does_not_skip_provider_rerun() {
        let temp = private_tempdir();
        let mut provider = FakeCi::new(&[]).with_detail("run", "sha");
        let mut first = ci_run("run", "sha");
        first.finished_at = Some("2026-10-12T10:00:00Z".to_string());
        ingest_test(
            &temp,
            &provider,
            &[first.clone()],
            MAX_LOG_INGEST_CALLS,
            20_500,
        );

        provider.details.insert(
            "run".to_string(),
            ci_detail_with_job("run", "sha", "job-rerun"),
        );
        let mut rerun = first;
        rerun.finished_at = Some("2026-10-12T10:05:00Z".to_string());
        ingest_test(&temp, &provider, &[rerun], MAX_LOG_INGEST_CALLS, 20_500);

        assert_eq!(provider.detail_calls.load(Ordering::Relaxed), 2);
        assert!(
            provider
                .log_calls
                .lock()
                .expect("calls")
                .iter()
                .any(|(_, job)| job == "job-rerun")
        );
    }

    #[test]
    fn untouched_older_run_gets_budget_before_backed_off_newer_run() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&["newest"])
            .with_detail("newest", "new-sha")
            .with_detail("older", "old-sha");
        let runs = [ci_run("newest", "new-sha"), ci_run("older", "old-sha")];
        ingest_test(&temp, &provider, &runs, 2, 30_000);
        ingest_test(&temp, &provider, &runs, 2, 30_000);
        assert!(
            provider
                .log_calls
                .lock()
                .expect("calls")
                .iter()
                .any(|(run, _)| run == "older")
        );
    }

    #[test]
    fn empty_log_is_cached_and_not_refetched() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&[]).with_detail("run", "sha");
        let runs = [ci_run("run", "sha")];
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 40_000);
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 40_000);
        assert_eq!(provider.log_calls.lock().expect("calls").len(), 1);
    }

    #[test]
    fn enormous_ttl_and_clock_rollback_keep_cached_ci_quiet() {
        for ttl in [i64::MAX as u64 - 1, i64::MAX as u64 + 1, u64::MAX] {
            assert!(ci_cache_is_fresh(Some(1), 1000, ttl));
        }
        assert!(ci_cache_is_fresh(Some(i64::MAX), i64::MIN, 1));
        assert!(!ci_cache_is_fresh(Some(i64::MIN), i64::MAX, 1));
    }

    #[test]
    fn ci_cadence_honors_config_and_clamps() {
        // `[ci] poll_interval_secs` maps to 500ms ticker slots…
        assert_eq!(ci_every_slots(30), 60);
        assert_eq!(ci_every_slots(20), 40);
        // …and is clamped to ≥ 5s (a provider subprocess per tick).
        assert_eq!(ci_every_slots(0), 10);
        assert_eq!(ci_every_slots(1), 10);
    }

    #[test]
    fn log_ingestion_call_budget_stops_at_its_cap() {
        let mut budget = CallBudget {
            remaining: 2,
            bytes_remaining: MAX_LOG_INGEST_BYTES,
            deadline: None,
        };
        assert!(budget.take());
        assert!(budget.take());
        assert!(!budget.take());
        assert!(!budget.take());
    }

    #[test]
    fn log_ingestion_byte_budget_stops_before_fetching_later_runs() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&[])
            .with_detail("run-a", "sha-a")
            .with_detail("run-b", "sha-b")
            .with_log_text("x".repeat(MAX_LOG_INGEST_BYTES + 1));
        let runs = [ci_run("run-a", "sha-a"), ci_run("run-b", "sha-b")];
        ingest_test(&temp, &provider, &runs, MAX_LOG_INGEST_CALLS, 60_000);
        ingest_test(&temp, &provider, &runs[..1], MAX_LOG_INGEST_CALLS, 60_001);
        assert_eq!(provider.log_calls.lock().expect("calls").len(), 1);
    }

    #[test]
    fn cancelled_ingestion_makes_no_provider_calls_or_failure_memo() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&[]).with_detail("cancelled-run", "sha");
        let runs = [ci_run("cancelled-run", "sha")];
        let db = thegn_core::db::Db::open_at(&temp.path().join("ci.db")).expect("temp DB");
        let loc = thegn_core::remote::GitLoc::for_worktree(temp.path());
        let cfg = thegn_core::config::CiConfig::default();
        let full = thegn_core::config::Config::default();
        let cancel = Arc::new(AtomicBool::new(true));
        let mut budget = CallBudget {
            remaining: MAX_LOG_INGEST_CALLS,
            bytes_remaining: MAX_LOG_INGEST_BYTES,
            deadline: None,
        };
        ingest_failed_logs_with(IngestArgs {
            host_path: temp.path(),
            loc: &loc,
            client: &provider,
            cfg: &cfg,
            full: &full,
            db: &db,
            runs: &runs,
            budget: &mut budget,
            waker: None,
            generation: None,
            now: 70_000,
            cancel,
        });
        assert_eq!(provider.detail_calls.load(Ordering::Relaxed), 0);
        let worktree = thegn_core::remote::GitLoc::worktree_cache_key(temp.path());
        assert!(
            !ingest_memo()
                .lock()
                .expect("memo")
                .entries
                .keys()
                .any(|key| { key.worktree == worktree && key.run == "cancelled-run" })
        );
    }

    #[test]
    fn log_ingestion_budget_rejects_expired_deadline() {
        let mut budget = CallBudget {
            remaining: 2,
            bytes_remaining: MAX_LOG_INGEST_BYTES,
            deadline: Some(Instant::now() - Duration::from_secs(1)),
        };
        assert!(!budget.take());
        assert_eq!(budget.remaining, 2);
    }

    #[test]
    fn expired_log_ingestion_budget_makes_no_provider_log_calls() {
        let temp = private_tempdir();
        let provider = FakeCi::new(&[]).with_detail("run", "sha");
        let runs = [ci_run("run", "sha")];
        ingest_test_with_deadline(
            &temp,
            &provider,
            &runs,
            MAX_LOG_INGEST_CALLS,
            50_000,
            Some(Instant::now() - Duration::from_secs(1)),
        );
        assert!(provider.log_calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn ci_cache_freshness_guard() {
        // Fresh row within the ttl → skip the refetch.
        assert!(ci_cache_is_fresh(Some(100), 120, 30));
        // Aged past the ttl → refetch.
        assert!(!ci_cache_is_fresh(Some(100), 130, 30));
        assert!(!ci_cache_is_fresh(Some(100), 131, 30));
        // No cached row yet → always fetch.
        assert!(!ci_cache_is_fresh(None, 120, 30));
        // ttl 0 disables the guard entirely.
        assert!(!ci_cache_is_fresh(Some(120), 120, 0));
        // A clock that went backwards must not wedge the guard forever
        // (saturating_sub keeps the age ≥ 0, so it reads as "just fetched").
        assert!(ci_cache_is_fresh(Some(200), 120, 30));
    }

    #[test]
    fn daemon_ci_cache_usable_requires_fresh_row_for_current_branch() {
        assert!(daemon_ci_cache_usable("main", Some("main"), 100, 120, 30));
        assert!(!daemon_ci_cache_usable("main", Some("main"), 89, 120, 30));
        assert!(!daemon_ci_cache_usable("main", Some("main"), 120, 120, 0));
        assert!(!daemon_ci_cache_usable("other", Some("main"), 100, 120, 30));
    }

    #[test]
    fn daemon_ci_cache_usable_rejects_unknown_identity() {
        assert!(!daemon_ci_cache_usable("", Some("main"), 100, 120, 30));
        assert!(!daemon_ci_cache_usable("main", None, 100, 120, 30));
        assert!(!daemon_ci_cache_usable("", Some(""), 100, 120, 30));
        assert!(!daemon_ci_cache_usable("HEAD", Some("HEAD"), 100, 120, 30));
    }

    #[test]
    fn branch_switch_makes_a_fresh_row_stale() {
        assert!(!cache_row_current(Some(100), Some("b"), Some("a"), 120, 30));
        assert!(cache_row_current(Some(100), Some("a"), Some("a"), 120, 30));
        assert!(!cache_row_current(Some(100), Some(""), Some("a"), 120, 30));
    }

    #[test]
    fn unknown_or_detached_branch_keeps_age_only_freshness() {
        for current_branch in [None, Some("HEAD"), Some("")] {
            assert_eq!(
                cache_row_current(Some(100), Some("b"), current_branch, 120, 30),
                ci_cache_is_fresh(Some(100), 120, 30)
            );
            assert_eq!(
                cache_row_current(Some(100), Some("b"), current_branch, 131, 30),
                ci_cache_is_fresh(Some(100), 131, 30)
            );
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test fixture: create an isolated unborn Git worktree"
    )]
    #[test]
    fn unborn_head_fresh_cached_row_is_not_refetched() {
        use thegn_core::store::CacheStore;

        let temp = private_tempdir();
        let worktree = temp.path().join("unborn");
        std::fs::create_dir(&worktree).expect("create unborn worktree");
        let status = thegn_core::util::git_cmd(&worktree)
            .args(["init", "-q", "-b", "topic"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("initialize unborn worktree");
        assert!(status.success(), "git init failed for unborn worktree");

        let loc = thegn_core::remote::GitLoc::for_worktree(&worktree);
        let branch = local_ci_cache_branch(&loc).expect("gix resolves unborn branch name");
        assert_eq!(branch, "topic");

        let db = thegn_core::db::Db::open_at(&temp.path().join("ci.db")).expect("temp DB");
        let key = thegn_core::remote::GitLoc::worktree_cache_key(&worktree);
        db.put_ci_cache(&key, &branch, r#"{"runs":[],"discarded_rows":0}"#)
            .expect("seed fresh CI cache row");
        let cached = db.get_ci_cache_entry(&key).expect("read cache row");
        assert!(cached_ci_row_is_current(
            &loc,
            cached.as_ref(),
            thegn_core::util::now(),
            30
        ));
    }

    #[test]
    fn failure_backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(0, 30), 0);
        assert_eq!(backoff_secs(1, 30), 30);
        assert_eq!(backoff_secs(2, 30), 60);
        assert_eq!(backoff_secs(3, 30), 120);
        assert_eq!(backoff_secs(4, 30), 240);
        // Capped at 5 minutes, however long the streak…
        assert_eq!(backoff_secs(5, 30), 300);
        assert_eq!(backoff_secs(100, 30), 300);
        // …and a degenerate poll interval is clamped into the sane band.
        assert_eq!(backoff_secs(1, 0), 5);
        assert_eq!(backoff_secs(1, 100_000), 300);
    }

    #[test]
    fn health_notes_lifecycle() {
        let wt = format!("/tmp/ci-health-test-{}", std::process::id());
        assert_eq!(note_for(&wt), None);
        record_failure(&wt, "CI provider API rate limited", 1000, 30);
        assert_eq!(
            note_for(&wt).as_deref(),
            Some("CI provider API rate limited")
        );
        assert!(backoff_active(&wt, 1010));
        assert!(!backoff_active(&wt, 2000));
        // A second consecutive failure notes the retry horizon.
        record_failure(&wt, "CI provider API rate limited", 2000, 30);
        assert_eq!(
            note_for(&wt).as_deref(),
            Some("CI provider API rate limited — retrying in 60s")
        );
        // Success clears note + backoff.
        record_success(&wt);
        assert_eq!(note_for(&wt), None);
        assert!(!backoff_active(&wt, 2001));
    }

    fn flight_token(path: &str, generation: Option<u64>, force: bool) -> FlightToken {
        FlightToken {
            key: FlightKey {
                active_path: Some(path.to_string()),
                generation,
            },
            force,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn ci_flight_coalesces_same_key_and_keeps_force_sticky() {
        let mut flight = CiFlight::default();
        assert!(flight.submit(flight_token("/wt", Some(4), false)));
        assert!(!flight.submit(flight_token("/wt", Some(4), true)));
        let queued = flight.finish().expect("coalesced queued refresh");
        assert!(queued.force);
        assert_eq!(
            queued.key,
            FlightKey {
                active_path: Some("/wt".into()),
                generation: Some(4)
            }
        );
        assert!(flight.finish().is_none());
    }

    #[test]
    fn ci_flight_cancels_running_request_when_path_or_generation_changes() {
        let mut flight = CiFlight::default();
        let first = flight_token("/wt-a", Some(4), false);
        let cancel = Arc::clone(&first.cancel);
        assert!(flight.submit(first));
        assert!(flight.submit(flight_token("/wt-b", Some(4), false)));
        assert!(cancel.load(Ordering::Relaxed));
        let queued = flight.finish().expect("changed path refresh is queued");
        assert_eq!(queued.key.active_path.as_deref(), Some("/wt-b"));

        let second = flight_token("/wt-a", Some(4), false);
        let cancel = Arc::clone(&second.cancel);
        assert!(flight.submit(second));
        assert!(flight.submit(flight_token("/wt-a", Some(5), false)));
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn ci_shutdown_cancels_local_running_and_queued_flights_and_waits() {
        let lock = Arc::new(Mutex::new(CiRefreshCoordinator::default()));
        let idle = Arc::new(Condvar::new());
        let running = flight_token("/running", None, false);
        let running_cancel = Arc::clone(&running.cancel);
        let queued = flight_token("/queued", None, true);
        let queued_cancel = Arc::clone(&queued.cancel);
        {
            let mut coordinator = lock.lock().expect("local coordinator");
            coordinator.flight.submit(running);
            coordinator.flight.queued = Some(queued.clone());
            coordinator.queued = Some(RefreshRequest {
                session: crate::session::Session::default(),
                full: thegn_core::config::Config::default(),
                waker: None,
                token: queued.clone(),
                generation: None,
            });
            cancel_coordinator_for_shutdown(&mut coordinator);
            assert!(coordinator.shut_down);
            assert!(coordinator.queued.is_none());
            assert!(coordinator.flight.queued.is_none());
            assert!(!coordinator.submit(RefreshRequest {
                session: crate::session::Session::default(),
                full: thegn_core::config::Config::default(),
                waker: None,
                token: flight_token("/later", None, false),
                generation: None,
            }));
            assert!(
                coordinator
                    .flight
                    .running
                    .as_ref()
                    .is_some_and(|token| Arc::ptr_eq(&token.cancel, &running_cancel))
            );
            assert!(coordinator.queued.is_none());
            assert!(coordinator.flight.queued.is_none());
        }
        assert!(running_cancel.load(Ordering::Relaxed));
        assert!(queued_cancel.load(Ordering::Relaxed));
        assert!(!wait_coordinator_idle(&lock, &idle, Instant::now()));

        let worker_lock = Arc::clone(&lock);
        let worker_idle = Arc::clone(&idle);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let mut coordinator = worker_lock.lock().expect("local coordinator");
            coordinator.flight.clear();
            worker_idle.notify_all();
        });
        assert!(wait_coordinator_idle(
            &lock,
            &idle,
            Instant::now() + Duration::from_secs(1),
        ));
        worker.join().expect("local flight completion");
    }

    #[test]
    fn ci_shutdown_cancels_in_flight_mutations_and_refuses_new_ones() {
        let mut coordinator = CiRefreshCoordinator::default();
        let cancel = register_mutation(&mut coordinator).expect("mutation admitted");
        assert!(!cancel.load(Ordering::Relaxed));

        cancel_coordinator_for_shutdown(&mut coordinator);

        assert!(cancel.load(Ordering::Relaxed));
        assert!(register_mutation(&mut coordinator).is_none());
    }

    #[test]
    fn ci_idle_wait_covers_in_flight_mutations() {
        let lock = Arc::new(Mutex::new(CiRefreshCoordinator::default()));
        let idle = Arc::new(Condvar::new());
        let cancel = register_mutation(&mut lock.lock().expect("local coordinator"))
            .expect("mutation admitted");
        assert!(!wait_coordinator_idle(&lock, &idle, Instant::now()));

        let worker_lock = Arc::clone(&lock);
        let worker_idle = Arc::clone(&idle);
        let worker_cancel = Arc::clone(&cancel);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            let mut coordinator = worker_lock.lock().expect("local coordinator");
            finish_mutation(&mut coordinator, &worker_cancel);
            worker_idle.notify_all();
        });
        assert!(wait_coordinator_idle(
            &lock,
            &idle,
            Instant::now() + Duration::from_secs(1),
        ));
        worker.join().expect("local mutation completion");
    }

    #[test]
    fn ci_mutation_guard_unregisters_on_drop() {
        let lock: &'static Mutex<CiRefreshCoordinator> =
            Box::leak(Box::new(Mutex::new(CiRefreshCoordinator::default())));
        let idle: &'static Condvar = Box::leak(Box::new(Condvar::new()));
        let guard = begin_ci_mutation_in(lock, idle).expect("mutation admitted");
        assert_eq!(lock.lock().expect("local coordinator").mutations.len(), 1);

        drop(guard);

        assert!(lock.lock().expect("local coordinator").mutations.is_empty());
        cancel_coordinator_for_shutdown(&mut lock.lock().expect("local coordinator"));
        assert!(begin_ci_mutation_in(lock, idle).is_none());
    }

    #[test]
    fn ci_refresh_latch_clears_after_refused_admission() {
        let _lock = FLIGHT_TEST_LOCK.lock().expect("flight test lock");
        clear_ci_refresh_flight();
        spawn_ci_cache_refresh_with_generation(
            crate::session::Session::default(),
            thegn_core::config::Config::default(),
            None,
            false,
            None,
        );
        let coordinator = ci_refresh_coordinator().lock().expect("coordinator");
        assert!(coordinator.flight.running.is_none());
        assert!(coordinator.queued.is_none());
    }

    #[test]
    fn ci_refresh_latch_clears_when_worker_unwinds() {
        let _lock = FLIGHT_TEST_LOCK.lock().expect("flight test lock");
        let token = flight_token("/panic", None, false);
        let cancel = Arc::clone(&token.cancel);
        {
            let mut coordinator = ci_refresh_coordinator().lock().expect("coordinator");
            coordinator.flight.submit(token);
        }
        let panic = std::panic::catch_unwind(|| {
            let _guard = FlightGuard { cancel };
            panic!("fixture worker panic");
        });
        assert!(panic.is_err());
        assert!(
            ci_refresh_coordinator()
                .lock()
                .expect("coordinator")
                .flight
                .running
                .is_none()
        );
    }
}
