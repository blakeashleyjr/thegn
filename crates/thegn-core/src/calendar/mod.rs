//! Calendar domain: month-grid geometry, a navigation cursor, IANA world
//! clocks, and the event model the providers and the popup share.
//!
//! **Everything here is pure.** No I/O, no `Local::now()`, no globals: every
//! entry point takes `now` and the home zone explicitly. That is what lets the
//! whole module be exhaustively unit-tested under the 95% core coverage gate,
//! and what lets the popup render any month instantly without a round trip.
//!
//! Layout:
//! - [`grid`] — the 7×6 month matrix (leading/trailing days, configured-start week numbers).
//! - [`cursor`] — the selection state machine (`h`/`j`/`k`/`l`, month/year paging).
//! - [`tz`] — zone resolution and world-clock readings.
//! - [`locale`] — resolving `week_start = "auto"` / `time_format = "auto"`.
//!
//! Naming note: the event type is [`CalEvent`], never `Event` —
//! [`crate::event_bus::Event`] already owns that name.

pub mod admission;
pub mod cursor;
pub mod display;
pub mod grid;
pub mod ics;
pub mod locale;
pub mod recur;
pub mod reminders;
pub mod tz;

pub use admission::{
    AdmissionBudget, AdmissionError, AdmissionLease, AdmissionLimit, AdmissionMeter, AdmissionPool,
};
pub use cursor::{CalCursor, CalNav};
pub use grid::{DayCell, MonthGrid, WeekdayStyle, month_bounds, weekday_headers};
pub use ics::{parse_ics, parse_ics_admitted, parse_ics_window};
pub use locale::{resolve_time_format, resolve_week_start};
pub use recur::{ByDay, Freq, RRule, RecurError, Recurrence};
pub use reminders::{DueReminder, next_event};
pub use tz::{
    ClockFormat, ClockReading, GapPolicy, ResolvedClock, TzRef, read_clocks, resolve_zone,
};

use chrono::{DateTime, Days, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Hard ceilings for one calendar expansion. These are deliberately code
/// constants: accepting them from configuration would make an untrusted
/// source able to disable the admission boundary.
///
/// The output ceilings are sized against what an admitted cache can
/// legitimately produce, so a real calendar never hits them (a row that is
/// merely malformed is skipped per-row instead — see
/// [`ExpansionError::is_row_local`]). The arithmetic, measured and pinned by
/// `default_ceilings_admit_a_heavy_but_legitimate_month`:
///
/// - `[calendar] max_events` admits 2,000 rows per account, and the month
///   window is widened by a week either side, so the heaviest legitimate row
///   — a daily recurrence — yields ~49 occurrences. A saturated account is
///   therefore ~98,000 occurrences, and ~106,000 expanded locals (the walk
///   looks back by the event's own duration, so it produces a few more locals
///   than it materializes). Both are held to `MAX_EXPANSION_OCCURRENCES`,
///   which leaves ~24% over the tighter of the two.
/// - Bucket entries follow occupancy: a long occurrence occupies several
///   days, so four entries per occurrence.
/// - Recurrence work is ~32 units per materialized occurrence (period +
///   candidate + filter passes).
/// - Retained bytes are the dimension that actually binds, deliberately: at
///   the measured ~680 bytes per occurrence a saturated account costs ~66 MB
///   against this 64 MiB ceiling — about 1% of slack. That is the contract,
///   not an oversight: memory, rather than an arbitrary count, is what stops
///   an expansion. Growing [`CalEvent`] eats that slack, which is why the
///   test above asserts the whole-account total from the MEASURED
///   per-occurrence cost. When it fails, re-derive this ceiling instead of
///   loosening the test, or a saturated account will report its month
///   unavailable in production while the suite stays green.
pub const MAX_EXPANSION_WINDOW_DAYS: u64 = 3_660;
pub const MAX_EXPANSION_SOURCE_VISITS: usize = 65_536;
pub const MAX_EXPANSION_RECURRENCE_WORK: usize = 4_194_304;
pub const MAX_EXPANSION_OCCURRENCES: usize = 131_072;
pub const MAX_EXPANSION_BUCKET_ENTRIES: usize = 524_288;
pub const MAX_EXPANSION_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;
pub const MAX_EVENT_CHILD_ENTRIES: usize = 16_384;

/// The accounting dimension that stopped an expansion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpansionLimit {
    WindowDays,
    SourceVisits,
    RecurrenceWork,
    MaterializedOccurrences,
    BucketEntries,
    RetainedBytes,
    EventPayload,
    EventChildren,
}

