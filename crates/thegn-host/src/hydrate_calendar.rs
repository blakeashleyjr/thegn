//! Calendar syncing, off the event loop.
//!
//! Threading contract, same as `hydrate_tracker`:
//! - runs on the background lane ([`crate::sched::spawn_bg`], 8 permits) so a
//!   slow calendar can't starve interactive model hydration;
//! - uses the shared `thegn_svc::bg_runtime` current-thread runtime;
//! - pulses the [`TerminalWaker`] only when something actually changed.
//!
//! Three rules differ from the issue/PR refreshers and are each easy to get
//! wrong:
//!
//! 1. **Offline is decided per account, not per pass.** A `.ics` file or a
//!    `command` plugin is not network-backed and must keep syncing with the
//!    network down — unlike `Issues`/`Pr`, which are gated at drain time.
//! 2. **An empty result is not proof the calendar is empty.** A 200 with an
//!    empty body from a flaky proxy must never erase a month of meetings.
//! 3. **A failure never touches the cache.** The prior events and the resume
//!    cursor both survive, so a blip degrades to stale data, not no data.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{Datelike, NaiveDate};
use termwiz::terminal::TerminalWaker;
use thegn_core::calendar::validate::validate_event;
use thegn_core::calendar::{CalEvent, CalendarWindow, ExpansionError};
use thegn_core::config_calendar::{CalendarAccount, CalendarConfig};
use thegn_core::db::Db;
use thegn_core::store::{CalendarRow, CalendarStore};
use thegn_svc::calendar::{CalendarError, CalendarRouter, EventPage, PageMode};
use tokio::sync::mpsc as tokio_mpsc;

use crate::calendar_docs::CalendarViewError;
use crate::hydrate::RefreshKind;

/// Days of slack either side of a month window.
///
/// The grid shows leading and trailing days from the neighbouring months, so an
/// event on Jan 31 has to be in February's answer.
const GRID_SLACK_DAYS: u64 = 7;

#[derive(Default)]
struct CalendarRequests {
    next: u64,
    latest: BTreeMap<(i32, u32), u64>,
}

fn calendar_requests() -> &'static Mutex<CalendarRequests> {
    static REQUESTS: OnceLock<Mutex<CalendarRequests>> = OnceLock::new();
    REQUESTS.get_or_init(|| Mutex::new(CalendarRequests::default()))
}

/// Visible month encoded as `year * 12 + month`; zero means no month is known.
/// Writers use Release and readers use Acquire so periodic workers see the
/// latest UI-loop update without a lock or another wake source.
static VISIBLE_MONTH: AtomicU32 = AtomicU32::new(0);
/// Monotonic id of the latest navigation request; zero means none is registered.
/// Registration uses Release and workers use Acquire to observe the fence.
static LATEST_NAVIGATION: AtomicU64 = AtomicU64::new(0);

/// Record the month currently visible in the calendar popup.
///
/// Valid months are encoded as `year * 12 + month` (`0` is reserved for none).
/// The Release store pairs with [`visible_month`]'s Acquire load.
pub(crate) fn note_visible_month(month: (i32, u32)) {
    let encoded = if (1..=12).contains(&month.1) {
        i64::from(month.0)
            .checked_mul(12)
            .and_then(|year| year.checked_add(i64::from(month.1)))
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0)
    } else {
        0
    };
    VISIBLE_MONTH.store(encoded, Ordering::Release);
}

/// Read the last visible month, pairing with [`note_visible_month`]'s Release
/// store through an Acquire load.
fn visible_month() -> Option<(i32, u32)> {
    let encoded = VISIBLE_MONTH.load(Ordering::Acquire);
    if encoded == 0 {
        return None;
    }
    let zero_based = encoded - 1;
    Some(((zero_based / 12) as i32, zero_based % 12 + 1))
}

pub(crate) fn register_month_request(month: (i32, u32)) -> u64 {
    let mut requests = calendar_requests()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    requests.next = requests
        .next
        .checked_add(1)
        .expect("calendar request id overflow");
    let request = requests.next;
    requests.latest.insert(month, request);
    // The global navigation fence is only read by workers. Release here pairs
    // with navigation_superseded's Acquire load.
    LATEST_NAVIGATION.store(request, Ordering::Release);
    request
}

/// Return whether a newer navigation request has been registered. The Acquire
/// load pairs with the Release store in [`register_month_request`].
fn navigation_superseded(request: u64) -> bool {
    LATEST_NAVIGATION.load(Ordering::Acquire) > request
}

fn request_is_latest(latest: &BTreeMap<(i32, u32), u64>, month: (i32, u32), request: u64) -> bool {
    latest.get(&month) == Some(&request)
}

pub(crate) fn calendar_payload_is_current(payload: &crate::detail::CalendarPayload) -> bool {
    let Some(request) = payload.request else {
        return true;
    };
    let requests = calendar_requests()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    request_is_latest(&requests.latest, payload.month, request)
}

fn connectivity_evidence(
    network_backed: bool,
    result: &Result<EventPage, CalendarError>,
) -> Option<bool> {
    if !network_backed {
        return None;
    }
    match result {
        Ok(_) => Some(true),
        Err(error) if error.is_transient() => Some(false),
        Err(_) => None,
    }
}

#[derive(Default)]
struct ConnectivityEvidenceBatch {
    saw_success: bool,
    failures: u32,
}

