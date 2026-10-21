//! The calendar cache seam: events and per-account sync bookkeeping.
//!
//! A **pure cache** — the provider is the source of truth, so these tables are
//! always safe to drop and let the next sync repopulate.

use anyhow::Result;

/// One cached event row.
///
/// `start_ms`/`end_ms` are denormalized *only* so a range query can prefilter;
/// the authoritative expansion is [`crate::calendar::recur`] over `json`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CalendarRow {
    pub uid: String,
    /// Normalized recurrence instance key; empty for a master or one-shot.
    pub recurrence_key: String,
    pub calendar: String,
    pub start_ms: i64,
    pub end_ms: i64,
    /// True for a recurrence master. Such rows are always loaded regardless of
    /// the query window, because a master far outside it can still produce
    /// occurrences inside it.
    pub recurring: bool,
    /// The serde-JSON `CalEvent`.
    pub json: String,
    /// Opaque provider resource key (CalDAV href). `None` for providers or
    /// legacy rows without a resource identity.
    pub href: Option<String>,
}

/// Per-account sync bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CalendarSyncRow {
    pub account: String,
    pub provider: String,
    /// The provider's opaque cursor (an ETag, a CalDAV sync-token). Empty means
    /// the last fetch was a full one.
    pub sync_token: String,
    /// When this account was last *attempted*, success or failure — which is
    /// what a freshness guard wants, so a persistently broken provider is
    /// retried on the normal cadence instead of on every popup open.
    pub fetched_at: i64,
    /// The last error, or empty. Kept so a persistently broken source can be
    /// surfaced rather than silently showing stale data forever.
    pub last_error: String,
    /// Resolved UTC milliseconds for the last fetched `[start, end)` horizon.
    pub horizon_from_ms: i64,
    pub horizon_to_ms: i64,
}

/// Cursor and fetched horizon committed with a calendar page.
pub struct CalendarCursor<'a> {
    /// Provider identifier for the cached account.
    pub provider: &'a str,
    /// Opaque provider cursor to persist.
    pub sync_token: &'a str,
    /// Start of the fetched horizon in UTC milliseconds.
    pub horizon_from_ms: i64,
    /// Exclusive end of the fetched horizon in UTC milliseconds.
    pub horizon_to_ms: i64,
}

/// Policy for an empty authoritative replacement.
pub enum EmptySnapshot {
    /// Keep a populated cache and record the suspicious empty response.
    KeepCachedEvents,
    /// The provider explicitly reset the collection; accept the empty page.
    Believe,
}

/// Tombstones in an incremental page.
pub enum CalendarTombstones<'a> {
    /// Delete every recurrence row for each uid.
    Uids(&'a [String]),
    /// Delete by resource href, falling back to legacy uid rows with NULL href.
    Resources(&'a [(String, String)]),
}

impl CalendarTombstones<'_> {
    /// Whether this page carries no deletion tombstones.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Uids(uids) => uids.is_empty(),
            Self::Resources(resources) => resources.is_empty(),
        }
    }
}

/// One complete provider page and its cache mutation semantics.
pub enum CalendarPageWrite<'a> {
    /// A 304-style response; only cursor freshness is updated.
    Unchanged,
    /// An authoritative full replacement.
    Replace {
        /// Complete rows returned by the provider.
        rows: &'a [CalendarRow],
        /// Whether an empty replacement is suspicious or authoritative.
        empty: EmptySnapshot,
    },
    /// An incremental set of upserts and deletions.
    Delta {
        /// Rows added or updated by the provider.
        rows: &'a [CalendarRow],
        /// Rows removed by the provider.
        tombstones: CalendarTombstones<'a>,
    },
}

/// Outcome of applying one page atomically.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarPageApplied {
    /// Only sync freshness/cursor state changed.
    FreshnessOnly,
    /// The complete page was applied.
    Applied { events_changed: bool },
    /// A suspicious empty replacement was refused to preserve cached events.
    RejectedEmpty,
}

/// Cached calendar events and their sync state.
pub trait CalendarStore {
    /// Events overlapping `[from_ms, to_ms)`, plus every recurrence master.
    ///
    /// Returns `(account, json)` pairs. `accounts` filters; empty means all.
    fn get_calendar_events(
        &self,
        from_ms: i64,
        to_ms: i64,
        accounts: &[String],
    ) -> Result<Vec<(String, String)>>;

    /// Upsert rows for one account (the incremental path), replacing every
    /// cached row of each resource the batch carries.
    fn put_calendar_events(&self, account: &str, rows: &[CalendarRow]) -> Result<()>;

    /// Delete specific uids (tombstones from an incremental sync).
    fn delete_calendar_events(&self, account: &str, uids: &[String]) -> Result<()>;

    /// Delete every cached component represented by each provider resource.
    /// `legacy_uid` is used only for rows whose href is NULL.
    fn delete_calendar_events_by_href(
        &self,
        account: &str,
        resources: &[(String, String)],
    ) -> Result<()>;

    /// Replace an account's whole cache in ONE transaction (the full-fetch path).
    ///
    /// Atomic on purpose: a crash mid-write must not leave a half-emptied
    /// calendar, and 2000 individual commits would fight the compositor for the
    /// write lock.
    fn replace_calendar_account(&self, account: &str, rows: &[CalendarRow]) -> Result<()>;

    fn get_calendar_sync(&self, account: &str) -> Result<Option<CalendarSyncRow>>;

    fn put_calendar_sync(
        &self,
        account: &str,
        provider: &str,
        sync_token: &str,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<()>;

    /// Record a failure. Advances the attempt stamp (so the retry is throttled
    /// like any other fetch) but leaves the cached events and the resume cursor
    /// untouched.
    fn set_calendar_error(&self, account: &str, err: &str) -> Result<()>;

    /// Whether this account has anything cached — the guard that stops an empty
    /// fetch from erasing a good calendar.
    fn has_calendar_events(&self, account: &str) -> Result<bool>;

    /// Apply page mutations and cursor/horizon state in one all-or-nothing
    /// transaction. On any storage error neither the events nor cursor advance.
    fn apply_calendar_page(
        &self,
        account: &str,
        write: &CalendarPageWrite<'_>,
        cursor: &CalendarCursor<'_>,
    ) -> Result<CalendarPageApplied>;

    /// Drop non-recurring events that ended before `before_ms`. Growth bound.
    fn prune_calendar_events(&self, before_ms: i64) -> Result<usize>;
}