/// Expansion failures are intentionally small and do not contain provider
/// values. Callers can expose the state to a user without leaking an event
/// payload or allowing a hostile value to inflate logs/UI state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpansionError {
    InvalidWindow,
    InvalidSpan,
    Arithmetic,
    Budget(ExpansionLimit),
}

impl ExpansionError {
    /// Whether this failure is a property of ONE source row rather than of the
    /// call: a malformed span, or a payload/child-count ceiling that only that
    /// row exceeded.
    ///
    /// Row-local failures are skipped and counted (the view reports itself
    /// incomplete) instead of failing the whole expansion: one bad row in a
    /// provider's cache must not be able to blank a month or — since the row
    /// stays in the cache and every retry re-reads the same bytes — stop
    /// reminders permanently. Everything else (the shared budget dimensions,
    /// an invalid window, arithmetic) is a property of the call and stays a
    /// hard failure.
    pub fn is_row_local(&self) -> bool {
        matches!(
            self,
            Self::InvalidSpan
                | Self::Budget(ExpansionLimit::EventPayload)
                | Self::Budget(ExpansionLimit::EventChildren)
        )
    }
}

impl fmt::Display for ExpansionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWindow => f.write_str("calendar window is invalid"),
            Self::InvalidSpan => f.write_str("calendar event span is invalid"),
            Self::Arithmetic => f.write_str("calendar date arithmetic overflowed"),
            Self::Budget(limit) => write!(f, "calendar expansion budget exhausted ({limit})"),
        }
    }
}

impl fmt::Display for ExpansionLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WindowDays => "window days",
            Self::SourceVisits => "source rows",
            Self::RecurrenceWork => "recurrence work",
            Self::MaterializedOccurrences => "occurrences",
            Self::BucketEntries => "day entries",
            Self::RetainedBytes => "retained bytes",
            Self::EventPayload => "event payload",
            Self::EventChildren => "event children",
        })
    }
}

/// Shared accounting for the complete expansion call. A caller must create
/// one value and pass it through every source row and recurrence; renewing it
/// per event would turn the limits into a per-event bypass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpansionBudget {
    pub max_window_days: u64,
    pub max_source_visits: usize,
    pub max_recurrence_work: usize,
    pub max_occurrences: usize,
    pub max_bucket_entries: usize,
    pub max_retained_bytes: usize,
    source_visits: usize,
    recurrence_work: usize,
    occurrences: usize,
    expanded_locals: usize,
    bucket_entries: usize,
    retained_bytes: usize,
}

impl Default for ExpansionBudget {
    fn default() -> Self {
        Self {
            max_window_days: MAX_EXPANSION_WINDOW_DAYS,
            max_source_visits: MAX_EXPANSION_SOURCE_VISITS,
            max_recurrence_work: MAX_EXPANSION_RECURRENCE_WORK,
            max_occurrences: MAX_EXPANSION_OCCURRENCES,
            max_bucket_entries: MAX_EXPANSION_BUCKET_ENTRIES,
            max_retained_bytes: MAX_EXPANSION_BYTES,
            source_visits: 0,
            recurrence_work: 0,
            occurrences: 0,
            expanded_locals: 0,
            bucket_entries: 0,
            retained_bytes: 0,
        }
    }
}

