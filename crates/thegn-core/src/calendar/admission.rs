//! Source admission: how much one calendar fetch may bring into memory.
//!
//! `[calendar] max_events` used to be advisory — nothing forwarded it, zero
//! meant "unlimited", and every backend truncated a vector it had already
//! built. This module makes it an **admission budget** instead:
//!
//! - [`AdmissionBudget`] is the typed, *nonzero* per-account event cap every
//!   backend is constructed with. Zero is rejected, never read as unlimited.
//! - [`AdmissionMeter`] is charged by the parser **before** it stores a
//!   retained value or builds the next [`CalEvent`], so an oversized source is
//!   refused at the first byte over the line rather than after it has been
//!   materialized.
//! - [`AdmissionPool`] is the process-wide ceiling across every account and
//!   every router instance. Reservation is non-blocking: a refusal is a typed
//!   error, so no fetch ever waits on another (no deadlock, no starvation — the
//!   refused account keeps its cache and cursor and retries on its next tick).
//! - [`AdmissionLease`] owns a reservation and releases it on drop. A fetched
//!   page owns its lease, so the global accounting lives exactly as long as the
//!   admitted data does.
//!
//! Overflow is all-or-nothing: an [`AdmissionError`] replaces the page. A
//! truncated snapshot is never presented as a complete calendar, which is what
//! keeps a cache replacement or a cursor advance from silently dropping the
//! events past the cap.
//!
//! The per-event and per-account byte ceilings match the expansion pass's
//! per-event payload and retained-byte ceilings, so anything admitted here is a
//! shape expansion will accept.

use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use super::{CalEvent, EventTime};

/// Smallest accepted `[calendar] max_events`.
pub const MIN_MAX_EVENTS: usize = 1;
/// Default `[calendar] max_events`.
pub const DEFAULT_MAX_EVENTS: usize = 2_000;
/// Largest accepted `[calendar] max_events`. Six accounts at this cap fit the
/// global record ceiling at once.
pub const MAX_MAX_EVENTS: usize = 10_000;

/// Retained bytes one event may account for (strings, recurrence, extras).
pub const MAX_EVENT_BYTES: usize = 1 << 20;
/// Child entries (extras, reminders, rules, RDATE/EXDATE values) per event.
pub const MAX_EVENT_CHILDREN: usize = 16_384;
/// Retained bytes an account may admit per event of its `max_events`
/// budget. Raising `max_events` raises the byte budget with it, so a byte
/// refusal is always fixable from the same knob.
pub const ACCOUNT_BYTES_PER_EVENT: usize = 8 << 10;
/// Floor of the per-account retained byte budget (the default budget's value).
pub const MIN_ACCOUNT_BYTES: usize = 32 << 20;
/// Ceiling of the per-account retained byte budget. Together with one
/// in-flight transport body it still fits the global byte ceiling.
pub const MAX_ACCOUNT_BYTES: usize = 96 << 20;
/// Largest single source document (a local `.ics` file, one CalDAV resource).
/// Equal to the remote transport's response cap.
pub const MAX_SOURCE_DOCUMENT_BYTES: usize = 32 << 20;
/// Largest unfolded iCalendar content line that is ever materialized.
pub const MAX_LINE_BYTES: usize = 1 << 20;
/// Deepest component nesting accepted inside a `VEVENT`.
pub const MAX_COMPONENT_DEPTH: usize = 16;
/// Admitted records (events + deletions) across every concurrent fetch.
pub const GLOBAL_MAX_RECORDS: usize = 65_536;
/// Reserved bytes (retained output + in-flight source documents) across every
/// concurrent fetch.
pub const GLOBAL_MAX_BYTES: usize = 128 << 20;

/// Conservative per-entry bookkeeping charged for every retained string, map
/// entry or child value on top of its payload bytes.
pub const ENTRY_OVERHEAD: usize = 64;

