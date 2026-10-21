//! Calendar event sources.
//!
//! Follows the house pattern (`control::ControlApi`): a dyn-compatible trait
//! whose async methods return [`BoxFuture`]s (not native `async fn`), whose
//! optional operations default to `Unsupported`, and a router built from
//! config that holds `Box<dyn CalendarBackend>` per account and returns
//! **per-account** results so one failing source can never clobber another's
//! cache.
//!
//! Built-in providers are currently read-only. The write methods exist so the
//! shape is fixed before anything depends on it — `EditScope` in particular
//! cannot be retrofitted later without breaking the plugin wire format — and
//! account policy is enforced before an adapter is allowed to handle them.

pub mod caldav;
pub mod command;
pub mod ics;
pub mod ics_url;

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use thegn_core::calendar::{
    AdmissionBudget, AdmissionError, AdmissionLease, AdmissionMeter, AdmissionPool, CalEvent,
    CalendarWindow,
};
use thegn_core::config_calendar::{CalendarAccount, CalendarConfig, CalendarProviderKind};

/// Why a fetch failed.
#[derive(Debug, Clone)]
pub enum CalendarError {
    NotConfigured,
    Network(String),
    Auth(String),
    Api(String),
    Subprocess(String),
    Parse(String),
    Policy(&'static str),
    BodyLimit(&'static str),
    Timeout(&'static str),
    Unsupported(&'static str),
    /// The account policy refused a mutation before the provider was called.
    ReadOnly(CalendarMutation),
    Io(String),
    /// The source exceeded its admission budget. Nothing from it is published:
    /// the account keeps its previous cache and cursor.
    Admission(AdmissionError),
}

impl From<AdmissionError> for CalendarError {
    fn from(e: AdmissionError) -> Self {
        CalendarError::Admission(e)
    }
}

impl std::fmt::Display for CalendarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalendarError::NotConfigured => write!(f, "not configured"),
            CalendarError::Network(e) => write!(f, "network error: {e}"),
            CalendarError::Auth(e) => write!(f, "authentication failed: {e}"),
            CalendarError::Api(e) => write!(f, "provider error: {e}"),
            CalendarError::Subprocess(e) => write!(f, "{e}"),
            CalendarError::Parse(e) => write!(f, "could not parse calendar: {e}"),
            CalendarError::Policy(e) => write!(f, "calendar transport policy: {e}"),
            CalendarError::BodyLimit(e) => write!(f, "calendar body limit: {e}"),
            CalendarError::Timeout(e) => write!(f, "calendar timeout: {e}"),
            CalendarError::Unsupported(op) => write!(f, "{op} is not supported by this provider"),
            CalendarError::ReadOnly(op) => {
                write!(f, "calendar account is read-only; {op} is not permitted")
            }
            CalendarError::Io(e) => write!(f, "{e}"),
            CalendarError::Admission(e) => write!(f, "{e}"),
        }
    }
}

/// The upstream mutation guarded by an account's `read_only` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarMutation {
    Create,
    Update,
    Delete,
}

impl CalendarMutation {
    fn as_str(self) -> &'static str {
        match self {
            CalendarMutation::Create => "create",
            CalendarMutation::Update => "update",
            CalendarMutation::Delete => "delete",
        }
    }
}

impl std::fmt::Display for CalendarMutation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl CalendarError {
    /// Whether retrying later might work.
    ///
    /// Retryable failures preserve the cache and may work on a later attempt.
    /// Connectivity evidence is classified separately by the transport that
    /// produced the result; a local command timeout is retryable too.
    pub fn is_transient(&self) -> bool {
        matches!(self, CalendarError::Network(_) | CalendarError::Timeout(_))
    }
}

/// What a backend can do beyond listing.
///
/// Only the mutation bits exist: they are masked by account `read_only` policy
/// and published through [`CalendarRouter::caps`]. `server_expand` and
/// `incremental` were removed because nothing consulted them (THE-466) — add a
/// field back only together with the production code that branches on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CalendarCaps {
    pub create: bool,
    pub update: bool,
    pub delete: bool,
}

/// Which instances of a recurring event an edit applies to.
///
/// Present from day one even though nothing writes yet: adding it later would
/// be a breaking change to the plugin wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditScope {
    ThisInstance,
    ThisAndFuture,
    AllInstances,
}