impl ExpansionBudget {
    /// A smaller budget is useful for deterministic unit tests. Zero means
    /// zero capacity, never unlimited capacity.
    pub fn limited(
        max_window_days: u64,
        max_source_visits: usize,
        max_recurrence_work: usize,
        max_occurrences: usize,
        max_bucket_entries: usize,
        max_retained_bytes: usize,
    ) -> Self {
        Self {
            max_window_days,
            max_source_visits,
            max_recurrence_work,
            max_occurrences,
            max_bucket_entries,
            max_retained_bytes,
            ..Self::default()
        }
    }

    fn spend(
        used: &mut usize,
        max: usize,
        amount: usize,
        limit: ExpansionLimit,
    ) -> Result<(), ExpansionError> {
        let next = used.checked_add(amount).ok_or(ExpansionError::Arithmetic)?;
        if next > max {
            return Err(ExpansionError::Budget(limit));
        }
        *used = next;
        Ok(())
    }

    pub(crate) fn source_visit(&mut self) -> Result<(), ExpansionError> {
        let max = self.max_source_visits;
        Self::spend(
            &mut self.source_visits,
            max,
            1,
            ExpansionLimit::SourceVisits,
        )
    }

    pub(crate) fn recurrence_work(&mut self, amount: usize) -> Result<(), ExpansionError> {
        let max = self.max_recurrence_work;
        Self::spend(
            &mut self.recurrence_work,
            max,
            amount,
            ExpansionLimit::RecurrenceWork,
        )
    }

    /// Reserve one recurrence candidate before it is built. Charged on two
    /// dimensions because a candidate is both work AND memory: the period's
    /// candidate vector is real bytes, and at the raised work ceiling an
    /// unbilled one would be the largest allocation in the pass.
    pub(crate) fn candidate(&mut self) -> Result<(), ExpansionError> {
        self.recurrence_work(1)?;
        self.bytes(CANDIDATE_BYTES)
    }

    /// Reserve one expanded local time before it is retained. Every local
    /// becomes at most one materialized occurrence, so it is held to the same
    /// ceiling (on its own counter, so the two passes do not double-charge)
    /// and to its share of the byte budget.
    pub(crate) fn expanded_local(&mut self) -> Result<(), ExpansionError> {
        let max = self.max_occurrences;
        Self::spend(
            &mut self.expanded_locals,
            max,
            1,
            ExpansionLimit::MaterializedOccurrences,
        )?;
        self.bytes(CANDIDATE_BYTES)
    }

    fn occurrence(&mut self) -> Result<(), ExpansionError> {
        let max = self.max_occurrences;
        Self::spend(
            &mut self.occurrences,
            max,
            1,
            ExpansionLimit::MaterializedOccurrences,
        )
    }

    fn bucket_entry(&mut self) -> Result<(), ExpansionError> {
        let max = self.max_bucket_entries;
        Self::spend(
            &mut self.bucket_entries,
            max,
            1,
            ExpansionLimit::BucketEntries,
        )
    }

    fn bytes(&mut self, amount: usize) -> Result<(), ExpansionError> {
        let max = self.max_retained_bytes;
        Self::spend(
            &mut self.retained_bytes,
            max,
            amount,
            ExpansionLimit::RetainedBytes,
        )
    }

