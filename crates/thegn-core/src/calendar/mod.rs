//! Calendar domain: month-grid geometry, a navigation cursor, IANA world
//! clocks, and the event model the providers and the popup share.
//!
//! **Everything here is pure.** No I/O, no `Local::now()`, no globals: every
//! entry point takes `now` and the home zone explicitly. That is what lets the
//! whole module be exhaustively unit-tested under the 95% core coverage gate,
//! and what lets the popup render any month instantly without a round trip.
//!
//! Layout:
//! - [`grid`] — the 7×6 month matrix (leading/trailing days, ISO week numbers).
//! - [`cursor`] — the selection state machine (`h`/`j`/`k`/`l`, month/year paging).
//! - [`tz`] — zone resolution and world-clock readings.
//! - [`locale`] — resolving `week_start = "auto"` / `time_format = "auto"`.
//!
//! Naming note: the event type is [`CalEvent`], never `Event` —
//! [`crate::event_bus::Event`] already owns that name.

pub mod cursor;
pub mod grid;
pub mod ics;
pub mod locale;
pub mod recur;
pub mod reminders;
pub mod tz;

pub use cursor::{CalCursor, CalNav};
pub use grid::{DayCell, MonthGrid, WeekdayStyle, month_bounds, weekday_headers};
pub use ics::parse_ics;
pub use locale::{resolve_time_format, resolve_week_start};
pub use recur::{ByDay, Freq, RRule, RecurError, Recurrence};
pub use reminders::{DueReminder, next_event};
pub use tz::{ClockReading, GapPolicy, ResolvedClock, TzRef, read_clocks, resolve_zone};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A half-open instant range, `[from, to)`.
///
/// Half-open on purpose: an event ending exactly at `from` does not overlap,
/// and adjacent month windows tile without double-counting a midnight event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl DateRange {
    pub fn new(from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        DateRange { from, to }
    }

    /// Whether `[start, end)` overlaps this range.
    pub fn overlaps(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
        start < self.to && end > self.from
    }
}

/// When an event happens.
///
/// Three-valued deliberately — collapsing these into one timestamp is *the*
/// classic calendar bug:
///
/// - [`EventTime::Date`] is a floating calendar date with no time and no zone
///   (an all-day event; "Christmas" is Dec 25 everywhere, not an instant).
/// - [`EventTime::Zoned`] is a wall-clock time in a named zone. This is what
///   recurring events store, so that a weekly 09:00 stays 09:00 across a DST
///   boundary rather than drifting to 08:00 or 10:00.
/// - [`EventTime::Instant`] is a fixed point on the timeline, for providers
///   that hand back an absolute timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventTime {
    Date { date: NaiveDate },
    Zoned { local: NaiveDateTime, zone: TzRef },
    Instant { at: DateTime<Utc> },
}

impl EventTime {
    /// Resolve to an absolute instant, interpreting a floating date as midnight
    /// in `home`.
    ///
    /// Returns `None` only for a local time that genuinely does not exist (the
    /// spring-forward gap) under [`GapPolicy::Skip`]; every other policy always
    /// yields an instant.
    pub fn instant_in(&self, home: chrono_tz::Tz, gap: GapPolicy) -> Option<DateTime<Utc>> {
        match self {
            EventTime::Instant { at } => Some(*at),
            EventTime::Date { date } => {
                let midnight = date.and_hms_opt(0, 0, 0)?;
                tz::resolve_local(midnight, home, gap)
            }
            EventTime::Zoned { local, zone } => {
                let z = zone.resolve().unwrap_or(home);
                tz::resolve_local(*local, z, gap)
            }
        }
    }

    /// The calendar date this falls on, as seen from `home`. Used to bucket
    /// events into month-grid cells.
    pub fn date_in(&self, home: chrono_tz::Tz) -> Option<NaiveDate> {
        match self {
            // A floating date is already a date — never round-trip it through
            // an instant, or a zone east of UTC can shift it by a day.
            EventTime::Date { date } => Some(*date),
            other => {
                use chrono::TimeZone;
                let at = other.instant_in(home, GapPolicy::ShiftForward)?;
                Some(home.from_utc_datetime(&at.naive_utc()).date_naive())
            }
        }
    }

    /// Whether this is a floating all-day date.
    pub fn is_all_day(&self) -> bool {
        matches!(self, EventTime::Date { .. })
    }
}

/// A globally unique event id, `"<source>/<uid>"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(pub String);