/// The immutable per-account context every backend is built from: the
/// admission budget and shared pool one account's fetches are metered against,
/// and the resolved home zone floating date-times are anchored to (THE-464).
/// Required by every backend constructor, so a production path cannot forget
/// the zone the way an optional builder setter could be forgotten.
#[derive(Debug, Clone)]
pub struct AccountAdmission {
    budget: AdmissionBudget,
    pool: Arc<AdmissionPool>,
    zone: thegn_core::calendar::Tz,
}

impl AccountAdmission {
    pub fn new(
        budget: AdmissionBudget,
        pool: Arc<AdmissionPool>,
        zone: thegn_core::calendar::Tz,
    ) -> Self {
        AccountAdmission { budget, pool, zone }
    }

    /// `budget` against the process-wide pool every router shares.
    pub fn global(budget: AdmissionBudget, zone: thegn_core::calendar::Tz) -> Self {
        Self::new(budget, AdmissionPool::global(), zone)
    }

    /// `budget` against a private pool at the global ceilings — for tests and
    /// callers that must not contend with live syncs.
    pub fn isolated(budget: AdmissionBudget, zone: thegn_core::calendar::Tz) -> Self {
        Self::new(
            budget,
            AdmissionPool::new(
                thegn_core::calendar::admission::GLOBAL_MAX_RECORDS,
                thegn_core::calendar::admission::GLOBAL_MAX_BYTES,
            ),
            zone,
        )
    }

    /// The home zone floating date-times are anchored to.
    pub fn zone(&self) -> thegn_core::calendar::Tz {
        self.zone
    }

    pub fn budget(&self) -> AdmissionBudget {
        self.budget
    }

    pub fn pool(&self) -> &Arc<AdmissionPool> {
        &self.pool
    }

    /// A fresh meter for one fetch.
    pub fn meter(&self) -> AdmissionMeter {
        AdmissionMeter::new(self.budget, self.pool.clone())
    }
}

/// What the fetch that produced an [`EventPage`] actually completed, and so how
/// the cache may apply it. This is the *only* thing the host branches on: the
/// returned cursor says where to resume, never what the page means (THE-452).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PageMode {
    /// An authoritative, complete picture of the window: anything cached for
    /// the account and absent from the page is gone. It may still return a
    /// next cursor (a full CalDAV `calendar-query` does).
    #[default]
    Snapshot,
    /// Only what changed since the request cursor: upserts plus tombstones,
    /// merged into the cache. Never destructive, whatever cursor comes back.
    Delta,
    /// Nothing changed since the request cursor; the cache is left alone.
    Unchanged,
    /// The provider asked for the account's cache to be discarded and replaced
    /// by this page, even when the page is empty (a snapshot that is believed
    /// unconditionally).
    Reset,
}

impl PageMode {
    /// Parse the plugin wire spelling.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "snapshot" => Some(PageMode::Snapshot),
            "delta" => Some(PageMode::Delta),
            "unchanged" => Some(PageMode::Unchanged),
            "reset" => Some(PageMode::Reset),
            _ => None,
        }
    }

    /// Whether the page replaces the account's cached events wholesale.
    pub fn replaces_cache(self) -> bool {
        matches!(self, PageMode::Snapshot | PageMode::Reset)
    }
}

/// One fetch's worth of events — always complete.
///
/// A page is only ever built from a fetch that stayed within its admission
/// budget; an overflow is [`CalendarError::Admission`], never a truncated page.
/// There is deliberately no "partial" [`PageMode`]: a truncated page cannot be
/// represented, so it can never be mistaken for an authoritative replacement.
/// A cursor is only ever advanced past data that was fully admitted.
///
/// The page owns the [`AdmissionLease`] for what it holds: the global
/// reservation is released when the page is dropped, not when the fetch
/// returns. Fields are private and the page is not `Clone` so the data cannot
/// outlive or escape its accounting.
#[derive(Debug, Default)]
pub struct EventPage {
    mode: PageMode,
    events: Vec<CalEvent>,
    /// Ids removed since the request cursor — only ever non-empty for a
    /// [`PageMode::Delta`].
    deleted: Vec<String>,
    /// Opaque resource hrefs parallel to events when supplied by the provider.
    event_hrefs: Vec<String>,
    /// Resource hrefs parallel to legacy UID tombstones.
    deleted_hrefs: Vec<String>,
    /// Opaque provider cursor to resume from next time (an ETag, a CalDAV
    /// sync-token). Empty means "no cursor": the next fetch is a full one.
    /// Independent of `mode`.
    sync_token: String,
    lease: AdmissionLease,
}