    pub fn used_source_visits(&self) -> usize {
        self.source_visits
    }
    pub fn used_recurrence_work(&self) -> usize {
        self.recurrence_work
    }
    pub fn used_occurrences(&self) -> usize {
        self.occurrences
    }
    pub fn used_expanded_locals(&self) -> usize {
        self.expanded_locals
    }
    pub fn used_bucket_entries(&self) -> usize {
        self.bucket_entries
    }
    pub fn used_retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

/// The expanded month/reminder result. Buckets contain handles only; the
/// unique occurrence list owns each materialized event payload once.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExpandedCalendar {
    pub by_date: BTreeMap<NaiveDate, Vec<Arc<CalEvent>>>,
    pub occurrences: Vec<Arc<CalEvent>>,
    /// Source rows skipped for a row-local defect (see
    /// [`ExpansionError::is_row_local`]). Non-zero means this result is
    /// incomplete — truthful, but not the whole calendar.
    pub skipped: usize,
}

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

    /// Every date this event touches, as seen from `home` — test-only: it
    /// walks the whole lifetime. Production uses [`Self::occupied_dates_in`],
    /// which intersects the span with a window before any date is produced.
    #[cfg(test)]
    pub fn dates_in(&self, home: chrono_tz::Tz) -> Vec<NaiveDate> {
        match span_dates(&self.start, &self.end, home) {
            Ok((first, last)) => OccupiedDates::new(first, last).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The dates in `[from, to]` this event occupies, as seen from `home`.
    ///
    /// The event's span is resolved and clamped to the window FIRST, so the
    /// iterator is empty for a non-overlap and never walks the event's
    /// lifetime outside the window. An all-day DTEND is exclusive (RFC 5545);
    /// a timed end at exactly local midnight is treated the same way rather
    /// than bleeding a marker onto a day it does not occupy. A zero-length
    /// event is a point on its start date. An end before the start is
    /// [`ExpansionError::InvalidSpan`], never a fabricated one-day event.
    pub fn occupied_dates_in(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        home: chrono_tz::Tz,
    ) -> Result<OccupiedDates, ExpansionError> {
        if from > to {
            return Err(ExpansionError::InvalidWindow);
        }
        let (first, last) = span_dates(&self.start, &self.end, home)?;
        Ok(OccupiedDates::new(first.max(from), last.min(to)))
    }

    /// A concrete, non-recurring copy of this event at one instance's times.
    /// Built field by field so a (possibly large) recurrence is never cloned
    /// just to be thrown away.
    fn materialize(&self, start: EventTime, end: EventTime) -> CalEvent {
        CalEvent {
            uid: self.uid.clone(),
            title: self.title.clone(),
            start,
            end,
            source: self.source.clone(),
            description: self.description.clone(),
            location: self.location.clone(),
            url: self.url.clone(),
            status: self.status,
            busy: self.busy,
            calendar: self.calendar.clone(),
            category: self.category.clone(),
            color: self.color,
            organizer: self.organizer.clone(),
            recurrence: None,
            reminders: self.reminders.clone(),
            etag: self.etag.clone(),
            updated_at_ms: self.updated_at_ms,
            extra: self.extra.clone(),
        }
    }
}

/// Whether an end time is exactly local midnight in `home` (sub-seconds
/// included: 00:00:00.500 is NOT midnight). An all-day DTEND always is.
fn ends_at_midnight(end: &EventTime, home: chrono_tz::Tz) -> bool {
    match end {
        EventTime::Date { .. } => true,
        other => {
            use chrono::{TimeZone, Timelike};
            other
                .instant_in(home, GapPolicy::ShiftForward)
                .map(|at| {
                    let l = home.from_utc_datetime(&at.naive_utc());
                    l.hour() == 0 && l.minute() == 0 && l.second() == 0 && l.nanosecond() == 0
                })
                .unwrap_or(false)
        }
    }
}

/// Resolve a `[start, end)` span to the inclusive date range it occupies in
/// `home`, validating it once.
///
/// Order is checked in the span's own frame — dates for an all-day span, wall
/// time for two times in one zone, instants otherwise — so a same-day inverted
/// span is refused, while a valid wall-time span whose start falls in a DST gap
/// (and so resolves a little later) stays a valid point rather than an error.
fn span_dates(
    start: &EventTime,
    end: &EventTime,
    home: chrono_tz::Tz,
) -> Result<(NaiveDate, NaiveDate), ExpansionError> {
    let ordered = match (start, end) {
        (EventTime::Date { date: a }, EventTime::Date { date: b }) => a <= b,
        (EventTime::Zoned { local: a, zone: za }, EventTime::Zoned { local: b, zone: zb })
            if za == zb =>
        {
            a <= b
        }
        _ => {
            let a = start
                .instant_in(home, GapPolicy::ShiftForward)
                .ok_or(ExpansionError::InvalidSpan)?;
            let b = end
                .instant_in(home, GapPolicy::ShiftForward)
                .ok_or(ExpansionError::InvalidSpan)?;
            a <= b
        }
    };
    if !ordered {
        return Err(ExpansionError::InvalidSpan);
    }
    let first = start.date_in(home).ok_or(ExpansionError::InvalidSpan)?;
    let mut last = end.date_in(home).ok_or(ExpansionError::InvalidSpan)?;
    if last > first && ends_at_midnight(end, home) {
        last = last.pred_opt().ok_or(ExpansionError::Arithmetic)?;
    }
    // Only a gap-shifted start can resolve past a valid end; it is a point.
    Ok((first, last.max(first)))
}

/// Inclusive date walk that is empty when `first > last` and stops at the
/// last representable date instead of overflowing.
#[derive(Debug, Clone)]
pub struct OccupiedDates {
    next: Option<NaiveDate>,
    last: NaiveDate,
}

impl OccupiedDates {
    fn new(first: NaiveDate, last: NaiveDate) -> Self {
        OccupiedDates {
            next: (first <= last).then_some(first),
            last,
        }
    }
}

impl Iterator for OccupiedDates {
    type Item = NaiveDate;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.next?;
        self.next = current.succ_opt().filter(|next| *next <= self.last);
        Some(current)
    }
}

