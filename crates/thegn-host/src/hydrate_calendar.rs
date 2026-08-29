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

use chrono::{Datelike, NaiveDate};
use termwiz::terminal::TerminalWaker;
use thegn_core::calendar::CalEvent;
use thegn_core::config_calendar::CalendarConfig;
use thegn_core::db::Db;
use thegn_core::store::{CalendarRow, CalendarStore};
use thegn_svc::calendar::{CalendarRouter, EventPage};
use tokio::sync::mpsc as tokio_mpsc;

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
            sync_accounts(db, &cfg, h_from, h_to, false);
        }

        let events = match db.as_ref() {
            Some(db) => load_cached(db, wide_from, wide_to),
            None => Vec::new(),
        };
        // Expand recurrence into concrete per-day buckets once, here, so the
        // popup never re-expands while painting.
        let by_date = thegn_core::calendar::expand_by_date(&events, wide_from, wide_to, home);
        deliver(&tx, &waker, year, month, by_date.into_iter().collect());
    });
}

/// The periodic sync pass: refresh every account, then push the visible month
/// back into whatever popup is open.
pub(crate) fn spawn_periodic_sync(
    cfg: CalendarConfig,
    tx: tokio_mpsc::UnboundedSender<RefreshKind>,
    waker: TerminalWaker,
) {
    crate::sched::spawn_bg(move || {
        let Ok(db) = Db::open() else { return };
        let today = chrono::Utc::now()
            .with_timezone(&home_zone(&cfg))
            .date_naive();
        let (from, to) = horizon(&cfg, today);
        let changed = sync_accounts(&db, &cfg, from, to, false);
        if !changed {
            return;
        }
        // Re-deliver the current month so an open popup picks the change up
        // without the user having to page away and back.
        let (m_from, m_to) =
            thegn_core::calendar::month_bounds(today.year(), today.month()).unwrap_or((from, to));
        let (wide_from, wide_to) = widen(m_from, m_to);
        let events = load_cached(&db, wide_from, wide_to);
        let by_date =
            thegn_core::calendar::expand_by_date(&events, wide_from, wide_to, home_zone(&cfg));
        deliver(
            &tx,
            &waker,
            today.year(),
            today.month(),
            by_date.into_iter().collect(),
        );
    });
}