impl EventPage {
    /// Nothing changed since `sync_token`: keep the cache, keep the cursor.
    pub fn unchanged(sync_token: impl Into<String>) -> Self {
        EventPage {
            mode: PageMode::Unchanged,
            sync_token: sync_token.into(),
            ..Default::default()
        }
    }

    /// Seal a fetch whose events and deletions were admitted through `meter`
    /// while they were parsed. Charges the cursor, then hands the meter's
    /// retained reservation to the page.
    ///
    /// `mode` is what the fetch completed. Contradictions fail closed: an
    /// unchanged page carries no data (use [`Self::unchanged`]), and only a
    /// delta can carry tombstones.
    pub fn from_meter(
        meter: AdmissionMeter,
        mode: PageMode,
        events: Vec<CalEvent>,
        deleted: Vec<String>,
        sync_token: String,
    ) -> Result<Self, CalendarError> {
        Self::from_meter_with_hrefs(
            meter,
            mode,
            events,
            deleted,
            Vec::new(),
            Vec::new(),
            sync_token,
        )
    }

    /// CalDAV constructor retaining the provider resource key for every
    /// admitted event and tombstone.
    pub(crate) fn from_meter_with_hrefs(
        mut meter: AdmissionMeter,
        mode: PageMode,
        events: Vec<CalEvent>,
        deleted: Vec<String>,
        event_hrefs: Vec<String>,
        deleted_hrefs: Vec<String>,
        sync_token: String,
    ) -> Result<Self, CalendarError> {
        // Every event and deletion handed over must have been admitted through
        // this meter; a page is only as accounted as its lease.
        debug_assert!(
            meter.records() >= events.len() + deleted.len(),
            "EventPage::from_meter given unadmitted data"
        );
        if mode == PageMode::Unchanged {
            return Err(CalendarError::Parse(
                "an unchanged page cannot carry data".into(),
            ));
        }
        if mode != PageMode::Delta && !deleted.is_empty() {
            return Err(CalendarError::Parse(
                "only a delta page can carry deletions".into(),
            ));
        }
        if (!event_hrefs.is_empty() && event_hrefs.len() != events.len())
            || (!deleted_hrefs.is_empty() && deleted_hrefs.len() != deleted.len())
        {
            return Err(CalendarError::Parse(
                "resource hrefs do not match page rows".into(),
            ));
        }
        for href in event_hrefs.iter().chain(&deleted_hrefs) {
            meter.charge_retained(href.len())?;
        }
        meter.charge_retained(sync_token.len())?;
        Ok(EventPage {
            mode,
            events,
            deleted,
            event_hrefs,
            deleted_hrefs,
            sync_token,
            lease: meter.into_lease(),
        })
    }

    /// Admit already-materialized values (tests, in-memory sources). Refused
    /// whole when they exceed the budget.
    pub fn try_new(
        mode: PageMode,
        events: Vec<CalEvent>,
        deleted: Vec<String>,
        sync_token: impl Into<String>,
        admission: &AccountAdmission,
    ) -> Result<Self, CalendarError> {
        let mut meter = admission.meter();
        for e in &events {
            meter.admit_materialized(e)?;
        }
        for d in &deleted {
            meter.admit_deletion(d.len())?;
        }
        Self::from_meter(meter, mode, events, deleted, sync_token.into())
    }