/// One occurrence of an event: the materialized (concrete, recurrence-free)
/// event plus this instance's times. The event is shared, so every date bucket
/// the occurrence occupies holds a handle rather than a payload copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub event: Arc<CalEvent>,
    pub start: EventTime,
    pub end: EventTime,
}

/// An occurrence plus the (window-clamped) dates it occupies.
struct Placed {
    occurrence: Occurrence,
    first: NaiveDate,
    last: NaiveDate,
}

impl CalEvent {
    /// Every occurrence of this event touching `[from, to]` — test-only
    /// convenience that swallows a refusal into an empty list. Production goes
    /// through [`Self::occurrences_bounded`] / [`expand_calendar`], where a
    /// refusal is an error the caller must surface.
    #[cfg(test)]
    pub fn occurrences(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        home: chrono_tz::Tz,
    ) -> Vec<Occurrence> {
        let mut budget = ExpansionBudget::default();
        self.occurrences_bounded(from, to, home, &mut budget)
            .unwrap_or_default()
    }

    /// Every occurrence of this event touching `[from, to]`, charged to
    /// `budget`.
    ///
    /// A non-recurring event yields itself when its span intersects the window
    /// (decided from its boundaries, never by walking its dates). A recurring
    /// one is expanded through [`recur`] in local wall time, so each instance
    /// lands at the right instant across DST — including instances that START
    /// before the window but run into it (the expansion looks back by the
    /// event's own duration).
    pub fn occurrences_bounded(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        home: chrono_tz::Tz,
        budget: &mut ExpansionBudget,
    ) -> Result<Vec<Occurrence>, ExpansionError> {
        if from > to {
            return Err(ExpansionError::InvalidWindow);
        }
        let cost = source_cost(self)?;
        Ok(self
            .place(from, to, home, budget, &cost)?
            .into_iter()
            .map(|p| p.occurrence)
            .collect())
    }