/// Which admission ceiling stopped a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionLimit {
    /// More events + deletions than the account's `max_events`.
    AccountRecords,
    /// More retained bytes than one account may admit.
    AccountBytes,
    /// One event's retained payload exceeds [`MAX_EVENT_BYTES`].
    EventBytes,
    /// One event carries more than [`MAX_EVENT_CHILDREN`] child values.
    EventChildren,
    /// A retained content line outside any event exceeds [`MAX_LINE_BYTES`].
    LineBytes,
    /// Components nest deeper than [`MAX_COMPONENT_DEPTH`].
    Nesting,
    /// One source document exceeds [`MAX_SOURCE_DOCUMENT_BYTES`].
    DocumentBytes,
    /// A plugin sent more messages than one run may.
    Messages,
    /// The process-wide record ceiling is in use by other fetches.
    GlobalRecords,
    /// The process-wide byte ceiling is in use by other fetches.
    GlobalBytes,
    /// A size computation overflowed.
    Arithmetic,
}

/// A fetch refused by its admission budget. Deliberately value-free: it never
/// carries provider data, so it is safe to log and to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionError {
    pub limit: AdmissionLimit,
}

impl AdmissionError {
    pub const fn new(limit: AdmissionLimit) -> Self {
        AdmissionError { limit }
    }

    /// Whether the refusal is about this account's own volume — the case
    /// where retrying a delta as a full (windowed) fetch may fit.
    pub fn is_account_limit(&self) -> bool {
        matches!(
            self.limit,
            AdmissionLimit::AccountRecords | AdmissionLimit::AccountBytes
        )
    }

    /// Whether the refusal came from other fetches holding the shared budget,
    /// so the same fetch may succeed on a later tick without any change.
    pub fn is_contention(&self) -> bool {
        matches!(
            self.limit,
            AdmissionLimit::GlobalRecords | AdmissionLimit::GlobalBytes
        )
    }
}

impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.limit {
            AdmissionLimit::AccountRecords => {
                "calendar has more events (plus deletions) in the sync window than max_events allows; raise [calendar] max_events (up to 10000) — the previous events were kept"
            }
            AdmissionLimit::AccountBytes => {
                "calendar events are larger in total than the max_events budget allows; raise [calendar] max_events (up to 10000) — the previous events were kept"
            }
            AdmissionLimit::EventBytes => "a calendar event exceeds the per-event size budget",
            AdmissionLimit::EventChildren => {
                "a calendar event has too many recurrence, reminder or extra values"
            }
            AdmissionLimit::LineBytes => "a calendar content line exceeds the size budget",
            AdmissionLimit::Nesting => "calendar components are nested too deeply",
            AdmissionLimit::DocumentBytes => "a calendar document exceeds the size budget",
            AdmissionLimit::Messages => "calendar plugin sent too many messages",
            AdmissionLimit::GlobalRecords | AdmissionLimit::GlobalBytes => {
                "calendar sync budget is in use by other accounts; will retry"
            }
            AdmissionLimit::Arithmetic => "calendar size computation overflowed",
        })
    }
}

impl std::error::Error for AdmissionError {}

/// `[calendar] max_events` outside `[MIN_MAX_EVENTS, MAX_MAX_EVENTS]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionConfigError {
    pub value: usize,
}

impl std::fmt::Display for AdmissionConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "must be between {MIN_MAX_EVENTS} and {MAX_MAX_EVENTS} (got {}); it is a safety cap and 0 does not mean unlimited",
            self.value
        )
    }
}

/// The typed per-account budget every backend is constructed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionBudget {
    max_events: NonZeroUsize,
}

impl Default for AdmissionBudget {
    fn default() -> Self {
        AdmissionBudget {
            max_events: NonZeroUsize::new(DEFAULT_MAX_EVENTS).unwrap_or(NonZeroUsize::MIN),
        }
    }
}

impl AdmissionBudget {
    /// A budget of exactly `max_events`, or an error outside the supported range.
    pub fn new(max_events: usize) -> Result<Self, AdmissionConfigError> {
        if !(MIN_MAX_EVENTS..=MAX_MAX_EVENTS).contains(&max_events) {
            return Err(AdmissionConfigError { value: max_events });
        }
        NonZeroUsize::new(max_events)
            .map(|max_events| AdmissionBudget { max_events })
            .ok_or(AdmissionConfigError { value: max_events })
    }