impl ConnectivityEvidenceBatch {
    fn observe(&mut self, evidence: Option<bool>) {
        match evidence {
            Some(true) => self.saw_success = true,
            Some(false) => self.failures = self.failures.saturating_add(1),
            None => {}
        }
    }

    fn outcome(self) -> Option<ConnEvidence> {
        if self.saw_success {
            Some(ConnEvidence::Reachable)
        } else if self.failures > 0 {
            Some(ConnEvidence::Unreachable(self.failures))
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnEvidence {
    Reachable,
    Unreachable(u32),
}

fn network_account_names(accounts: &[CalendarAccount]) -> std::collections::BTreeSet<&str> {
    accounts
        .iter()
        .filter(|account| account.is_network_backed())
        .map(|account| account.name.as_str())
        .collect()
}

#[derive(Clone, Copy)]
struct MonthCoverage {
    year: i32,
    month: u32,
    covered: (NaiveDate, NaiveDate),
}

/// Fetch one month's events for the open popup.
///
/// Reads the cache first and answers immediately from it when the month is
/// already covered; otherwise it syncs. Nothing here is on a latency path — the
/// grid is already painted, only the markers and agenda are waiting — so it
/// takes the slow lane and posts no status message.
pub(crate) fn spawn_month_fetch(
    cfg: CalendarConfig,
    year: i32,
    month: u32,
    from: NaiveDate,
    to: NaiveDate,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) -> crate::sched::Admission {
    let request = register_month_request((year, month));
    let worker_tx = tx.clone();
    let worker_waker = waker.clone();
    let admission = crate::sched::spawn_bg(move || {
        let coverage = MonthCoverage {
            year,
            month,
            covered: widen(from, to),
        };
        run_month_fetch(
            cfg,
            coverage,
            request,
            worker_tx,
            Some(worker_waker),
            || Db::open().ok(),
        );
    });
    deliver_unavailable_if_refused(
        admission,
        &tx,
        Some(&waker),
        MonthCoverage {
            year,
            month,
            covered: widen(from, to),
        },
        Some(request),
    );
    admission
}

fn run_month_fetch(
    cfg: CalendarConfig,
    coverage: MonthCoverage,
    request: u64,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: Option<TerminalWaker>,
    open_db: impl FnOnce() -> Option<Db>,
) {
    let mut completion = MonthFetchCompletion {
        tx: tx.clone(),
        waker: waker.clone(),
        coverage,
        request,
        sent: false,
    };
    if navigation_superseded(request) {
        completion.sent = true;
        return;
    }
    let (wide_from, wide_to) = coverage.covered;
    let home = home_zone(&cfg);
    let db = open_db();

    // Sync first when anything is stale, so the popup shows current data
    // rather than yesterday's; `sync_accounts` no-ops when everything is
    // fresh within `ttl_secs`.
    //
    // The sync spans the whole configured HORIZON, not the month being shown.
    // The narrow window is for READING the cache, never for fetching into it.
    if let Some(db) = db.as_ref() {
        let today = chrono::Utc::now().with_timezone(&home).date_naive();
        match horizon(&cfg, today) {
            Ok((h_from, h_to)) => {
                sync_accounts(db, &cfg, h_from, h_to, false, &mut |m| {
                    if let Some(waker) = waker.as_ref() {
                        toast(&tx, waker, m, None);
                    }
                });
            }
            Err(problem) => {
                refuse_horizon(&problem, &mut |m| {
                    if let Some(waker) = waker.as_ref() {
                        toast(&tx, waker, m, None);
                    }
                });
            }
        }
    }

    if navigation_superseded(request) {
        completion.sent = true;
        return;
    }
    let month_view = match db.as_ref() {
        Some(db) => expand_month(db, wide_from, wide_to, home),
        None => MonthView::failed(CalendarViewError::CacheUnavailable),
    };
    deliver(
        &tx,
        waker.as_ref(),
        coverage,
        month_view,
        None,
        Some(request),
    );
    completion.sent = true;
}

fn deliver_unavailable_if_refused(
    admission: crate::sched::Admission,
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: Option<&TerminalWaker>,
    coverage: MonthCoverage,
    request: Option<u64>,
) {
    if admission != crate::sched::Admission::Admitted {
        deliver(
            tx,
            waker,
            coverage,
            MonthView::failed(CalendarViewError::CacheUnavailable),
            None,
            request,
        );
    }
}

/// Ensures the calendar popup receives a terminal result if the month worker
/// panics before its normal delivery.
struct MonthFetchCompletion {
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: Option<TerminalWaker>,
    coverage: MonthCoverage,
    request: u64,
    sent: bool,
}

impl Drop for MonthFetchCompletion {
    fn drop(&mut self) {
        if !self.sent {
            deliver(
                &self.tx,
                self.waker.as_ref(),
                self.coverage,
                MonthView::failed(CalendarViewError::CacheUnavailable),
                None,
                Some(self.request),
            );
        }
    }
}

/// One month's expansion outcome, before it becomes a payload.
pub(crate) struct MonthView {
    pub events: Option<Vec<(NaiveDate, Vec<Arc<CalEvent>>)>>,
    pub error: Option<CalendarViewError>,
}

impl MonthView {
    fn failed(error: CalendarViewError) -> MonthView {
        MonthView {
            events: None,
            error: Some(error),
        }
    }
}

/// Read the cache for a window and expand it into shared per-day buckets —
/// once, here, so the popup never re-expands while painting. Any cache or
/// expansion failure is reported, never collapsed into an empty month.
pub(crate) fn expand_month(
    db: &Db,
    from: NaiveDate,
    to: NaiveDate,
    home: chrono_tz::Tz,
) -> MonthView {
    let Some(window) = CalendarWindow::new(from, to, home) else {
        return MonthView::failed(CalendarViewError::InvalidWindow);
    };
    let cached = match load_cached_window(db, window) {
        Ok(cached) => cached,
        Err(error) => return MonthView::failed(error),
    };
    match thegn_core::calendar::expand_calendar(&cached.events, from, to, home) {
        Ok(calendar) => MonthView {
            events: Some(calendar.by_date.into_iter().collect()),
            // An undecodable or malformed row costs only itself, but the month
            // says so rather than passing for the whole calendar.
            error: (cached.skipped + calendar.skipped > 0)
                .then_some(CalendarViewError::MalformedCache),
        },
        Err(error) => MonthView::failed(CalendarViewError::Expansion(error)),
    }
}

/// The periodic sync pass: refresh every account, then push today's and the
/// popup's visible month back into the shared docs and any open popup.
#[allow(dead_code)] // retained as the untagged/event-driven entry point
pub(crate) fn spawn_periodic_sync(
    cfg: CalendarConfig,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    spawn_periodic_sync_with_generation(cfg, tx, waker, None);
}

pub(crate) fn spawn_periodic_sync_with_generation(
    cfg: CalendarConfig,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
    generation: Option<crate::hydrate_schedule::ScheduleFence>,
) {
    crate::sched::spawn_bg_replaceable(move || {
        let Ok(db) = Db::open() else { return };
        let today = chrono::Utc::now()
            .with_timezone(&home_zone(&cfg))
            .date_naive();
        let (from, to) = match horizon(&cfg, today) {
            Ok(h) => h,
            Err(problem) => {
                refuse_horizon(&problem, &mut |m| toast(&tx, &waker, m, generation.clone()));
                return;
            }
        };
        let changed = sync_accounts_with_generation(
            &db,
            &cfg,
            from,
            to,
            false,
            generation.as_ref(),
            &mut |m| toast(&tx, &waker, m, generation.clone()),
        );
        if !changed {
            return;
        }
        for (year, month) in periodic_months(today, visible_month()) {
            let (m_from, m_to) =
                thegn_core::calendar::month_bounds(year, month).unwrap_or((from, to));
            let (wide_from, wide_to) = widen(m_from, m_to);
            let month_view = expand_month(&db, wide_from, wide_to, home_zone(&cfg));
            deliver(
                &tx,
                Some(&waker),
                MonthCoverage {
                    year,
                    month,
                    covered: (wide_from, wide_to),
                },
                month_view,
                generation.as_ref(),
                None,
            );
        }
    });
}

pub(crate) fn periodic_months(today: NaiveDate, visible: Option<(i32, u32)>) -> Vec<(i32, u32)> {
    let current = (today.year(), today.month());
    match visible {
        Some(month) if month != current => vec![current, month],
        _ => vec![current],
    }
}

/// Sync every configured account into the cache. Returns whether anything
/// changed.
///
/// `force` bypasses the `ttl_secs` freshness guard (the popup's `r` key).
/// How long an account waits after the shared admission budget refused it.
///
/// Nothing is written to the DB for a contention refusal — a stamp there would
/// look like an attempt and hold the account back for `ttl_secs` — so the
/// backoff lives here instead. Without it the popup would re-sync (and
/// re-download) the account on every month change for as long as the
/// contention lasts.
const CONTENTION_BACKOFF_SECS: i64 = 60;

/// Accounts refused by the shared budget, and when. Bounded by the number of
/// configured accounts.
fn contention_seen() -> &'static std::sync::Mutex<BTreeMap<String, i64>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, i64>>> =
        std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

fn note_contention(account: &str, now: i64) {
    contention_seen()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(account.to_string(), now);
}

/// Whether `account` is still inside its post-contention backoff.
fn contention_backoff(account: &str, now: i64) -> bool {
    let mut seen = contention_seen().lock().unwrap_or_else(|e| e.into_inner());
    match seen.get(account) {
        Some(at) if now.saturating_sub(*at) < CONTENTION_BACKOFF_SECS => true,
        Some(_) => {
            seen.remove(account);
            false
        }
        None => false,
    }
}

/// Surface a calendar problem the user has to act on as an in-app toast.
fn toast(
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    message: String,
    generation: Option<crate::hydrate_schedule::ScheduleFence>,
) {
    let result = RefreshKind::Toast {
        message,
        priority: thegn_core::notification::Priority::Alert,
    };
    if !crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
        return;
    }
    let result = generation
        .as_ref()
        .map_or(result.clone(), |(_, generation)| RefreshKind::Scheduled {
            generation: *generation,
            kind: Box::new(result),
        });
    if tx.send(result).is_ok() {
        // best-effort: the loop may already be shutting down.
        let _ = waker.wake();
    }
}

/// `notify` receives a user-facing message when an account is refused by its
/// admission budget — once per new condition, not on every retry.
fn sync_accounts(
    db: &Db,
    cfg: &CalendarConfig,
    from: NaiveDate,
    to: NaiveDate,
    force: bool,
    notify: &mut dyn FnMut(String),
) -> bool {
    sync_accounts_with_generation(db, cfg, from, to, force, None, notify)
}

fn sync_accounts_with_generation(
    db: &Db,
    cfg: &CalendarConfig,
    from: NaiveDate,
    to: NaiveDate,
    force: bool,
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
    notify: &mut dyn FnMut(String),
) -> bool {
    let home = cfg.effective_home_zone();
    let Some(window) = CalendarWindow::new(from, to, home) else {
        tracing::warn!(target: "thegn::calendar", "invalid home-zone calendar window; skipping sync");
        return false;
    };
    let accounts = cfg.active_accounts();
    if accounts.is_empty() {
        return false;
    }
    let offline =
        thegn_core::connectivity::current() == thegn_core::connectivity::Connectivity::Offline;
    let now = thegn_core::util::now();

    // Which accounts actually need a fetch, and their resume cursors.
    let mut tokens: BTreeMap<String, String> = BTreeMap::new();
    let mut prior_errors: BTreeMap<String, String> = BTreeMap::new();
    let mut wanted: Vec<String> = Vec::new();
    for a in &accounts {
        // Rule 1: gate the NETWORK-backed accounts only. A local file or a
        // subprocess plugin must keep working offline.
        if offline && a.is_network_backed() {
            continue;
        }
        // Backed off after a shared-budget refusal: nothing was recorded for
        // it (deliberately — see `record_failure`), so this is what stops the
        // popup re-fetching it on every month change.
        if !force && contention_backoff(&a.name, now) {
            continue;
        }
        let sync = db.get_calendar_sync(&a.name).ok().flatten();
        let fresh = !force
            && sync.as_ref().is_some_and(|s| {
                thegn_core::time_policy::is_fresh(now, Some(s.fetched_at), cfg.ttl_secs)
            });
        if fresh {
            continue;
        }
        if let Some(s) = &sync {
            prior_errors.insert(a.name.clone(), s.last_error.clone());
        }
        if let Some(s) = &sync
            && !s.sync_token.is_empty()
        {
            tokens.insert(a.name.clone(), s.sync_token.clone());
        }
        wanted.push(a.name.clone());
    }
    if wanted.is_empty() {
        return false;
    }

    // Build a router over only the accounts we intend to fetch, so a fresh one
    // isn't re-hit just because a stale sibling needs work.
    let mut scoped = cfg.clone();
    scoped.accounts.retain(|a| wanted.contains(&a.name));
    let router = CalendarRouter::from_config_with_zone(&scoped, home);
    if !router.is_configured() {
        return false;
    }
    // Results arrive in completion order under per-class caps. Each page is
    // still applied and dropped inside the sink, releasing its admission share.
    let mut changed = false;
    let network_accounts = network_account_names(&accounts);
    let mut connectivity = ConnectivityEvidenceBatch::default();
    // The sink applies database pages while this blocking thread drives the shared
    // runtime (ms-scale).
    let Some(()) = thegn_svc::bg_runtime::block_on(router.list_events_each(window, &tokens, |r| {
        connectivity.observe(connectivity_evidence(
            network_accounts.contains(r.account.as_str()),
            &r.result,
        ));
        let mut page = match r.result {
            Ok(p) => p,
            Err(e) => {
                record_failure(
                    db,
                    &r.account,
                    r.provider,
                    &e,
                    &prior_errors,
                    notify,
                    generation,
                );
                return;
            }
        };
        // The cache rows built from the page are a second copy of it, so they
        // are accounted under the page's lease. Advisory on purpose: the data
        // is already admitted and in hand, and throwing a valid page away —
        // silently, since a pool refusal is nobody's fault — would be worse
        // than one unaccounted copy. The pool is sized so this cannot happen
        // to an account on its own (see `GLOBAL_MAX_BYTES`).
        let derived = page.reserved().1;
        if let Err(e) = page.reserve_derived(derived) {
            tracing::debug!(
                target: "thegn::calendar",
                account = %r.account,
                error = %e,
                "cache rows exceed the shared budget — writing them anyway"
            );
        }
        if apply_page_with_generation(db, &r.account, r.provider, &page, window, generation) {
            changed = true;
        }
    })) else {
        return false;
    };
    if crate::hydrate_schedule::generation_is_current(generation) {
        match connectivity.outcome() {
            Some(ConnEvidence::Reachable) => thegn_core::connectivity::report_success(),
            Some(ConnEvidence::Unreachable(failures)) => {
                for _ in 0..failures {
                    thegn_core::connectivity::report_failure();
                }
            }
            None => {}
        }
    }
    changed
}

/// Rule 3: a failed account keeps its events and cursor; only the failure is
/// recorded. Log the account NAME only — `token` and `url` are both secrets,
/// and a subscribed ICS URL *is* a credential.
fn record_failure(
    db: &Db,
    account: &str,
    provider: &str,
    e: &CalendarError,
    prior_errors: &BTreeMap<String, String>,
    notify: &mut dyn FnMut(String),
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
) {
    tracing::warn!(
        target: "thegn::calendar",
        account = %account,
        provider = provider,
        error = %e,
        "calendar sync failed — keeping the cached events"
    );
    match e {
        // Other syncs hold the shared budget right now. Nothing is wrong with
        // this account, so nothing is recorded: without an attempt stamp the
        // next tick retries it instead of waiting out `ttl_secs`.
        CalendarError::Admission(a) if a.is_contention() => {
            note_contention(account, thegn_core::util::now());
        }
        CalendarError::Admission(_) => {
            let message = e.to_string();
            // Tell the user once per new condition, not on every retry.
            if prior_errors.get(account) != Some(&message)
                && crate::hydrate_schedule::generation_is_current(generation)
            {
                let name = thegn_core::calendar::display::DisplayText::new(
                    account,
                    thegn_core::calendar::display::Field::Calendar,
                );
                notify(format!("Calendar \"{}\": {message}", name.as_str()));
            }
            if crate::hydrate_schedule::generation_is_current(generation) {
                let _ = db.set_calendar_error(account, &message); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            }
        }
        _ => {
            if crate::hydrate_schedule::generation_is_current(generation) {
                let _ = db.set_calendar_error(account, &e.to_string()); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
            }
        }
    }
}

/// Write one account's page into the cache. Returns whether anything changed.
// Test-only since the scheduler refactor: production now calls the
// generation-aware form directly, and this wrapper keeps the existing
// unfenced regressions meaningful.
#[cfg(test)]
fn apply_page(
    db: &Db,
    account: &str,
    provider: &str,
    page: &EventPage,
    from: NaiveDate,
    to: NaiveDate,
) -> bool {
    let Some(window) = CalendarWindow::new(from, to, chrono_tz::Tz::UTC) else {
        return false;
    };
    apply_page_with_generation(db, account, provider, page, window, None)
}

fn apply_page_with_generation(
    db: &Db,
    account: &str,
    provider: &str,
    page: &EventPage,
    window: CalendarWindow,
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
) -> bool {
    let (from_ms, to_ms) = (window.start_ms, window.end_exclusive_ms);

    // A conditional fetch that came back 304: nothing to write, but the sync
    // stamp must still advance or we would re-hit the provider every tick.
    // The cache strategy branches on what the fetch completed, never on the
    // cursor it returned (THE-452): a snapshot may return a token and a delta
    // may return none.
    let mode = page.mode();
    if mode == PageMode::Unchanged {
        if crate::hydrate_schedule::generation_is_current(generation) {
            let _ = db.put_calendar_sync(account, provider, page.sync_token(), from_ms, to_ms); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }
        return false;
    }

    let incremental = !mode.replaces_cache();
    // Rule 2: an EMPTY FULL fetch is only believed when there was nothing
    // cached. Otherwise a 200-with-empty-body from a flaky proxy silently
    // erases a month of meetings — and unlike an error, nothing would warn.
    // (A page is always complete: an over-budget source never produces one.)
    // A provider-requested reset is believed even when empty.
    if mode == PageMode::Snapshot
        && page.events().is_empty()
        && db.has_calendar_events(account).unwrap_or(false)
    {
        tracing::warn!(
            target: "thegn::calendar",
            account = %account,
            "empty full fetch — keeping the prior cache rather than erasing it"
        );
        if crate::hydrate_schedule::generation_is_current(generation) {
            let _ = db.set_calendar_error(account, "provider returned an empty calendar"); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }
        return false;
    }

    let rows: Vec<CalendarRow> = page
        .events()
        .iter()
        .enumerate()
        .map(|(index, event)| row_of(event, page.event_hrefs().get(index).map(String::as_str)))
        .collect();
    if !crate::hydrate_schedule::generation_is_current(generation) {
        return false;
    }
    let wrote = if incremental {
        let put = db.put_calendar_events(account, &rows);
        let del = if crate::hydrate_schedule::generation_is_current(generation) {
            if page.deleted_hrefs().is_empty() {
                db.delete_calendar_events(account, page.deleted())
            } else {
                let resources: Vec<_> = page
                    .deleted_hrefs()
                    .iter()
                    .zip(page.deleted())
                    .map(|(href, uid)| (href.clone(), uid.clone()))
                    .collect();
                db.delete_calendar_events_by_href(account, &resources)
            }
        } else {
            return false;
        };
        put.is_ok() && del.is_ok()
    } else {
        db.replace_calendar_account(account, &rows).is_ok()
    };
    if !wrote {
        return false;
    }
    if !crate::hydrate_schedule::generation_is_current(generation) {
        return false;
    }
    let _ = db.put_calendar_sync(account, provider, page.sync_token(), from_ms, to_ms); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    !rows.is_empty() || !page.deleted().is_empty()
}

/// Turn an event into its cache row.
fn row_of(e: &CalEvent, href: Option<&str>) -> CalendarRow {
    // A master's own span is irrelevant to the range query (it is loaded
    // unconditionally), but store it anyway so the row is self-describing.
    let home = chrono_tz::Tz::UTC;
    let ms = |t: &thegn_core::calendar::EventTime| {
        t.instant_in(home, thegn_core::calendar::GapPolicy::ShiftForward)
            .map(|d| d.timestamp_millis())
            .unwrap_or(0)
    };
    CalendarRow {
        uid: e.uid.clone(),
        recurrence_key: e.recurrence_key(),
        calendar: e.calendar.clone(),
        start_ms: ms(&e.start),
        end_ms: ms(&e.end),
        recurring: e.recurrence.as_ref().is_some_and(|r| !r.is_empty()),
        json: serde_json::to_string(e).unwrap_or_else(|_| "{}".into()),
        href: href.map(str::to_string),
    }
}

/// Cached events overlapping a window, and how many rows could not be read.
pub(crate) struct Cached {
    pub events: Vec<CalEvent>,
    pub skipped: usize,
}

/// Read cached events overlapping a window.
///
/// A query failure is an error, not an empty calendar. A row that fails to
/// deserialize (a newer schema, a corrupt write) is skipped rather than
/// losing the whole month, but counted, so the caller can say the view is
/// incomplete instead of presenting it as complete.
///
/// Date-taking convenience for the cache-mechanics tests (row counts, deserialize
/// failures). **UTC only** — production always goes through `load_cached_window`
/// with the home-zone window, and zone behaviour is covered by the explicit-zone
/// endpoint/DST tests rather than by this wrapper.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "date-taking cache wrapper used only by the hydrate_calendar tests"
    )
)]
fn load_cached(db: &Db, from: NaiveDate, to: NaiveDate) -> Result<Cached, CalendarViewError> {
    let window = CalendarWindow::new(from, to, chrono_tz::Tz::UTC)
        .ok_or(CalendarViewError::InvalidWindow)?;
    load_cached_window(db, window)
}