/// Sync every configured account into the cache. Returns whether anything
/// changed.
///
/// `force` bypasses the `ttl_secs` freshness guard (the popup's `r` key).
fn sync_accounts(
    db: &Db,
    cfg: &CalendarConfig,
    from: NaiveDate,
    to: NaiveDate,
    force: bool,
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
    let mut wanted: Vec<String> = Vec::new();
    for a in &accounts {
        // Rule 1: gate the NETWORK-backed accounts only. A local file or a
        // subprocess plugin must keep working offline.
        if offline && a.is_network_backed() {
            continue;
        }
        let sync = db.get_calendar_sync(&a.name).ok().flatten();
        let fresh = !force
            && sync
                .as_ref()
                .is_some_and(|s| s.fetched_at > 0 && now - s.fetched_at < cfg.ttl_secs as i64);
        if fresh {
            continue;
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

    let results = rt.block_on(router.list_events(from, to, &tokens));
    let mut changed = false;
    for r in results {
        let page = match r.result {
            Ok(p) => {
                thegn_core::connectivity::report_success();
                p
            }
            Err(e) => {
                if e.is_transient() {
                    thegn_core::connectivity::report_failure();
                }
                // Rule 3: record the failure, touch nothing else. Log the
                // account NAME only — `token` and `url` are both secrets, and a
                // subscribed ICS URL *is* a credential.
                tracing::warn!(
                    target: "thegn::calendar",
                    account = %r.account,
                    provider = r.provider,
                    error = %e,
                    "calendar sync failed — keeping the cached events"
                );
                let _ = db.set_calendar_error(&r.account, &e.to_string()); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
                continue;
            }
        };
        if apply_page(db, &r.account, r.provider, &page, from, to) {
            changed = true;
        }
    }
    changed
}

/// Write one account's page into the cache. Returns whether anything changed.
fn apply_page(
    db: &Db,
    account: &str,
    provider: &str,
    page: &EventPage,
    from: NaiveDate,
    to: NaiveDate,
) -> bool {
    let (from_ms, to_ms) = (day_ms(from), day_ms(to));

    // A conditional fetch that came back 304: nothing to write, but the sync
    // stamp must still advance or we would re-hit the provider every tick.
    if page.unchanged {
        let _ = db.put_calendar_sync(account, provider, &page.sync_token, from_ms, to_ms); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        return false;
    }

    let incremental = !page.sync_token.is_empty();
    // Rule 2: an EMPTY FULL fetch is only believed when there was nothing
    // cached. Otherwise a 200-with-empty-body from a flaky proxy silently
    // erases a month of meetings — and unlike an error, nothing would warn.
    if !incremental
        && page.events.is_empty()
        && !page.partial
        && db.has_calendar_events(account).unwrap_or(false)
    {
        tracing::warn!(
            target: "thegn::calendar",
            account = %account,
            "empty full fetch — keeping the prior cache rather than erasing it"
        );
        let _ = db.set_calendar_error(account, "provider returned an empty calendar"); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
        return false;
    }

    let rows: Vec<CalendarRow> = page.events.iter().map(row_of).collect();
    let wrote = if incremental {
        let put = db.put_calendar_events(account, &rows);
        let del = db.delete_calendar_events(account, &page.deleted);
        put.is_ok() && del.is_ok()
    } else {
        db.replace_calendar_account(account, &rows).is_ok()
    };
    if !wrote {
        return false;
    }
    let _ = db.put_calendar_sync(account, provider, &page.sync_token, from_ms, to_ms); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    !rows.is_empty() || !page.deleted.is_empty()
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

/// Read cached events overlapping a window.
fn load_cached(db: &Db, from: NaiveDate, to: NaiveDate) -> Vec<CalEvent> {
    db.get_calendar_events(day_ms(from), day_ms(to) + 86_400_000, &[])
        .unwrap_or_default()
        .into_iter()
        // A row that fails to deserialize is from a newer schema or a corrupt
        // write; skip it rather than losing the whole month.
        .filter_map(|(_, json)| serde_json::from_str::<CalEvent>(&json).ok())
        .collect()
}

/// Raise whatever reminders have come due, off the loop.
///
/// The check reads the DB, so it must not run on the event loop — blocking I/O
/// there is the one thing the event model forbids outright. Inert when nothing
/// is configured, so the coarse ticker slot costs a user without a calendar
/// nothing at all.
pub(crate) fn spawn_reminder_check(
    cfg: CalendarConfig,
    last_checked_ms: i64,
    waker: TerminalWaker,
) {
    if !cfg.reminders_enabled || cfg.active_accounts().is_empty() {
        return;
    }
    crate::sched::spawn_bg(move || {
        for r in due_reminders(&cfg, last_checked_ms) {
            crate::handlers::calendar::raise_reminder(&r, &waker);
        }
    });
}

/// Which reminders have come due.
///
/// No network and no re-expansion of the whole horizon — it reads the cache the
/// sync already filled, and the decision itself is a pure comparison
/// ([`thegn_core::calendar::reminders::due`]). Split out from the spawn so it
/// is testable without a runtime.
pub(crate) fn due_reminders(
    cfg: &CalendarConfig,
    last_checked_ms: i64,
) -> Vec<thegn_core::calendar::DueReminder> {
    if !cfg.reminders_enabled || cfg.active_accounts().is_empty() {
        return Vec::new();
    }
    let Ok(db) = Db::open() else {
        return Vec::new();
    };
    let home = home_zone(cfg);
    let now = chrono::Utc::now();
    let today = now.with_timezone(&home).date_naive();
    // A day either side is plenty: no sane reminder leads by more than that,
    // and it keeps the scan small.
    let (from, to) = (
        today.pred_opt().unwrap_or(today),
        today.succ_opt().unwrap_or(today),
    );
    let events = load_cached(&db, from, to);
    let expanded: Vec<CalEvent> = thegn_core::calendar::expand_by_date(&events, from, to, home)
        .into_values()
        .flatten()
        .collect();
    thegn_core::calendar::reminders::due(
        &expanded,
        home,
        &cfg.default_reminders(),
        last_checked_ms,
        now.timestamp_millis(),
    )
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
    events: Vec<(NaiveDate, Vec<CalEvent>)>,
) {
    let payload = crate::detail::CalendarPayload {
        month: (year, month),
        events,
    };
    if tx
        .send(RefreshKind::CalendarMonth(Box::new(payload)))
        .is_ok()
    {
        // best-effort: the loop may already be shutting down.
        let _ = waker.wake();
    }
}

#[cfg(test)]
#[path = "hydrate_calendar_tests.rs"]
mod tests;