impl EventId {
    pub fn new(source: &str, uid: &str) -> Self {
        EventId(format!("{source}/{uid}"))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Which configured account an event came from (`"<provider>:<account>"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub String);

impl SourceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// RFC 5545 participation status, kept as data even though this pass never
/// writes it back — dropping it would make the cache lossy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    #[default]
    Confirmed,
    Tentative,
    Cancelled,
}

/// Free/busy transparency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Busy {
    #[default]
    Busy,
    Free,
}

/// A reminder offset, in minutes *before* the occurrence start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Reminder {
    pub minutes_before: u32,
}

/// One calendar event, as cached and as sent over the plugin wire.
///
/// **This struct is the plugin API.** Every field but the four essentials is
/// `#[serde(default)]`, so a plugin emitting only `{uid, title, start, end}`
/// works, and adding a field later breaks no existing plugin. Deliberately no
/// `deny_unknown_fields` — unknown keys are ignored so a *newer* plugin can
/// talk to an older thegn, and [`CalEvent::extra`] carries anything a provider
/// wants to round-trip explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalEvent {
    /// The provider's native UID. Unique within a source, not globally.
    pub uid: String,
    pub title: String,
    pub start: EventTime,
    pub end: EventTime,

    #[serde(default)]
    pub source: SourceId,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub status: EventStatus,
    #[serde(default)]
    pub busy: Busy,
    /// Which calendar within the account (a CalDAV collection, an ICS
    /// `X-WR-CALNAME`), for display and filtering.
    #[serde(default)]
    pub calendar: String,
    #[serde(default)]
    pub category: String,
    /// A *semantic* hue, never RGB — the host resolves it against the active
    /// theme, following the `StyleRole` precedent in [`crate::plugin_api`].
    #[serde(default)]
    pub color: Option<crate::theme::Hue>,
    #[serde(default)]
    pub organizer: String,
    /// The repeat rule, if any. Parsed in full — including `BY*` parts the
    /// expander may not act on — so a rule round-trips losslessly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<Recurrence>,
    #[serde(default)]
    pub reminders: Vec<Reminder>,
    /// Provider validator for conditional fetches (an ICS `ETag`).
    #[serde(default)]
    pub etag: String,
    #[serde(default)]
    pub updated_at_ms: i64,
    /// Provider passthrough. Explicit, so round-tripping unknown data is a
    /// deliberate act rather than an accident of the serde config.
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

impl CalEvent {
    /// A minimal event — the shape a one-line shell plugin emits.
    pub fn new(
        uid: impl Into<String>,
        title: impl Into<String>,
        start: EventTime,
        end: EventTime,
    ) -> Self {
        CalEvent {
            uid: uid.into(),
            title: title.into(),
            start,
            end,
            source: SourceId::default(),
            description: String::new(),
            location: String::new(),
            url: String::new(),
            status: EventStatus::default(),
            busy: Busy::default(),
            calendar: String::new(),
            category: String::new(),
            color: None,
            organizer: String::new(),
            recurrence: None,
            reminders: Vec::new(),
            etag: String::new(),
            updated_at_ms: 0,
            extra: BTreeMap::new(),
        }
    }

    /// The globally unique id for this event.
    pub fn id(&self) -> EventId {
        EventId::new(self.source.as_str(), &self.uid)
    }

    /// Whether this event occupies whole days rather than a time span.
    pub fn all_day(&self) -> bool {
        self.start.is_all_day()
    }

    /// Every date this event touches, as seen from `home`, inclusive of both
    /// ends — so a multi-day event marks each of its days in the month grid.
    ///
    /// An all-day event's DTEND is *exclusive* per RFC 5545 (a one-day event
    /// ends the next midnight), so the last day is trimmed back; a timed event
    /// ending at 00:00 is treated the same way rather than bleeding a marker
    /// onto a day it does not actually occupy.
    pub fn dates_in(&self, home: chrono_tz::Tz) -> Vec<NaiveDate> {
        let Some(first) = self.start.date_in(home) else {
            return Vec::new();
        };
        let Some(mut last) = self.end.date_in(home) else {
            return vec![first];
        };
        if last > first && self.ends_at_midnight(home) {
            last = last.pred_opt().unwrap_or(last);
        }
        if last < first {
            return vec![first];
        }
        let mut out = Vec::new();
        let mut d = first;
        while d <= last {
            out.push(d);
            let Some(next) = d.succ_opt() else { break };
            d = next;
        }
        out
    }

    fn ends_at_midnight(&self, home: chrono_tz::Tz) -> bool {
        match &self.end {
            // An all-day DTEND is exclusive by definition.
            EventTime::Date { .. } => true,
            other => {
                use chrono::{TimeZone, Timelike};
                other
                    .instant_in(home, GapPolicy::ShiftForward)
                    .map(|at| {
                        let l = home.from_utc_datetime(&at.naive_utc());
                        l.hour() == 0 && l.minute() == 0 && l.second() == 0
                    })
                    .unwrap_or(false)
            }
        }
    }
}

/// One occurrence of an event: the event itself plus this instance's times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub event: CalEvent,
    pub start: EventTime,
    pub end: EventTime,
}

impl CalEvent {
    /// Every occurrence of this event whose date falls in `[from, to]`.
    ///
    /// A non-recurring event yields itself when it overlaps. A recurring one is
    /// expanded through [`recur`], which walks local wall time in the event's
    /// own zone so each instance lands at the right instant across DST.
    ///
    /// The window bounds the walk, so an endless `RRULE` costs no more than a
    /// finite one.
    pub fn occurrences(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        home: chrono_tz::Tz,
    ) -> Vec<Occurrence> {
        let Some(rec) = self.recurrence.as_ref().filter(|r| !r.is_empty()) else {
            // Not recurring: it occurs once, if it touches the window at all.
            let hit = self.dates_in(home).iter().any(|d| *d >= from && *d <= to);
            return if hit {
                vec![Occurrence {
                    event: self.clone(),
                    start: self.start.clone(),
                    end: self.end.clone(),
                }]
            } else {
                Vec::new()
            };
        };
        // The nominal LOCAL span, carried onto every instance — so a one-hour
        // meeting stays one hour of wall time even across a boundary, rather
        // than becoming 0 or 2 hours' worth of instants.
        let span = nominal_span(self, home);
        recur::occurrences(rec, &self.start, from, to, home, GapPolicy::ShiftForward)
            .into_iter()
            .map(|start| Occurrence {
                event: self.clone(),
                end: shift(&start, span),
                start,
            })
            .collect()
    }
}

/// DTEND − DTSTART measured in local wall time.
fn nominal_span(e: &CalEvent, home: chrono_tz::Tz) -> chrono::Duration {
    let local = |t: &EventTime| -> Option<NaiveDateTime> {
        match t {
            EventTime::Date { date } => date.and_hms_opt(0, 0, 0),
            EventTime::Zoned { local, .. } => Some(*local),
            EventTime::Instant { at } => {
                use chrono::TimeZone;
                Some(home.from_utc_datetime(&at.naive_utc()).naive_local())
            }
        }
    };
    match (local(&e.start), local(&e.end)) {
        (Some(a), Some(b)) if b >= a => b - a,
        _ => chrono::Duration::zero(),
    }
}

/// Advance an event time by a wall-clock duration, keeping its shape.
fn shift(t: &EventTime, by: chrono::Duration) -> EventTime {
    match t {
        EventTime::Date { date } => EventTime::Date {
            date: *date + chrono::Duration::days(by.num_days().max(1)),
        },
        EventTime::Zoned { local, zone } => EventTime::Zoned {
            local: *local + by,
            zone: zone.clone(),
        },
        EventTime::Instant { at } => EventTime::Instant { at: *at + by },
    }
}

/// Expand many events over a window, bucketed by the date each occurrence
/// occupies — the shape the month grid and agenda both want.
pub fn expand_by_date(
    events: &[CalEvent],
    from: NaiveDate,
    to: NaiveDate,
    home: chrono_tz::Tz,
) -> BTreeMap<NaiveDate, Vec<CalEvent>> {
    let mut out: BTreeMap<NaiveDate, Vec<CalEvent>> = BTreeMap::new();
    for e in events {
        for occ in e.occurrences(from, to, home) {
            // Each occurrence is materialized as a concrete, non-recurring
            // event: the UI never has to re-expand, and a cached day bucket is
            // meaningful on its own.
            let mut inst = occ.event.clone();
            inst.start = occ.start;
            inst.end = occ.end;
            inst.recurrence = None;
            for date in inst.dates_in(home) {
                if date >= from && date <= to {
                    out.entry(date).or_default().push(inst.clone());
                }
            }
        }
    }
    for v in out.values_mut() {
        // All-day first, then by start time — the order an agenda reads in.
        v.sort_by(|a, b| {
            b.all_day().cmp(&a.all_day()).then_with(|| {
                a.start
                    .instant_in(home, GapPolicy::ShiftForward)
                    .cmp(&b.start.instant_in(home, GapPolicy::ShiftForward))
                    .then_with(|| a.title.cmp(&b.title))
            })
        });
    }
    out
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