fn load_cached_window(db: &Db, window: CalendarWindow) -> Result<Cached, CalendarViewError> {
    let rows = db
        .get_calendar_events(window.start_ms, window.end_exclusive_ms, &[])
        .map_err(|_| CalendarViewError::CacheUnavailable)?;
    let mut cached = Cached {
        events: Vec::with_capacity(rows.len()),
        skipped: 0,
    };
    for (_, json) in rows {
        match serde_json::from_str::<CalEvent>(&json) {
            Ok(event) if validate_event(&event).is_ok() => cached.events.push(event),
            Ok(_) => cached.skipped += 1,
            Err(_) => cached.skipped += 1,
        }
    }
    Ok(cached)
}

/// One reminder evaluation window, `(from_ms, to_ms]`, tagged with the
/// generation that dispatched it so a stale acknowledgment can be told apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReminderWindow {
    pub generation: u64,
    pub from_ms: i64,
    pub to_ms: i64,
}

/// How a reminder evaluation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReminderOutcome {
    /// Every cached occurrence in the window was evaluated.
    Complete,
    /// Evaluated, but some cached rows could not be decoded. Retrying reads the
    /// same bytes, so the window is still done; the gap is reported.
    Incomplete(CalendarViewError),
    /// Nothing was raised; the window stays pending for the next ticker slot.
    Failed(CalendarViewError),
}

