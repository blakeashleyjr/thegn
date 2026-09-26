//! Calendar syncing, off the event loop.
//!
//! Threading contract, same as `hydrate_tracker`:
//! - runs on the background lane ([`crate::sched::spawn_bg`], 8 permits) so a
//!   slow calendar can't starve interactive model hydration;
//! - builds its own current-thread runtime *inside* the blocking task;
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
use std::sync::Arc;

use chrono::{Datelike, NaiveDate};
use termwiz::terminal::TerminalWaker;
use thegn_core::calendar::{CalEvent, ExpansionError};
use thegn_core::config_calendar::CalendarConfig;
use thegn_core::db::Db;
use thegn_core::store::{CalendarRow, CalendarStore};
use thegn_svc::calendar::{CalendarError, CalendarRouter, EventPage};
use tokio::sync::mpsc as tokio_mpsc;

use crate::calendar_docs::CalendarViewError;
use crate::hydrate::RefreshKind;

/// Days of slack either side of a month window.
///
/// The grid shows leading and trailing days from the neighbouring months, so an
/// event on Jan 31 has to be in February's answer.
const GRID_SLACK_DAYS: u64 = 7;

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
) {
    crate::sched::spawn_bg(move || {
        let (wide_from, wide_to) = widen(from, to);
        let home = home_zone(&cfg);
        let db = Db::open().ok(); // best-effort: cache: the popup falls back to the provider fetch when the DB is out

        // Sync first when anything is stale, so the popup shows current data
        // rather than yesterday's; `sync_accounts` no-ops when everything is
        // fresh within `ttl_secs`.
        //
        // Note the sync spans the whole configured HORIZON, not the month being
        // shown. A provider that honours the requested range (CalDAV's
        // `calendar-query` does) would otherwise return only that month, and a
        // full fetch replaces the account's cache — silently deleting every
        // other month. The narrow window is for READING the cache, never for
        // fetching into it.
        if let Some(db) = db.as_ref() {
            let today = chrono::Utc::now().with_timezone(&home).date_naive();
            let (h_from, h_to) = horizon(&cfg, today);
            sync_accounts(db, &cfg, h_from, h_to, false, &mut |m| {
                toast(&tx, &waker, m, None)
            });
        }

        // No cache is NOT an empty month: without the DB there is nothing to
        // show, and saying "no events" would be a lie.
        let month_view = match db.as_ref() {
            Some(db) => expand_month(db, wide_from, wide_to, home),
            None => MonthView::failed(CalendarViewError::CacheUnavailable),
        };
        deliver(&tx, &waker, year, month, month_view, None);
    });
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
    let cached = match load_cached(db, from, to) {
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

/// The periodic sync pass: refresh every account, then push the visible month
/// back into whatever popup is open.
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
    crate::sched::spawn_bg(move || {
        let Ok(db) = Db::open() else { return };
        let today = chrono::Utc::now()
            .with_timezone(&home_zone(&cfg))
            .date_naive();
        let (from, to) = horizon(&cfg, today);
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
        // Re-deliver the current month so an open popup picks the change up
        // without the user having to page away and back.
        let (m_from, m_to) =
            thegn_core::calendar::month_bounds(today.year(), today.month()).unwrap_or((from, to));
        let (wide_from, wide_to) = widen(m_from, m_to);
        let month_view = expand_month(&db, wide_from, wide_to, home_zone(&cfg));
        deliver(
            &tx,
            &waker,
            today.year(),
            today.month(),
            month_view,
            generation.as_ref(),
        );
    });
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
    let router = CalendarRouter::from_config(&scoped);
    if !router.is_configured() {
        return false;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };

    // Each account's page is applied and dropped inside the sink, releasing its
    // share of the global admission budget before the next account is fetched.
    let mut changed = false;
    rt.block_on(router.list_events_each(from, to, &tokens, |r| {
        let mut page = match r.result {
            Ok(p) => {
                thegn_core::connectivity::report_success();
                p
            }
            Err(e) => {
                if e.is_transient() {
                    thegn_core::connectivity::report_failure();
                }
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
        if apply_page_with_generation(db, &r.account, r.provider, &page, from, to, generation) {
            changed = true;
        }
    }));
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
    apply_page_with_generation(db, account, provider, page, from, to, None)
}

fn apply_page_with_generation(
    db: &Db,
    account: &str,
    provider: &str,
    page: &EventPage,
    from: NaiveDate,
    to: NaiveDate,
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
) -> bool {
    let (from_ms, to_ms) = (day_ms(from), day_ms(to));

    // A conditional fetch that came back 304: nothing to write, but the sync
    // stamp must still advance or we would re-hit the provider every tick.
    if page.is_unchanged() {
        if crate::hydrate_schedule::generation_is_current(generation) {
            let _ = db.put_calendar_sync(account, provider, page.sync_token(), from_ms, to_ms); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        }
        return false;
    }

    let incremental = !page.sync_token().is_empty();
    // Rule 2: an EMPTY FULL fetch is only believed when there was nothing
    // cached. Otherwise a 200-with-empty-body from a flaky proxy silently
    // erases a month of meetings — and unlike an error, nothing would warn.
    // (A page is always complete: an over-budget source never produces one.)
    if !incremental && page.events().is_empty() && db.has_calendar_events(account).unwrap_or(false)
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

    let rows: Vec<CalendarRow> = page.events().iter().map(row_of).collect();
    if !crate::hydrate_schedule::generation_is_current(generation) {
        return false;
    }
    let wrote = if incremental {
        let put = db.put_calendar_events(account, &rows);
        let del = if crate::hydrate_schedule::generation_is_current(generation) {
            db.delete_calendar_events(account, page.deleted())
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
fn row_of(e: &CalEvent) -> CalendarRow {
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
        calendar: e.calendar.clone(),
        start_ms: ms(&e.start),
        end_ms: ms(&e.end),
        recurring: e.recurrence.as_ref().is_some_and(|r| !r.is_empty()),
        json: serde_json::to_string(e).unwrap_or_else(|_| "{}".into()),
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
fn load_cached(db: &Db, from: NaiveDate, to: NaiveDate) -> Result<Cached, CalendarViewError> {
    let rows = db
        .get_calendar_events(day_ms(from), day_ms(to).saturating_add(86_400_000), &[])
        .map_err(|_| CalendarViewError::CacheUnavailable)?;
    let mut cached = Cached {
        events: Vec::with_capacity(rows.len()),
        skipped: 0,
    };
    for (_, json) in rows {
        match serde_json::from_str::<CalEvent>(&json) {
            Ok(event) => cached.events.push(event),
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
    crate::sched::spawn_bg_reserved(permit, move || {
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
        });
    }
    let home = home_zone(cfg);
    let at = chrono::DateTime::from_timestamp_millis(window.to_ms)
        .ok_or(CalendarViewError::Expansion(ExpansionError::InvalidWindow))?;
    let today = at.with_timezone(&home).date_naive();
    // A day either side is plenty: no sane reminder leads by more than that,
    // and it keeps the scan small.
    let (from, to) = (
        today.pred_opt().unwrap_or(today),
        today.succ_opt().unwrap_or(today),
    );
    let cached = load_cached(db, from, to)?;
    let expanded = thegn_core::calendar::expand_calendar(&cached.events, from, to, home)
        .map_err(CalendarViewError::Expansion)?;
    Ok(DueReminders {
        reminders: thegn_core::calendar::reminders::due_shared(
            &expanded.occurrences,
            home,
            &cfg.default_reminders(),
            window.from_ms,
            window.to_ms,
        ),
        // Rows the expander skipped as malformed are counted with the ones
        // that would not decode: both mean "these reminders are everything the
        // cache could yield", not "the evaluation failed".
        skipped: cached.skipped + expanded.skipped,
    })
}

fn widen(from: NaiveDate, to: NaiveDate) -> (NaiveDate, NaiveDate) {
    (
        from.checked_sub_days(chrono::Days::new(GRID_SLACK_DAYS))
            .unwrap_or(from),
        to.checked_add_days(chrono::Days::new(GRID_SLACK_DAYS))
            .unwrap_or(to),
    )
}

/// The configured sync horizon around `today`.
fn horizon(cfg: &CalendarConfig, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    (
        today
            .checked_sub_days(chrono::Days::new(cfg.horizon_past_days as u64))
            .unwrap_or(today),
        today
            .checked_add_days(chrono::Days::new(cfg.horizon_future_days as u64))
            .unwrap_or(today),
    )
}

fn home_zone(cfg: &CalendarConfig) -> chrono_tz::Tz {
    cfg.home_zone()
        .unwrap_or_else(thegn_core::calendar::tz::system_zone)
}

/// Midnight UTC on `d`, in unix ms.
fn day_ms(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc().timestamp_millis())
        .unwrap_or(0)
}

fn deliver(
    tx: &tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: &TerminalWaker,
    year: i32,
    month: u32,
    view: MonthView,
    generation: Option<&crate::hydrate_schedule::ScheduleFence>,
) {
    if !crate::hydrate_schedule::generation_is_current(generation) {
        return;
    }
    if let Some(error) = view.error {
        // A fixed label, never event content.
        tracing::warn!(
            target: "thegn::calendar",
            error = %error,
            year,
            month,
            "calendar month is unavailable or incomplete"
        );
    }
    let payload = crate::detail::CalendarPayload {
        month: (year, month),
        events: view.events,
        error: view.error,
    };
    let result = RefreshKind::CalendarMonth(Box::new(payload));
    let result = generation.map_or(result.clone(), |(_, generation)| RefreshKind::Scheduled {
        generation: *generation,
        kind: Box::new(result),
    });
    if tx.send(result).is_ok() {
        // best-effort: the loop may already be shutting down.
        let _ = waker.wake();
    }
}

#[cfg(test)]
#[path = "hydrate_calendar_tests.rs"]
mod tests;
