//! The compositor's calendar snapshot: resolved display settings, world clocks,
//! and whatever events have been fetched so far.
//!
//! Lives on [`crate::panel::docs::PanelDocs`] and is handed to the popup through
//! `StatusCtx`. The popup copies what it needs at open time and holds no borrow
//! across frames, per `detail.rs`'s founding invariant.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use chrono::{NaiveDate, Weekday};
use chrono_tz::Tz;
use thegn_core::calendar::{CalEvent, ExpansionError, ResolvedClock};
use thegn_core::config_calendar::CalendarConfig;

/// Display settings the popup reads, already resolved out of `auto`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalUiCfg {
    pub week_start: Weekday,
    pub twelve_hour: bool,
    pub show_week_numbers: bool,
    pub six_weeks: bool,
    pub show_agenda: bool,
    pub agenda_rows: usize,
    pub show_markers: bool,
    /// Whether any event source is configured at all. With none, the agenda
    /// block is suppressed entirely rather than showing a permanent "no events".
    pub has_sources: bool,
}

/// Why a calendar month (or reminder pass) is unavailable or incomplete.
/// Fixed labels only — never provider content — so it is safe to show and log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarError {
    /// The cache could not be opened or queried.
    CacheUnavailable,
    /// Some cached rows could not be decoded; the rest are shown.
    MalformedCache,
    /// Expansion refused (budget, invalid span/window, arithmetic).
    Expansion(ExpansionError),
}

impl CalendarError {
    /// Whether the view still carries the readable data (incomplete) rather
    /// than having none at all (unavailable).
    pub fn is_partial(&self) -> bool {
        matches!(self, Self::MalformedCache)
    }
}

impl std::fmt::Display for CalendarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CacheUnavailable => f.write_str("calendar cache unavailable"),
            Self::MalformedCache => f.write_str("calendar cache contains unreadable rows"),
            Self::Expansion(error) => error.fmt(f),
        }
    }
}

/// Fold one delivered month into a snapshot (the docs cache or an open popup).
///
/// Events land only when the month was actually produced, and only then is
/// it marked loaded — so a failed FIRST load stays "unavailable" instead of
/// reading as a successful empty month, and a failed REFRESH keeps the last
/// valid snapshot. The error is recorded alongside and cleared by the next
/// complete delivery.
pub(crate) fn fold_month(
    events: &mut BTreeMap<NaiveDate, Vec<Arc<CalEvent>>>,
    loaded: &mut BTreeSet<(i32, u32)>,
    errors: &mut BTreeMap<(i32, u32), CalendarError>,
    payload: &crate::detail::CalendarPayload,
) {
    if let Some(month) = &payload.events {
        for (date, evs) in month {
            events.insert(*date, evs.clone());
        }
        loaded.insert(payload.month);
    }
    match payload.error {
        Some(error) => {
            errors.insert(payload.month, error);
        }
        None => {
            errors.remove(&payload.month);
        }
    }
}

/// The `[weather]` knobs the popup's WEATHER block needs: the two staleness
/// thresholds, whether to draw the day strip, and how many days of it.
///
/// A copy rather than a `WeatherConfig` handle, for the same reason `CalUiCfg`
/// is: the popup snapshots at open time and holds no borrow across frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WxUiCfg {
    pub stale_after_secs: u64,
    pub hard_expiry_secs: u64,
    pub show_forecast: bool,
    pub forecast_days: usize,
}

impl WxUiCfg {
    /// The four fields the block reads, out of the live `[weather]` config.
    pub fn from_config(cfg: &thegn_core::config_weather::WeatherConfig) -> Self {
        WxUiCfg {
            stale_after_secs: cfg.stale_after_secs,
            hard_expiry_secs: cfg.hard_expiry_secs,
            show_forecast: cfg.show_forecast,
            forecast_days: cfg.forecast_days,
        }
    }
}

impl Default for CalUiCfg {
    fn default() -> Self {
        CalUiCfg {
            week_start: Weekday::Mon,
            twelve_hour: false,
            show_week_numbers: false,
            six_weeks: true,
            show_agenda: true,
            agenda_rows: 6,
            show_markers: true,
            has_sources: false,
        }
    }
}

/// Everything the calendar popup draws from.
#[derive(Debug, Clone)]
pub struct CalendarDocs {
    pub ui: CalUiCfg,
    /// The world-clock rows, home first.
    pub clocks: Vec<ResolvedClock>,
    /// The zone day boundaries and clock deltas are measured against.
    pub home: Tz,
    /// Cached events bucketed by the date they occupy, in `home`.
    pub events: BTreeMap<NaiveDate, Vec<Arc<CalEvent>>>,
    /// Months whose events are known. A month outside this set still paints its
    /// grid instantly — with blank markers and a "loading" agenda.
    pub loaded: BTreeSet<(i32, u32)>,
    /// Expansion failures are kept separately from loaded data so a failed
    /// refresh cannot turn a first load into a successful empty calendar or
    /// erase the last valid snapshot.
    pub errors: BTreeMap<(i32, u32), CalendarError>,
}

