//! The compositor's calendar snapshot: resolved display settings, world clocks,
//! and whatever events have been fetched so far.
//!
//! Lives on [`crate::panel::docs::PanelDocs`] and is handed to the popup through
//! `StatusCtx`. The popup copies what it needs at open time and holds no borrow
//! across frames, per `detail.rs`'s founding invariant.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{NaiveDate, Weekday};
use chrono_tz::Tz;
use thegn_core::calendar::{CalEvent, ResolvedClock};
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
    pub events: BTreeMap<NaiveDate, Vec<CalEvent>>,
    /// Months whose events are known. A month outside this set still paints its
    /// grid instantly — with blank markers and a "loading" agenda.
    pub loaded: BTreeSet<(i32, u32)>,
}

impl Default for CalendarDocs {
    fn default() -> Self {
        CalendarDocs {
            ui: CalUiCfg::default(),
            clocks: Vec::new(),
            home: Tz::UTC,
            events: BTreeMap::new(),
            loaded: BTreeSet::new(),
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

    /// Fold a fetched month into the cache.
    pub fn merge(&mut self, month: (i32, u32), events: &[(NaiveDate, Vec<CalEvent>)]) {
        for (date, evs) in events {
            self.events.insert(*date, evs.clone());
        }
        self.loaded.insert(month);
    }
}