    /// Admit an in-memory page while retaining provider resource identities.
    pub fn try_new_with_hrefs(
        mode: PageMode,
        events: Vec<CalEvent>,
        deleted: Vec<String>,
        event_hrefs: Vec<String>,
        deleted_hrefs: Vec<String>,
        sync_token: impl Into<String>,
        admission: &AccountAdmission,
    ) -> Result<Self, CalendarError> {
        let mut meter = admission.meter();
        for event in &events {
            meter.admit_materialized(event)?;
        }
        for deleted_id in &deleted {
            meter.admit_deletion(deleted_id.len())?;
        }
        Self::from_meter_with_hrefs(
            meter,
            mode,
            events,
            deleted,
            event_hrefs,
            deleted_hrefs,
            sync_token.into(),
        )
    }

    pub fn events(&self) -> &[CalEvent] {
        &self.events
    }

    pub fn deleted(&self) -> &[String] {
        &self.deleted
    }

    pub fn event_hrefs(&self) -> &[String] {
        &self.event_hrefs
    }

    pub fn deleted_hrefs(&self) -> &[String] {
        &self.deleted_hrefs
    }

    pub fn sync_token(&self) -> &str {
        &self.sync_token
    }

    /// What the producing fetch completed — see [`PageMode`].
    pub fn mode(&self) -> PageMode {
        self.mode
    }

    pub fn is_unchanged(&self) -> bool {
        self.mode == PageMode::Unchanged
    }

    /// `(records, bytes)` this page holds reserved in its pool.
    pub fn reserved(&self) -> (usize, usize) {
        (self.lease.records(), self.lease.bytes())
    }

    /// Extend this page's reservation to cover `bytes` of data derived from it
    /// while it is alive (the host's cache rows). Released with the page.
    pub fn reserve_derived(&mut self, bytes: usize) -> Result<(), CalendarError> {
        self.lease.reserve(0, bytes)?;
        Ok(())
    }

    /// Stamp the account identity onto every event, charging the copies first.
    fn stamp(
        &mut self,
        source: &thegn_core::calendar::SourceId,
        hue: Option<thegn_core::theme::Hue>,
    ) -> Result<(), CalendarError> {
        let bytes =
            source
                .as_str()
                .len()
                .checked_mul(self.events.len())
                .ok_or(AdmissionError::new(
                    thegn_core::calendar::AdmissionLimit::Arithmetic,
                ))?;
        self.lease.reserve(0, bytes)?;
        for e in &mut self.events {
            e.source = source.clone();
            if e.color.is_none() {
                e.color = hue;
            }
        }
        Ok(())
    }
}

/// A source of calendar events.
///
/// Methods return [`BoxFuture`]s (not native `async fn`) so the trait stays
/// dyn-compatible — the router holds a `Box<dyn CalendarBackend>` per account.
pub trait CalendarBackend: Send + Sync {
    fn provider_id(&self) -> &'static str;
    fn caps(&self) -> CalendarCaps;

    /// Events selected by the provider's date-oriented adapter. The router
    /// passes the canonical resolved range through `list_events_window`.
    ///
    /// A provider that cannot expand recurrence returns the masters with their
    /// `recurrence` intact and the host expands.
    fn list_events<'a>(
        &'a self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>>;

    /// Window-aware listing. Date-oriented providers inherit this adapter;
    /// providers that query instants (CalDAV) consume the canonical bounds.
    fn list_events_window<'a>(
        &'a self,
        window: &'a CalendarWindow,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        self.list_events(window.from, window.to, sync_token)
    }

    fn create_event<'a>(
        &'a self,
        _e: &'a CalEvent,
    ) -> BoxFuture<'a, Result<CalEvent, CalendarError>> {
        Box::pin(async { Err(CalendarError::Unsupported("creating events")) })
    }
    fn update_event<'a>(
        &'a self,
        _id: &'a str,
        _e: &'a CalEvent,
        _scope: EditScope,
    ) -> BoxFuture<'a, Result<CalEvent, CalendarError>> {
        Box::pin(async { Err(CalendarError::Unsupported("editing events")) })
    }
    fn delete_event<'a>(
        &'a self,
        _id: &'a str,
        _scope: EditScope,
    ) -> BoxFuture<'a, Result<(), CalendarError>> {
        Box::pin(async { Err(CalendarError::Unsupported("deleting events")) })
    }
}