impl Default for CalendarDocs {
    fn default() -> Self {
        CalendarDocs {
            ui: CalUiCfg::default(),
            clocks: Vec::new(),
            home: Tz::UTC,
            events: BTreeMap::new(),
            loaded: BTreeSet::new(),
            errors: BTreeMap::new(),
        }
    }
}

impl CalendarDocs {
    /// Resolve `[calendar]` into what the popup needs.
    ///
    /// `locale` is the environment's `LC_TIME`/`LANG`, passed in so the
    /// resolution itself stays a pure core function.
    pub fn from_config(cfg: &CalendarConfig, locale: Option<&str>) -> Self {
        let home = cfg
            .home_zone()
            .unwrap_or_else(thegn_core::calendar::tz::system_zone);
        // The home row is synthesized rather than configured, so the block is
        // never empty even with no `[[calendar.clocks]]` at all.
        let mut clocks = vec![ResolvedClock {
            label: "local".into(),
            zone: home,
            format: String::new(),
            is_home: true,
        }];
        clocks.extend(cfg.active_clocks());
        CalendarDocs {
            ui: CalUiCfg {
                week_start: thegn_core::calendar::resolve_week_start(cfg.week_start_pref(), locale),
                twelve_hour: thegn_core::calendar::resolve_time_format(
                    cfg.twelve_hour_pref(),
                    locale,
                ),
                show_week_numbers: cfg.show_week_numbers,
                six_weeks: cfg.show_six_weeks,
                show_agenda: cfg.show_agenda,
                agenda_rows: cfg.agenda_rows.max(1),
                show_markers: cfg.show_event_markers,
                has_sources: !cfg.active_accounts().is_empty(),
            },
            clocks,
            home,
            events: BTreeMap::new(),
            loaded: BTreeSet::new(),
            errors: BTreeMap::new(),
        }
    }

    /// The locale string to resolve `auto` settings against.
    pub fn env_locale() -> Option<String> {
        for var in ["LC_ALL", "LC_TIME", "LANG"] {
            if let Ok(v) = std::env::var(var)
                && !v.trim().is_empty()
            {
                return Some(v);
            }
        }
        None
    }

    /// Fold a fetched month into the cache (see [`fold_month`]).
    pub fn merge(&mut self, payload: &crate::detail::CalendarPayload) {
        fold_month(
            &mut self.events,
            &mut self.loaded,
            &mut self.errors,
            payload,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detail::CalendarPayload;
    use chrono::NaiveDate;
    use thegn_core::calendar::{EventTime, ExpansionLimit};

    fn event() -> Arc<CalEvent> {
        Arc::new(CalEvent::new(
            "one",
            "One",
            EventTime::Date {
                date: NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            },
            EventTime::Date {
                date: NaiveDate::from_ymd_opt(2026, 8, 22).unwrap(),
            },
        ))
    }

    fn payload(
        events: Option<Vec<(NaiveDate, Vec<Arc<CalEvent>>)>>,
        error: Option<CalendarError>,
    ) -> CalendarPayload {
        CalendarPayload {
            month: (2026, 8),
            events,
            error,
        }
    }

    #[test]
    fn first_load_error_is_not_a_successful_empty_month() {
        let month = (2026, 8);
        let mut docs = CalendarDocs::default();
        let error = CalendarError::Expansion(ExpansionError::Budget(ExpansionLimit::BucketEntries));
        docs.merge(&payload(None, Some(error)));
        assert!(!docs.loaded.contains(&month));
        assert_eq!(docs.errors.get(&month), Some(&error));
        assert!(docs.events.is_empty());
    }

    #[test]
    fn failed_refresh_keeps_last_good_snapshot_and_success_clears_status() {
        let month = (2026, 8);
        let date = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
        let mut docs = CalendarDocs::default();
        docs.merge(&payload(Some(vec![(date, vec![event()])]), None));
        let prior = Arc::clone(&docs.events[&date][0]);

        let failed = CalendarError::Expansion(ExpansionError::InvalidSpan);
        docs.merge(&payload(None, Some(failed)));
        assert!(docs.loaded.contains(&month));
        assert!(
            Arc::ptr_eq(&docs.events[&date][0], &prior),
            "last good kept"
        );
        assert_eq!(docs.errors.get(&month), Some(&failed));

        docs.merge(&payload(Some(Vec::new()), None));
        assert!(docs.loaded.contains(&month));
        assert!(!docs.errors.contains_key(&month));
    }

    #[test]
    fn an_incomplete_month_is_loaded_and_flagged() {
        let month = (2026, 8);
        let date = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
        let mut docs = CalendarDocs::default();
        docs.merge(&payload(
            Some(vec![(date, vec![event()])]),
            Some(CalendarError::MalformedCache),
        ));
        assert!(docs.loaded.contains(&month));
        assert_eq!(docs.events[&date].len(), 1);
        assert!(docs.errors[&month].is_partial());
        assert!(!CalendarError::CacheUnavailable.is_partial());
        assert_eq!(
            CalendarError::MalformedCache.to_string(),
            "calendar cache contains unreadable rows"
        );
    }
}