    /// The runtime reading of a possibly-invalid config value, with the
    /// diagnostic to surface when it was adjusted. A legacy `0` (which used to
    /// mean unlimited) becomes the default budget — never unlimited, but not a
    /// one-event calendar either; anything above the ceiling becomes the
    /// ceiling.
    pub fn clamped(max_events: usize) -> (Self, Option<AdmissionConfigError>) {
        match Self::new(max_events) {
            Ok(b) => (b, None),
            Err(e) => {
                let n = if max_events == 0 {
                    DEFAULT_MAX_EVENTS
                } else {
                    max_events.min(MAX_MAX_EVENTS)
                };
                let b = NonZeroUsize::new(n)
                    .map(|max_events| AdmissionBudget { max_events })
                    .unwrap_or_default();
                (b, Some(e))
            }
        }
    }

    pub fn max_events(&self) -> usize {
        self.max_events.get()
    }

    /// Retained bytes one fetch of this account may admit: scales with
    /// `max_events`, within `[MIN_ACCOUNT_BYTES, MAX_ACCOUNT_BYTES]`.
    pub fn account_bytes(&self) -> usize {
        self.max_events()
            .saturating_mul(ACCOUNT_BYTES_PER_EVENT)
            .clamp(MIN_ACCOUNT_BYTES, MAX_ACCOUNT_BYTES)
    }
}

/// The shared ceiling across every concurrent calendar fetch.
#[derive(Debug)]
pub struct AdmissionPool {
    max_records: usize,
    max_bytes: usize,
    records: AtomicUsize,
    bytes: AtomicUsize,
}

impl AdmissionPool {
    /// An isolated pool — tests, or a caller that wants its own ceiling.
    pub fn new(max_records: usize, max_bytes: usize) -> Arc<Self> {
        Arc::new(AdmissionPool {
            max_records,
            max_bytes,
            records: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        })
    }

    /// The process-wide pool every production router shares.
    pub fn global() -> Arc<Self> {
        static GLOBAL: OnceLock<Arc<AdmissionPool>> = OnceLock::new();
        GLOBAL
            .get_or_init(|| AdmissionPool::new(GLOBAL_MAX_RECORDS, GLOBAL_MAX_BYTES))
            .clone()
    }

    /// `(records, bytes)` currently reserved.
    pub fn in_use(&self) -> (usize, usize) {
        (
            self.records.load(Ordering::Acquire),
            self.bytes.load(Ordering::Acquire),
        )
    }