/// The built-in backend for one configured account, or `None` for a
/// deactivated (`provider = "none"`) account.
fn raw_backend_from_account(
    a: &CalendarAccount,
    admission: AccountAdmission,
) -> Option<Box<dyn CalendarBackend>> {
    match a.provider {
        CalendarProviderKind::Ics => Some(Box::new(ics::IcsBackend::new(a, admission))),
        CalendarProviderKind::IcsUrl => Some(Box::new(ics_url::IcsUrlBackend::new(a, admission))),
        CalendarProviderKind::CalDav => Some(Box::new(caldav::CalDavBackend::new(a, admission))),
        CalendarProviderKind::Command => Some(Box::new(command::CommandBackend::new(a, admission))),
        CalendarProviderKind::None => None,
    }
}

/// Build the account-bound backend for one configured account.
///
/// The concrete constructors are deliberately crate-private. Callers that
/// have an account must come through this factory so the account policy wraps
/// every provider, including command/plugin adapters, before any mutation can
/// reach it.
pub fn backend_from_account(
    a: &CalendarAccount,
    admission: AccountAdmission,
) -> Option<Box<dyn CalendarBackend>> {
    raw_backend_from_account(a, admission)
        .map(|inner| Box::new(AccountPolicyBackend::new(a, inner)) as Box<dyn CalendarBackend>)
}

/// The single account-policy boundary for provider operations.
struct AccountPolicyBackend {
    account: String,
    read_only: bool,
    inner: Box<dyn CalendarBackend>,
}

impl AccountPolicyBackend {
    fn new(account: &CalendarAccount, inner: Box<dyn CalendarBackend>) -> Self {
        AccountPolicyBackend {
            account: account.name.clone(),
            read_only: account.read_only,
            inner,
        }
    }

    fn denied(&self, mutation: CalendarMutation) -> CalendarError {
        tracing::warn!(
            target: "thegn::calendar",
            account = %self.account,
            provider = self.inner.provider_id(),
            operation = mutation.as_str(),
            "calendar mutation refused by account read-only policy"
        );
        CalendarError::ReadOnly(mutation)
    }

    fn caps_for_policy(&self) -> CalendarCaps {
        let mut caps = self.inner.caps();
        if self.read_only {
            caps.create = false;
            caps.update = false;
            caps.delete = false;
        }
        caps
    }
}

impl CalendarBackend for AccountPolicyBackend {
    fn provider_id(&self) -> &'static str {
        self.inner.provider_id()
    }

    fn caps(&self) -> CalendarCaps {
        self.caps_for_policy()
    }

    fn list_events<'a>(
        &'a self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        self.inner.list_events(from, to, sync_token)
    }

    fn list_events_window<'a>(
        &'a self,
        window: &'a CalendarWindow,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        self.inner.list_events_window(window, sync_token)
    }

    fn create_event<'a>(
        &'a self,
        event: &'a CalEvent,
    ) -> BoxFuture<'a, Result<CalEvent, CalendarError>> {
        if self.read_only {
            return Box::pin(std::future::ready(Err(
                self.denied(CalendarMutation::Create)
            )));
        }
        self.inner.create_event(event)
    }

    fn update_event<'a>(
        &'a self,
        id: &'a str,
        event: &'a CalEvent,
        scope: EditScope,
    ) -> BoxFuture<'a, Result<CalEvent, CalendarError>> {
        if self.read_only {
            return Box::pin(std::future::ready(Err(
                self.denied(CalendarMutation::Update)
            )));
        }
        self.inner.update_event(id, event, scope)
    }

    fn delete_event<'a>(
        &'a self,
        id: &'a str,
        scope: EditScope,
    ) -> BoxFuture<'a, Result<(), CalendarError>> {
        if self.read_only {
            return Box::pin(std::future::ready(Err(
                self.denied(CalendarMutation::Delete)
            )));
        }
        self.inner.delete_event(id, scope)
    }
}

/// Workload class used to bound concurrent calendar fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchClass {
    Network,
    Command,
    Local,
}

// Network providers hold sockets and response bodies while fetching. Four
// allows useful overlap without multiplying the process-wide admission cap.
const NETWORK_FETCH_CONCURRENCY: usize = 4;
// A command provider owns a subprocess; serialize these to bound child count.
const COMMAND_FETCH_CONCURRENCY: usize = 1;
// Local ICS reads and parsing can hold file handles and page memory; serialize.
const LOCAL_FETCH_CONCURRENCY: usize = 1;