    fn place(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        home: chrono_tz::Tz,
        budget: &mut ExpansionBudget,
        cost: &SourceCost,
    ) -> Result<Vec<Placed>, ExpansionError> {
        let Some(rec) = self.recurrence.as_ref().filter(|r| !r.is_empty()) else {
            // Not recurring: it occurs once, if its span touches the window.
            let (first, last) = span_dates(&self.start, &self.end, home)?;
            if last < from || first > to {
                return Ok(Vec::new());
            }
            let occurrence =
                self.charge_and_materialize(self.start.clone(), self.end.clone(), budget, cost)?;
            return Ok(vec![Placed {
                occurrence,
                first: first.max(from),
                last: last.min(to),
            }]);
        };
        // The nominal LOCAL span, carried onto every instance — so a one-hour
        // meeting stays one hour of wall time even across a boundary, rather
        // than becoming 0 or 2 hours' worth of instants.
        let span = nominal_span(self, home).ok_or(ExpansionError::InvalidSpan)?;
        // Instances starting up to one duration before the window still reach
        // into it, and a zone's local date can sit up to two days either side
        // of `home`'s. Nothing starts before the first representable date, so
        // saturating there is exact rather than a silent clamp.
        let lookback = u64::try_from(span.num_days())
            .ok()
            .and_then(|d| d.checked_add(ZONE_SLACK_DAYS))
            .ok_or(ExpansionError::Arithmetic)?;
        let rec_from = from
            .checked_sub_days(Days::new(lookback))
            .unwrap_or(NaiveDate::MIN);
        let rec_to = to
            .checked_add_days(Days::new(ZONE_SLACK_DAYS))
            .unwrap_or(NaiveDate::MAX);
        let Some(seed) = recur::seed_of(&self.start, home) else {
            return Err(ExpansionError::InvalidSpan);
        };
        let locals = recur::expand_local_bounded(rec, seed, rec_from, rec_to, budget)?;
        let mut out = Vec::new();
        for local in locals {
            // One transient instance at a time: nothing is retained (or
            // charged) until it is known to overlap the window.
            let start = recur::instance_time(local, &self.start, home);
            if !start.is_all_day() && start.instant_in(home, GapPolicy::ShiftForward).is_none() {
                continue;
            }
            let end = shift(&start, span).ok_or(ExpansionError::Arithmetic)?;
            let (first, last) = span_dates(&start, &end, home)?;
            if last < from || first > to {
                continue;
            }
            let occurrence = self.charge_and_materialize(start, end, budget, cost)?;
            out.push(Placed {
                occurrence,
                first: first.max(from),
                last: last.min(to),
            });
        }
        Ok(out)
    }

    /// Reserve an occurrence and its retained bytes, THEN clone the payload.
    fn charge_and_materialize(
        &self,
        start: EventTime,
        end: EventTime,
        budget: &mut ExpansionBudget,
        cost: &SourceCost,
    ) -> Result<Occurrence, ExpansionError> {
        let bytes = cost
            .materialized
            .checked_add(OCCURRENCE_OVERHEAD)
            .and_then(|n| n.checked_add(tz_bytes(&start)))
            .and_then(|n| n.checked_add(tz_bytes(&end)))
            .ok_or(ExpansionError::Arithmetic)?;
        budget.occurrence()?;
        budget.bytes(bytes)?;
        let event = Arc::new(self.materialize(start.clone(), end.clone()));
        Ok(Occurrence { event, start, end })
    }
}

/// Days of slack either side of a recurrence window for zone offsets: a local
/// date in the event's zone can be up to ~26h from the same moment in `home`.
const ZONE_SLACK_DAYS: u64 = 2;

/// Conservative per-entry share of a `BTreeMap` node (key/value slots, edges,
/// and node header amortized).
const MAP_ENTRY_OVERHEAD: usize = 64;

/// Retained bytes an occurrence adds beyond its materialized payload: the
/// `Arc` header, the `Occurrence` record, and its slot in the unique list
/// (doubled for `Vec` growth).
const OCCURRENCE_OVERHEAD: usize = 2 * std::mem::size_of::<usize>()
    + std::mem::size_of::<Occurrence>()
    + 2 * std::mem::size_of::<Arc<CalEvent>>();

/// Retained bytes of one expanded local time or recurrence candidate
/// (doubled for `Vec` growth).
const CANDIDATE_BYTES: usize = 2 * std::mem::size_of::<NaiveDateTime>();

/// Retained bytes of one bucket handle (doubled for `Vec` growth).
const BUCKET_ENTRY_BYTES: usize = 2 * std::mem::size_of::<Arc<CalEvent>>();

/// Retained bytes of a new date key in the bucket map.
const BUCKET_KEY_BYTES: usize = std::mem::size_of::<NaiveDate>()
    + std::mem::size_of::<Vec<Arc<CalEvent>>>()
    + MAP_ENTRY_OVERHEAD;

/// DTEND − DTSTART measured in local wall time; `None` for an inverted or
/// unrepresentable span.
fn nominal_span(e: &CalEvent, home: chrono_tz::Tz) -> Option<chrono::Duration> {
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
        (Some(a), Some(b)) if b >= a => Some(b.signed_duration_since(a)),
        _ => None,
    }
}