    fn try_add(counter: &AtomicUsize, max: usize, n: usize) -> bool {
        if n == 0 {
            return true;
        }
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                cur.checked_add(n).filter(|next| *next <= max)
            })
            .is_ok()
    }

    fn sub(counter: &AtomicUsize, n: usize) {
        if n == 0 {
            return;
        }
        // Saturating: a release can never wrap the shared counter even if a
        // caller double-released (which the lease type prevents).
        let mut cur = counter.load(Ordering::Acquire);
        loop {
            match counter.compare_exchange_weak(
                cur,
                cur.saturating_sub(n),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    fn try_reserve(&self, records: usize, bytes: usize) -> Result<(), AdmissionLimit> {
        if !Self::try_add(&self.records, self.max_records, records) {
            return Err(AdmissionLimit::GlobalRecords);
        }
        if !Self::try_add(&self.bytes, self.max_bytes, bytes) {
            Self::sub(&self.records, records);
            return Err(AdmissionLimit::GlobalBytes);
        }
        Ok(())
    }

    fn release(&self, records: usize, bytes: usize) {
        Self::sub(&self.records, records);
        Self::sub(&self.bytes, bytes);
    }
}

/// A reservation in an [`AdmissionPool`], released when dropped.
///
/// Not `Clone`: a copy would either double-release or leave data unaccounted.
#[derive(Debug, Default)]
pub struct AdmissionLease {
    pool: Option<Arc<AdmissionPool>>,
    records: usize,
    bytes: usize,
}

impl AdmissionLease {
    /// A lease that holds nothing (an empty or unchanged page).
    pub fn empty() -> Self {
        AdmissionLease::default()
    }

    fn with_pool(pool: Arc<AdmissionPool>) -> Self {
        AdmissionLease {
            pool: Some(pool),
            records: 0,
            bytes: 0,
        }
    }

    pub fn records(&self) -> usize {
        self.records
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Reserve more from the pool. A lease with no pool accepts nothing.
    pub fn reserve(&mut self, records: usize, bytes: usize) -> Result<(), AdmissionError> {
        let Some(pool) = self.pool.as_ref() else {
            if records == 0 && bytes == 0 {
                return Ok(());
            }
            return Err(AdmissionError::new(AdmissionLimit::GlobalBytes));
        };
        let next_records = self
            .records
            .checked_add(records)
            .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?;
        let next_bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?;
        pool.try_reserve(records, bytes)
            .map_err(AdmissionError::new)?;
        self.records = next_records;
        self.bytes = next_bytes;
        Ok(())
    }

    fn release(&mut self, records: usize, bytes: usize) {
        let records = records.min(self.records);
        let bytes = bytes.min(self.bytes);
        if let Some(pool) = self.pool.as_ref() {
            pool.release(records, bytes);
        }
        self.records -= records;
        self.bytes -= bytes;
    }
}

impl Drop for AdmissionLease {
    fn drop(&mut self) {
        let (records, bytes) = (self.records, self.bytes);
        self.release(records, bytes);
    }
}

/// Per-event charges accumulated while one event is being parsed.
#[derive(Debug, Default, Clone, Copy)]
struct EventCharge {
    bytes: usize,
    children: usize,
}

/// A position in an [`AdmissionMeter`]'s retained accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionCheckpoint {
    records: usize,
    retained: usize,
}

/// Accounting for one account's fetch.
///
/// Every `charge_*` / `admit_*` call checks, in order, the per-event, the
/// per-account and the global ceiling, and only then records the charge — so a
/// caller that charges *before* storing a value never stores one past a limit.
#[derive(Debug)]
pub struct AdmissionMeter {
    budget: AdmissionBudget,
    lease: AdmissionLease,
    records: usize,
    retained: usize,
    transient: usize,
    event: Option<EventCharge>,
}

impl AdmissionMeter {
    pub fn new(budget: AdmissionBudget, pool: Arc<AdmissionPool>) -> Self {
        AdmissionMeter {
            budget,
            lease: AdmissionLease::with_pool(pool),
            records: 0,
            retained: 0,
            transient: 0,
            event: None,
        }
    }

    /// A meter over a private pool at the global ceilings (fixtures, tests).
    pub fn isolated(budget: AdmissionBudget) -> Self {
        Self::new(
            budget,
            AdmissionPool::new(GLOBAL_MAX_RECORDS, GLOBAL_MAX_BYTES),
        )
    }

    pub fn budget(&self) -> AdmissionBudget {
        self.budget
    }

    /// Records (events + deletions) admitted so far.
    pub fn records(&self) -> usize {
        self.records
    }

    /// Retained bytes admitted so far, including an in-progress event.
    pub fn retained_bytes(&self) -> usize {
        self.retained
    }

    /// Transient (source document) bytes currently reserved.
    pub fn transient_bytes(&self) -> usize {
        self.transient
    }

    /// Reserve an in-flight source document (a response body, a file) in the
    /// global pool. Transient bytes do not count toward the account's retained
    /// budget and are released by [`Self::release_transient`] or
    /// [`Self::into_lease`].
    pub fn reserve_transient(&mut self, bytes: usize) -> Result<(), AdmissionError> {
        if bytes > MAX_SOURCE_DOCUMENT_BYTES {
            return Err(AdmissionError::new(AdmissionLimit::DocumentBytes));
        }
        self.lease.reserve(0, bytes)?;
        self.transient += bytes;
        Ok(())
    }

    /// Give back transient bytes (a document dropped, or a reservation shrunk
    /// to the body's real size).
    pub fn release_transient(&mut self, bytes: usize) {
        let bytes = bytes.min(self.transient);
        self.lease.release(0, bytes);
        self.transient -= bytes;
    }

    fn reserve_retained(&mut self, bytes: usize) -> Result<(), AdmissionError> {
        let next = self
            .retained
            .checked_add(bytes)
            .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?;
        if next > self.budget.account_bytes() {
            return Err(AdmissionError::new(AdmissionLimit::AccountBytes));
        }
        self.lease.reserve(0, bytes)?;
        self.retained = next;
        Ok(())
    }

    fn reserve_record(&mut self) -> Result<(), AdmissionError> {
        if self.records >= self.budget.max_events() {
            return Err(AdmissionError::new(AdmissionLimit::AccountRecords));
        }
        self.lease.reserve(1, 0)?;
        self.records += 1;
        Ok(())
    }

    /// Start accounting a new event, abandoning any unfinished one.
    pub fn begin_event(&mut self) {
        self.abandon_event();
        self.event = Some(EventCharge::default());
    }

    /// Charge retained payload to the in-progress event (and through it the
    /// account and the pool). Call **before** storing the value.
    pub fn charge_event(&mut self, bytes: usize, children: usize) -> Result<(), AdmissionError> {
        let cur = self.event.unwrap_or_default();
        let next = EventCharge {
            bytes: cur
                .bytes
                .checked_add(bytes)
                .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?,
            children: cur
                .children
                .checked_add(children)
                .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?,
        };
        if next.bytes > MAX_EVENT_BYTES {
            return Err(AdmissionError::new(AdmissionLimit::EventBytes));
        }
        if next.children > MAX_EVENT_CHILDREN {
            return Err(AdmissionError::new(AdmissionLimit::EventChildren));
        }
        self.reserve_retained(bytes)?;
        self.event = Some(next);
        Ok(())
    }

    /// Admit the in-progress event as a record. Call **before** building the
    /// [`CalEvent`] it becomes.
    pub fn admit_event(&mut self) -> Result<(), AdmissionError> {
        self.reserve_record()?;
        self.event = None;
        Ok(())
    }

    /// Drop the in-progress event's charges (it will not be retained).
    pub fn abandon_event(&mut self) {
        if let Some(ev) = self.event.take() {
            let bytes = ev.bytes.min(self.retained);
            self.lease.release(0, bytes);
            self.retained -= bytes;
        }
    }

    /// Admit a deletion (tombstone) whose identity is `id_len` bytes.
    pub fn admit_deletion(&mut self, id_len: usize) -> Result<(), AdmissionError> {
        let bytes = id_len
            .checked_add(ENTRY_OVERHEAD)
            .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?;
        self.reserve_record()?;
        self.reserve_retained(bytes)
    }

    /// Charge retained bytes that belong to the page rather than an event (a
    /// sync token, a router stamp).
    pub fn charge_retained(&mut self, bytes: usize) -> Result<(), AdmissionError> {
        self.reserve_retained(bytes)
    }

    /// Reserve the record for an event about to be decoded from an already
    /// bounded wire value (a plugin line). Pair with [`Self::charge_decoded`].
    pub fn reserve_decoded(&mut self) -> Result<(), AdmissionError> {
        self.reserve_record()
    }

    /// Charge a just-decoded event's retained footprint.
    pub fn charge_decoded(&mut self, e: &CalEvent) -> Result<(), AdmissionError> {
        let (bytes, children) = event_footprint(e)?;
        if bytes > MAX_EVENT_BYTES {
            return Err(AdmissionError::new(AdmissionLimit::EventBytes));
        }
        if children > MAX_EVENT_CHILDREN {
            return Err(AdmissionError::new(AdmissionLimit::EventChildren));
        }
        self.reserve_retained(bytes)
    }

    /// Admit an already-materialized event (record + footprint).
    pub fn admit_materialized(&mut self, e: &CalEvent) -> Result<(), AdmissionError> {
        self.reserve_decoded()?;
        self.charge_decoded(e)
    }

    /// Where the meter stands now, to [`Self::rollback`] to if a unit of input
    /// (one plugin message) turns out to be unusable after it was charged.
    pub fn checkpoint(&self) -> AdmissionCheckpoint {
        AdmissionCheckpoint {
            records: self.records,
            retained: self.retained,
        }
    }

    /// Release everything admitted since `cp`.
    pub fn rollback(&mut self, cp: AdmissionCheckpoint) {
        self.abandon_event();
        let records = self.records.saturating_sub(cp.records);
        let bytes = self.retained.saturating_sub(cp.retained);
        self.lease.release(records, bytes);
        self.records -= records;
        self.retained -= bytes;
    }

    /// Whether one more record fits the account budget — a cheap pre-check
    /// before decoding the next event.
    pub fn has_record_room(&self) -> bool {
        self.records < self.budget.max_events()
    }

    /// Finish the fetch: release transient reservations and any unfinished
    /// event, and hand the retained reservation to whoever keeps the data.
    pub fn into_lease(mut self) -> AdmissionLease {
        self.abandon_event();
        let t = self.transient;
        self.release_transient(t);
        std::mem::take(&mut self.lease)
    }
}

/// Conservative retained footprint of a materialized event: `(bytes, children)`.
///
/// Counts the struct twice (its slot plus vector growth slack), every string,
/// every zone name, recurrence rules with their `BY*` lists, RDATE/EXDATE
/// values, reminders and extra entries, each with [`ENTRY_OVERHEAD`].
pub fn event_footprint(e: &CalEvent) -> Result<(usize, usize), AdmissionError> {
    fn add(acc: &mut usize, n: usize) -> Result<(), AdmissionError> {
        *acc = acc
            .checked_add(n)
            .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?;
        Ok(())
    }
    fn time(t: &EventTime) -> usize {
        match t {
            EventTime::Zoned { zone, .. } => zone.as_str().len(),
            _ => 0,
        }
    }
    let mut bytes = 0usize;
    let mut children = 0usize;
    add(&mut bytes, 2 * std::mem::size_of::<CalEvent>())?;
    for s in [
        &e.uid,
        &e.title,
        &e.source.0,
        &e.description,
        &e.location,
        &e.url,
        &e.calendar,
        &e.category,
        &e.organizer,
        &e.etag,
    ] {
        add(&mut bytes, s.len())?;
    }
    add(&mut bytes, time(&e.start))?;
    add(&mut bytes, time(&e.end))?;
    for (k, v) in &e.extra {
        add(&mut bytes, k.len())?;
        add(&mut bytes, v.len())?;
        add(&mut bytes, ENTRY_OVERHEAD)?;
        add(&mut children, 1)?;
    }
    add(
        &mut bytes,
        e.reminders
            .len()
            .checked_mul(ENTRY_OVERHEAD)
            .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?,
    )?;
    add(&mut children, e.reminders.len())?;
    if let Some(r) = &e.recurrence {
        for rule in &r.rules {
            let parts = rule.by_second.len()
                + rule.by_minute.len()
                + rule.by_hour.len()
                + rule.by_day.len()
                + rule.by_month_day.len()
                + rule.by_year_day.len()
                + rule.by_week_no.len()
                + rule.by_month.len()
                + rule.by_set_pos.len();
            add(&mut bytes, std::mem::size_of::<super::RRule>())?;
            add(
                &mut bytes,
                parts
                    .checked_mul(8)
                    .ok_or(AdmissionError::new(AdmissionLimit::Arithmetic))?,
            )?;
            add(&mut children, 1)?;
            add(&mut children, parts)?;
        }
        for t in r.rdates.iter().chain(r.exdates.iter()) {
            add(&mut bytes, std::mem::size_of::<EventTime>())?;
            add(&mut bytes, time(t))?;
            add(&mut children, 1)?;
        }
    }
    Ok((bytes, children))
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