impl FetchClass {
    fn of(kind: CalendarProviderKind) -> Self {
        match kind {
            CalendarProviderKind::IcsUrl | CalendarProviderKind::CalDav => Self::Network,
            CalendarProviderKind::Command => Self::Command,
            CalendarProviderKind::Ics | CalendarProviderKind::None => Self::Local,
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Network => 0,
            Self::Command => 1,
            Self::Local => 2,
        }
    }

    fn limit(self) -> usize {
        match self {
            Self::Network => NETWORK_FETCH_CONCURRENCY,
            Self::Command => COMMAND_FETCH_CONCURRENCY,
            Self::Local => LOCAL_FETCH_CONCURRENCY,
        }
    }
}

/// One configured account plus its backend.
struct AccountBackend {
    name: String,
    hue: Option<thegn_core::theme::Hue>,
    class: FetchClass,
    inner: Box<dyn CalendarBackend>,
}

/// Every configured account, fetched together.
pub struct CalendarRouter {
    accounts: Vec<AccountBackend>,
}

/// One account's result, kept separate so a failure is scoped to its own cache.
pub struct AccountResult {
    pub account: String,
    pub provider: &'static str,
    pub result: Result<EventPage, CalendarError>,
}

impl CalendarRouter {
    /// Every active account, metered against the process-wide pool.
    pub fn from_config(cfg: &CalendarConfig) -> Self {
        Self::from_config_with_zone(cfg, cfg.effective_home_zone())
    }

    /// Every active account using the already-resolved home zone for this sync.
    pub fn from_config_with_zone(cfg: &CalendarConfig, zone: thegn_core::calendar::Tz) -> Self {
        Self::from_config_with_pool_and_zone(cfg, AdmissionPool::global(), zone)
    }

    /// Every active account, metered against `pool`.
    pub fn from_config_with_pool(cfg: &CalendarConfig, pool: Arc<AdmissionPool>) -> Self {
        Self::from_config_with_pool_and_zone(cfg, pool, cfg.effective_home_zone())
    }

    fn from_config_with_pool_and_zone(
        cfg: &CalendarConfig,
        pool: Arc<AdmissionPool>,
        zone: thegn_core::calendar::Tz,
    ) -> Self {
        let admission = AccountAdmission::new(cfg.admission_budget(), pool, zone);
        let accounts = cfg
            .active_accounts()
            .into_iter()
            .filter_map(|a| {
                backend_from_account(&a, admission.clone()).map(|inner| AccountBackend {
                    hue: a.hue(),
                    name: a.name.clone(),
                    class: FetchClass::of(a.provider),
                    inner,
                })
            })
            .collect();
        CalendarRouter { accounts }
    }