/// Advance an event time by a wall-clock duration, keeping its shape; `None`
/// past the representable range.
fn shift(t: &EventTime, by: chrono::Duration) -> Option<EventTime> {
    match t {
        EventTime::Date { date } => {
            let days = u64::try_from(by.num_days().max(1)).ok()?;
            Some(EventTime::Date {
                date: date.checked_add_days(Days::new(days))?,
            })
        }
        EventTime::Zoned { local, zone } => Some(EventTime::Zoned {
            local: local.checked_add_signed(by)?,
            zone: zone.clone(),
        }),
        EventTime::Instant { at } => Some(EventTime::Instant {
            at: at.checked_add_signed(by)?,
        }),
    }
}

fn tz_bytes(t: &EventTime) -> usize {
    match t {
        EventTime::Zoned { zone, .. } => zone.0.len(),
        _ => 0,
    }
}

/// What one source row costs, measured before anything is cloned.
struct SourceCost {
    /// Retained bytes of one materialized (recurrence-free) copy.
    materialized: usize,
}

fn add(n: usize, m: usize) -> Result<usize, ExpansionError> {
    n.checked_add(m).ok_or(ExpansionError::Arithmetic)
}

fn mul(n: usize, m: usize) -> Result<usize, ExpansionError> {
    n.checked_mul(m).ok_or(ExpansionError::Arithmetic)
}

/// Validate one source row's size and price its materialized copy.
///
/// Applied to EVERY row, overlapping or not, so the preflight itself stays
/// bounded: child counts are read from lengths (O(1)) and checked before any
/// child collection is walked, and the walked ones are then capped.
fn source_cost(e: &CalEvent) -> Result<SourceCost, ExpansionError> {
    use std::mem::size_of;
    let mut children = add(e.reminders.len(), e.extra.len())?;
    if let Some(r) = &e.recurrence {
        children = add(children, r.rules.len())?;
        children = add(children, r.rdates.len())?;
        children = add(children, r.exdates.len())?;
    }
    if children > MAX_EVENT_CHILD_ENTRIES {
        return Err(ExpansionError::Budget(ExpansionLimit::EventChildren));
    }
    let mut recurrence_bytes = 0usize;
    if let Some(r) = &e.recurrence {
        recurrence_bytes = add(
            size_of::<recur::Recurrence>(),
            mul(r.rules.len(), size_of::<RRule>())?,
        )?;
        for rule in &r.rules {
            let parts = [
                (rule.by_second.len(), size_of::<u32>()),
                (rule.by_minute.len(), size_of::<u32>()),
                (rule.by_hour.len(), size_of::<u32>()),
                (rule.by_day.len(), size_of::<ByDay>()),
                (rule.by_month_day.len(), size_of::<i8>()),
                (rule.by_year_day.len(), size_of::<i16>()),
                (rule.by_week_no.len(), size_of::<i8>()),
                (rule.by_month.len(), size_of::<u32>()),
                (rule.by_set_pos.len(), size_of::<i32>()),
            ];
            for (len, each) in parts {
                children = add(children, len)?;
                recurrence_bytes = add(recurrence_bytes, mul(len, each)?)?;
            }
        }
        if children > MAX_EVENT_CHILD_ENTRIES {
            return Err(ExpansionError::Budget(ExpansionLimit::EventChildren));
        }
        for t in r.rdates.iter().chain(&r.exdates) {
            recurrence_bytes = add(recurrence_bytes, size_of::<EventTime>())?;
            recurrence_bytes = add(recurrence_bytes, tz_bytes(t))?;
        }
    }
    let strings = [
        &e.uid,
        &e.title,
        &e.description,
        &e.location,
        &e.url,
        &e.calendar,
        &e.category,
        &e.organizer,
        &e.etag,
        &e.source.0,
    ];
    let mut materialized = size_of::<CalEvent>();
    for s in strings {
        materialized = add(materialized, s.len())?;
    }
    materialized = add(materialized, tz_bytes(&e.start))?;
    materialized = add(materialized, tz_bytes(&e.end))?;
    materialized = add(materialized, mul(e.reminders.len(), size_of::<Reminder>())?)?;
    for (k, v) in &e.extra {
        materialized = add(materialized, k.len())?;
        materialized = add(materialized, v.len())?;
        materialized = add(materialized, 2 * size_of::<String>() + MAP_ENTRY_OVERHEAD)?;
    }
    if add(materialized, recurrence_bytes)? > MAX_EVENT_PAYLOAD_BYTES {
        return Err(ExpansionError::Budget(ExpansionLimit::EventPayload));
    }
    Ok(SourceCost { materialized })
}

