//! CI run-history refresh machinery (AV group): the ttl-guarded refresh of the
//! active worktree, a round-robin background-worktree sweep that keeps the
//! cross-worktree Across excerpts honest, exponential backoff after fetch
//! failures, and the per-worktree fetch-health note the panel surfaces
//! ("CI provider API rate limited", …). The ticker cadence and the
//! `RefreshKind` plumbing stay in [`crate::hydrate`]; everything here runs off
//! the event loop.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use termwiz::terminal::TerminalWaker;

const MAX_LOG_INGEST_CALLS: usize = 32;
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
    crate::sched::spawn_bg_replaceable(move || {
        let cfg = full.ci.clone();
        let cwd = crate::hydrate::active_tab_path(&session);
        if cwd.is_dir() {
            let loc = thegn_core::remote::GitLoc::for_worktree(&cwd);
            refresh_ci_cache_for(
                &cwd,
                &loc,
                &cfg,
                &full,
                waker.as_ref(),
                force,
                generation.clone(),
            );
        }
        // The sweep rides the same bg task so provider subprocesses stay
        // serialized (kind to rate limits) and the loop-side call stays one
        // spawn. Sweeping is never forced — the ttl guard is its rate limiter.
        sweep_one_background(&session, &cwd, &cfg, &full, waker.as_ref(), generation);
    });
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
    let start = SWEEP_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for i in 0..others.len() {
        let p = std::path::Path::new(others[(start + i) % others.len()]);
        if !p.is_dir() {
            continue;
        }
        let loc = thegn_core::remote::GitLoc::for_worktree(p);
        // First worktree that actually fetched (fresh ones are skipped by the
        // guard) ends the sweep — one provider subprocess per tick, max.
        if refresh_ci_cache_for(p, &loc, cfg, full, waker, false, generation.clone()) {
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
    generation: Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>,
) -> bool {
    use thegn_core::store::CacheStore;
    let Ok(db) = thegn_core::db::Db::open() else {
        return false;
    };
    // Keyed by the HOST worktree path — `loc.path()` collides across sandboxed
    // worktrees, and the Across builder reads by the registry's worktree path.
    let key = thegn_core::remote::GitLoc::worktree_cache_key(host_path);
    let now = thegn_core::util::now();
    if !force {
        let fetched_at = db.get_ci_cache(&key).ok().flatten().map(|(_, at)| at);
        if ci_cache_is_fresh(fetched_at, now, cfg.ttl_secs) || backoff_active(&key, now) {
            return false;
        }
    }
    let Some(client) = thegn_svc::ci::provider_for(loc, cfg) else {
        return false;
    };
    let branch = loc
        .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|b| !b.is_empty());
    match client.runs(
        loc,
        branch.as_deref(),
        cfg.max_runs.min(thegn_core::config_ci::MAX_CI_RUNS),
    ) {
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
                });
            }
        }
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
        if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
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
        let detail = match client.run_detail(loc, &run.id) {
            Ok(detail) => detail,
            Err(_) => {
                if let Ok(mut memo) = ingest_memo().lock() {
                    memo.touch(run_key, now, cfg.poll_interval_secs, true);
                }
                continue;
            }
        };
        if detail.discarded_jobs > 0 {
            tracing::warn!(target: "thegn::ci", discarded_rows = detail.discarded_jobs, "CI detail omitted malformed job rows");
            // A partial matrix is not enough evidence for log ingestion or an autofix handoff.
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
            if !call_budget.take() {
                tracing::debug!(target: "thegn::ci", budget = MAX_LOG_INGEST_CALLS, "CI log ingestion provider-call budget exhausted");
                break 'runs;
            }
            let log = match client.logs(loc, &run.id, &job.id) {
                Ok(log) => log,
                Err(_) => {
                    run_complete = false;
                    if let Ok(mut memo) = ingest_memo().lock() {
                        memo.touch(job_key, now, cfg.poll_interval_secs, true);
                    }
                    continue;
                }
            };
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
        }
        if run_complete && let Ok(mut memo) = ingest_memo().lock() {
            memo.mark_complete(run_key);
        }
    }
    if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
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

    struct FakeCi {
        log_failures: HashSet<String>,
        detail_failures: HashSet<String>,
        details: HashMap<String, thegn_svc::ci::CiRunDetail>,
        detail_calls: AtomicUsize,
        log_calls: Mutex<Vec<(String, String)>>,
    }

    impl FakeCi {
        fn new(log_failures: &[&str]) -> Self {
            Self {
                log_failures: log_failures.iter().map(|id| (*id).to_string()).collect(),
                detail_failures: HashSet::new(),
                details: HashMap::new(),
                detail_calls: AtomicUsize::new(0),
                log_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_detail(mut self, run_id: &str, sha: &str) -> Self {
            self.details
                .insert(run_id.to_string(), ci_detail(run_id, sha));
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
                Ok(thegn_core::ci::CiLog::default())
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
            deadline: None,
        };
        assert!(budget.take());
        assert!(budget.take());
        assert!(!budget.take());
        assert!(!budget.take());
    }

    #[test]
    fn log_ingestion_budget_rejects_expired_deadline() {
        let mut budget = CallBudget {
            remaining: 2,
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
}