    #[cfg(test)]
    fn from_test_backends(accounts: Vec<(String, FetchClass, Box<dyn CalendarBackend>)>) -> Self {
        CalendarRouter {
            accounts: accounts
                .into_iter()
                .map(|(name, class, inner)| AccountBackend {
                    name,
                    hue: None,
                    class,
                    inner,
                })
                .collect(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.accounts.is_empty()
    }

    // The account-scoped mutation dispatch below. Nothing in production calls
    // these yet — no built-in backend can write, which is exactly the situation
    // THE-463 is about — so they are dead outside the policy tests that prove
    // the boundary denies correctly.
    //
    // They are deliberately NOT `pub`: exporting a calendar mutation API that
    // can only ever deny would recreate the untruthful-surface defect this
    // change removes. They are equally deliberately not deleted — the boundary
    // has to exist before a writable backend arrives, or that backend lands
    // with no single place to enforce account policy, and `EditScope` cannot be
    // retrofitted later without breaking the plugin wire format.
    //
    // Drop this allow when the first real mutation route calls through here.
    #[allow(dead_code)]
    /// Capabilities advertised by one account-bound provider.
    pub(crate) fn caps(&self, account: &str) -> Result<CalendarCaps, CalendarError> {
        self.account(account).map(|backend| backend.inner.caps())
    }

    /// Create an event through the account-bound policy boundary.
    // See the dead-code note on `caps` above.
    #[allow(dead_code)]
    pub(crate) async fn create_event(
        &self,
        account: &str,
        event: &CalEvent,
    ) -> Result<CalEvent, CalendarError> {
        self.account(account)?.inner.create_event(event).await
    }

    /// Update an event through the account-bound policy boundary.
    // See the dead-code note on `caps` above.
    #[allow(dead_code)]
    pub(crate) async fn update_event(
        &self,
        account: &str,
        id: &str,
        event: &CalEvent,
        scope: EditScope,
    ) -> Result<CalEvent, CalendarError> {
        self.account(account)?
            .inner
            .update_event(id, event, scope)
            .await
    }

    /// Delete an event through the account-bound policy boundary.
    // See the dead-code note on `caps` above.
    #[allow(dead_code)]
    pub(crate) async fn delete_event(
        &self,
        account: &str,
        id: &str,
        scope: EditScope,
    ) -> Result<(), CalendarError> {
        self.account(account)?.inner.delete_event(id, scope).await
    }

    fn account(&self, name: &str) -> Result<&AccountBackend, CalendarError> {
        let mut matches = self.accounts.iter().filter(|account| account.name == name);
        let account = matches.next().ok_or(CalendarError::NotConfigured)?;
        if matches.next().is_some() {
            // Never guess when malformed configuration names two providers
            // alike; a mutation must not fall through to the wrong account.
            return Err(CalendarError::NotConfigured);
        }
        Ok(account)
    }

    /// Fetch every account, handing each result to `sink` as soon as it is
    /// ready.
    ///
    /// Accounts run concurrently under per-class caps, so several pages can be
    /// alive at once (at most the sum of those caps). The shared admission pool
    /// still bounds their total; a pool refusal is the existing contention
    /// backoff. Results reach the sink in completion order.
    pub async fn list_events_each(
        &self,
        window: CalendarWindow,
        tokens: &BTreeMap<String, String>,
        mut sink: impl FnMut(AccountResult),
    ) {
        let mut queues: [VecDeque<&AccountBackend>; 3] = std::array::from_fn(|_| VecDeque::new());
        for account in &self.accounts {
            queues[account.class.index()].push_back(account);
        }
        let mut running = [0_usize; 3];
        let mut in_flight = FuturesUnordered::new();
        loop {
            for class in [FetchClass::Network, FetchClass::Command, FetchClass::Local] {
                let class_index = class.index();
                while running[class_index] < class.limit()
                    && let Some(account) = queues[class_index].pop_front()
                {
                    running[class_index] += 1;
                    let token = tokens.get(&account.name).map(String::as_str).unwrap_or("");
                    in_flight.push(async move { fetch_account(account, &window, token).await });
                }
            }
            let Some((class, result)) = in_flight.next().await else {
                break;
            };
            running[class.index()] -= 1;
            sink(result);
        }
    }

    /// Fetch every account, returning results **per account**.
    ///
    /// Test-only: every page's lease is held until the returned vector is
    /// dropped, so production uses [`Self::list_events_each`], which lets each
    /// result be applied and released before the next account is fetched.
    #[cfg(test)]
    pub(crate) async fn list_events(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        tokens: &BTreeMap<String, String>,
    ) -> Vec<AccountResult> {
        let mut out = Vec::with_capacity(self.accounts.len());
        let Some(window) = CalendarWindow::new(from, to, thegn_core::calendar::Tz::UTC) else {
            return out;
        };
        self.list_events_each(window, tokens, |r| out.push(r)).await;
        out
    }
}

async fn fetch_account(
    account: &AccountBackend,
    window: &CalendarWindow,
    token: &str,
) -> (FetchClass, AccountResult) {
    let mut result = account.inner.list_events_window(window, token).await;
    // Stamp identity onto every event so ids are globally unique and the UI
    // can color by source.
    if let Ok(page) = result.as_mut() {
        let source = thegn_core::calendar::SourceId(format!(
            "{}:{}",
            account.inner.provider_id(),
            account.name
        ));
        if let Err(error) = page.stamp(&source, account.hue) {
            result = Err(error);
        }
    }
    (
        account.class,
        AccountResult {
            account: account.name.clone(),
            provider: account.inner.provider_id(),
            result,
        },
    )
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