/// Expand many events over a window with the default hard budget. The
/// returned buckets share each materialized occurrence with `occurrences`, so
/// reminders can evaluate a multi-day event once instead of once per day.
pub fn expand_calendar(
    events: &[CalEvent],
    from: NaiveDate,
    to: NaiveDate,
    home: chrono_tz::Tz,
) -> Result<ExpandedCalendar, ExpansionError> {
    let mut budget = ExpansionBudget::default();
    expand_calendar_with_budget(events, from, to, home, &mut budget)
}

/// [`expand_calendar`] against a caller-supplied budget, which is charged for
/// the WHOLE call — every source row, recurrence, occurrence, and bucket entry
/// — and is never renewed per event. Any exhaustion or invalid span is an
/// error: a truncated calendar is never returned as if it were complete.
pub fn expand_calendar_with_budget(
    events: &[CalEvent],
    from: NaiveDate,
    to: NaiveDate,
    home: chrono_tz::Tz,
    budget: &mut ExpansionBudget,
) -> Result<ExpandedCalendar, ExpansionError> {
    if from > to {
        return Err(ExpansionError::InvalidWindow);
    }
    let days = u64::try_from(to.signed_duration_since(from).num_days())
        .ok()
        .and_then(|n| n.checked_add(1))
        .ok_or(ExpansionError::Arithmetic)?;
    if days > budget.max_window_days {
        return Err(ExpansionError::Budget(ExpansionLimit::WindowDays));
    }
    let mut out: BTreeMap<NaiveDate, Vec<Arc<CalEvent>>> = BTreeMap::new();
    let mut unique = Vec::new();
    let mut skipped = 0usize;
    for e in events {
        budget.source_visit()?;
        // A row-local defect costs that row, not the call: it is skipped and
        // counted, so the caller can report an incomplete view instead of
        // losing the month (and the reminders) to one bad event.
        let placed = match source_cost(e).and_then(|cost| e.place(from, to, home, budget, &cost)) {
            Ok(placed) => placed,
            Err(error) if error.is_row_local() => {
                skipped = skipped.checked_add(1).ok_or(ExpansionError::Arithmetic)?;
                continue;
            }
            Err(error) => return Err(error),
        };
        for placed in placed {
            for date in OccupiedDates::new(placed.first, placed.last) {
                budget.bucket_entry()?;
                let bucket = match out.get_mut(&date) {
                    Some(bucket) => {
                        budget.bytes(BUCKET_ENTRY_BYTES)?;
                        bucket
                    }
                    None => {
                        budget.bytes(BUCKET_KEY_BYTES + BUCKET_ENTRY_BYTES)?;
                        out.entry(date).or_default()
                    }
                };
                bucket.push(Arc::clone(&placed.occurrence.event));
            }
            unique.push(placed.occurrence.event);
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
    Ok(ExpandedCalendar {
        by_date: out,
        occurrences: unique,
        skipped,
    })
}

/// Date buckets only, for callers that do not need the unique list.
pub fn expand_by_date(
    events: &[CalEvent],
    from: NaiveDate,
    to: NaiveDate,
    home: chrono_tz::Tz,
) -> Result<BTreeMap<NaiveDate, Vec<Arc<CalEvent>>>, ExpansionError> {
    Ok(expand_calendar(events, from, to, home)?.by_date)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