/// The loop-side reminder cursor: the last successfully evaluated instant and
/// the one evaluation in flight.
///
/// The cursor advances only when the worker acknowledges success for the
/// exact window it was handed — not at dispatch — so an expansion failure
/// leaves the window retryable, and an acknowledgment for a window that is no
/// longer in flight can neither clear a newer one nor move the cursor back.
#[derive(Debug)]
pub(crate) struct ReminderCursor {
    last_checked_ms: i64,
    next_generation: u64,
    inflight: Option<ReminderWindow>,
    /// The last failure reported, so a persistent one logs once, not per tick.
    reported: Option<CalendarViewError>,
}

impl ReminderCursor {
    pub(crate) fn new(now_ms: i64) -> ReminderCursor {
        ReminderCursor {
            last_checked_ms: now_ms,
            next_generation: 0,
            inflight: None,
            reported: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn last_checked_ms(&self) -> i64 {
        self.last_checked_ms
    }

    /// Open the window `(last_checked, now]`, or `None` while one is in flight
    /// (the next ticker slot retries; there is never more than one).
    pub(crate) fn begin(&mut self, now_ms: i64) -> Option<ReminderWindow> {
        if self.inflight.is_some() {
            return None;
        }
        let window = ReminderWindow {
            generation: self.next_generation,
            from_ms: self.last_checked_ms,
            to_ms: now_ms,
        };
        self.next_generation = self.next_generation.wrapping_add(1);
        self.inflight = Some(window);
        Some(window)
    }

    /// The window could not be dispatched (the background lane was full):
    /// forget it without moving the cursor.
    pub(crate) fn abandon(&mut self, window: ReminderWindow) {
        if self.inflight == Some(window) {
            self.inflight = None;
        }
    }

    /// Nothing to evaluate (reminders off, no sources): the window is
    /// trivially complete.
    pub(crate) fn skip(&mut self, now_ms: i64) {
        if self.inflight.is_none() {
            self.last_checked_ms = self.last_checked_ms.max(now_ms);
        }
    }

    /// Apply a worker's acknowledgment. Returns whether it was current.
    pub(crate) fn finish(&mut self, window: ReminderWindow, outcome: ReminderOutcome) -> bool {
        if self.inflight != Some(window) {
            return false;
        }
        self.inflight = None;
        let error = match outcome {
            ReminderOutcome::Complete => None,
            ReminderOutcome::Incomplete(error) | ReminderOutcome::Failed(error) => Some(error),
        };
        if !matches!(outcome, ReminderOutcome::Failed(_)) {
            self.last_checked_ms = self.last_checked_ms.max(window.to_ms);
        }
        if error != self.reported {
            if let Some(error) = error {
                // The error is a fixed label: no event content reaches the log.
                tracing::warn!(
                    target: "thegn::calendar",
                    error = %error,
                    retrying = matches!(outcome, ReminderOutcome::Failed(_)),
                    "calendar reminders could not be fully evaluated"
                );
            }
            self.reported = error;
        }
        true
    }
}

/// Raise whatever reminders have come due, off the loop.
///
/// The check reads the DB, so it must not run on the event loop — blocking I/O
/// there is the one thing the event model forbids outright. Inert when nothing
/// is configured, so the coarse ticker slot costs a user without a calendar
/// nothing at all.
///
/// The background permit is reserved HERE: a dispatch that silently skipped a
/// full lane would leave its window "in flight" forever and stop reminders.
#[allow(dead_code)] // retained as the untagged/event-driven entry point
pub(crate) fn spawn_reminder_check(
    cursor: &mut ReminderCursor,
    now_ms: i64,
    cfg: CalendarConfig,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    spawn_reminder_check_with_generation(cursor, now_ms, cfg, tx, waker, None);
}

pub(crate) fn spawn_reminder_check_with_generation(
    cursor: &mut ReminderCursor,
    now_ms: i64,
    cfg: CalendarConfig,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
    generation: Option<crate::hydrate_schedule::ScheduleFence>,
) {
    if !reminders_configured(&cfg) {
        cursor.skip(now_ms);
        return;
    }
    let Some(window) = cursor.begin(now_ms) else {
        return;
    };
    let Some(permit) = crate::sched::bg_permit() else {
        cursor.abandon(window); // lane full: the next ticker slot retries
        return;
    };
    let admission = crate::sched::spawn_bg_reserved(permit, move || {
        // Acknowledge even if evaluation panics, or the window would stay in
        // flight for the life of the process.
        let mut ack = ReminderAck {
            tx,
            waker: waker.clone(),
            window,
            outcome: ReminderOutcome::Failed(CalendarViewError::CacheUnavailable),
            generation: generation.clone(),
        };
        let outcome = match Db::open() {
            Err(_) => ReminderOutcome::Failed(CalendarViewError::CacheUnavailable),
            Ok(db) => match due_reminders(&db, &cfg, window) {
                Ok(due) => {
                    for r in &due.reminders {
                        // Fence at the notification boundary: in-flight DB
                        // work may finish, but replaced schedules never raise
                        // stale reminders.
                        if crate::hydrate_schedule::generation_is_current(generation.as_ref()) {
                            crate::handlers::calendar::raise_reminder(r, &waker);
                        }
                    }
                    if due.unsupported_leads > 0 {
                        tracing::warn!(
                            target: "thegn::calendar",
                            count = due.unsupported_leads,
                            "cached calendar reminders exceed the supported 28-day lead"
                        );
                    }
                    match due.skipped {
                        0 => ReminderOutcome::Complete,
                        _ => ReminderOutcome::Incomplete(CalendarViewError::MalformedCache),
                    }
                }
                Err(error) => ReminderOutcome::Failed(error),
            },
        };
        ack.outcome = outcome;
    });
    crate::sched::report_reserved(admission);
    if admission != crate::sched::Admission::Admitted {
        cursor.abandon(window);
    }
}

/// Sends the worker's acknowledgment on drop, so every exit path reports.
struct ReminderAck {
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
    window: ReminderWindow,
    outcome: ReminderOutcome,
    generation: Option<crate::hydrate_schedule::ScheduleFence>,
}

impl Drop for ReminderAck {
    fn drop(&mut self) {
        let result = RefreshKind::CalendarReminderResult {
            window: self.window,
            outcome: self.outcome,
        };
        let result = self
            .generation
            .as_ref()
            .map_or(result.clone(), |(_, generation)| RefreshKind::Scheduled {
                generation: *generation,
                kind: Box::new(result),
            });
        let sent = self.tx.send(result);
        if sent.is_ok() {
            let _ = self.waker.wake(); // best-effort: the loop may already be shutting down
        }
    }
}

fn reminders_configured(cfg: &CalendarConfig) -> bool {
    cfg.reminders_enabled && !cfg.active_accounts().is_empty()
}

/// Due reminders from one evaluation, plus how many cached rows were unreadable.
pub(crate) struct DueReminders {
    pub reminders: Vec<thegn_core::calendar::DueReminder>,
    pub skipped: usize,
    pub unsupported_leads: usize,
}

/// Which reminders came due in exactly `window`.
///
/// No network and no re-expansion of the whole horizon — it reads the cache the
/// sync already filled, expands it once under the shared budget, and evaluates
/// the UNIQUE occurrence list, so a multi-day event is judged once rather than
/// once per day it occupies. An expansion failure raises nothing: a partial
/// list must never pass for a complete one.
pub(crate) fn due_reminders(
    db: &Db,
    cfg: &CalendarConfig,
    window: ReminderWindow,
) -> Result<DueReminders, CalendarViewError> {
    if !reminders_configured(cfg) {
        return Ok(DueReminders {
            reminders: Vec::new(),
            skipped: 0,
            unsupported_leads: 0,
        });
    }
    let home = cfg.effective_home_zone();
    let invalid = || CalendarViewError::Expansion(ExpansionError::InvalidWindow);
    let to_at = chrono::DateTime::from_timestamp_millis(window.to_ms).ok_or_else(invalid)?;
    // The core clamps catch-up to an hour, so the scan does too: a stale
    // cursor must not widen the query.
    let from_ms = window
        .from_ms
        .clamp(window.to_ms.saturating_sub(3_600_000), window.to_ms);
    let from_at = chrono::DateTime::from_timestamp_millis(from_ms).ok_or_else(invalid)?;
    let day = |at: chrono::DateTime<chrono::Utc>| at.with_timezone(&home).date_naive();
    let lead = |mins: u32| chrono::TimeDelta::minutes(i64::from(mins));
    // A reminder fires when its TRIGGER is in `(from, to]`, and the trigger is
    // `lead` before the start. Read the maximum supported lead in one bounded,
    // indexed query: a longer event lead cannot widen a query that never saw
    // the event (THE-460).
    let defaults = cfg.default_reminders();
    let range = reminder_cache_window(
        from_at,
        to_at,
        home,
        thegn_core::calendar::reminders::MAX_LEAD_MINS,
    )
    .ok_or_else(invalid)?;
    let cached = load_cached_window(db, range)?;
    let unsupported_leads = cached
        .events
        .iter()
        .flat_map(|event| event.reminders.iter())
        .filter(|reminder| reminder.minutes_before > thegn_core::calendar::reminders::MAX_LEAD_MINS)
        .count();
    let needed = thegn_core::calendar::reminders::max_lead_mins(&cached.events, &defaults);
    let reach = to_at.checked_add_signed(lead(needed)).ok_or_else(invalid)?;
    let from = range.from;
    let to = day(reach).succ_opt().unwrap_or(day(reach));
    let expanded = thegn_core::calendar::expand_calendar(&cached.events, from, to, home)
        .map_err(CalendarViewError::Expansion)?;
    Ok(DueReminders {
        reminders: thegn_core::calendar::reminders::due_shared(
            &expanded.occurrences,
            home,
            &defaults,
            window.from_ms,
            window.to_ms,
        ),
        // Rows the expander skipped as malformed are counted with the ones
        // that would not decode: both mean "these reminders are everything the
        // cache could yield", not "the evaluation failed".
        skipped: cached.skipped + expanded.skipped,
        unsupported_leads,
    })
}

fn reminder_cache_window(
    from_at: chrono::DateTime<chrono::Utc>,
    to_at: chrono::DateTime<chrono::Utc>,
    home: chrono_tz::Tz,
    lead_mins: u32,
) -> Option<CalendarWindow> {
    let day = |at: chrono::DateTime<chrono::Utc>| at.with_timezone(&home).date_naive();
    let first = day(from_at).pred_opt().unwrap_or(day(from_at));
    let reach = to_at.checked_add_signed(chrono::TimeDelta::minutes(i64::from(lead_mins)))?;
    let last = day(reach).succ_opt().unwrap_or(day(reach));
    CalendarWindow::new(first, last, home)
}

pub(crate) fn widen(from: NaiveDate, to: NaiveDate) -> (NaiveDate, NaiveDate) {
    (
        from.checked_sub_days(chrono::Days::new(GRID_SLACK_DAYS))
            .unwrap_or(from),
        to.checked_add_days(chrono::Days::new(GRID_SLACK_DAYS))
            .unwrap_or(to),
    )
}

/// The configured sync horizon around `today`, or why it is unusable. Never a
/// substitute interval: the result is the coverage a full fetch is authoritative
/// for, so a bad config must stop the sync instead.
fn horizon(cfg: &CalendarConfig, today: NaiveDate) -> Result<(NaiveDate, NaiveDate), String> {
    cfg.sync_horizon(today)
}

/// Surface an unusable horizon: logged and toasted, with the cache untouched.
fn refuse_horizon(problem: &str, notify: &mut dyn FnMut(String)) {
    tracing::warn!(
        target: "thegn::calendar",
        error = %problem,
        "invalid calendar horizon; sync refused, keeping the cached events"
    );
    notify(format!("Calendar sync skipped: {problem}"));
}

fn home_zone(cfg: &CalendarConfig) -> chrono_tz::Tz {
    cfg.effective_home_zone()
}

fn deliver(
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: Option<&TerminalWaker>,
    coverage: MonthCoverage,
    view: MonthView,
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
    request: Option<u64>,
) {
    if !crate::hydrate_schedule::generation_is_current(generation) {
        return;
    }
    if let Some(error) = view.error {
        // A fixed label, never event content.
        tracing::warn!(
            target: "thegn::calendar",
            error = %error,
            year = coverage.year,
            month = coverage.month,
            "calendar month is unavailable or incomplete"
        );
    }
    let payload = crate::detail::CalendarPayload {
        month: (coverage.year, coverage.month),
        covered: coverage.covered,
        request,
        events: view.events,
        error: view.error,
    };
    let result = RefreshKind::CalendarMonth(Box::new(payload));
    let result = generation.map_or(result.clone(), |(_, generation)| RefreshKind::Scheduled {
        generation: *generation,
        kind: Box::new(result),
    });
    if tx.send(result).is_ok()
        && let Some(waker) = waker
    {
        // best-effort: the loop may already be shutting down.
        let _ = waker.wake();
    }
}

#[cfg(test)]
#[path = "hydrate_calendar_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "hydrate_calendar_admission_tests.rs"]
mod admission_tests;
