//! Calendar event sources.
//!
//! Follows the house pattern (`control::ControlApi`): a dyn-compatible trait
//! whose async methods return [`BoxFuture`]s (not native `async fn`), whose
//! optional operations default to `Unsupported`, and a router built from
//! config that holds `Box<dyn CalendarBackend>` per account and returns
//! **per-account** results so one failing source can never clobber another's
//! cache.
//!
//! Everything here is read-only. The write methods exist so the shape is fixed
//! before anything depends on it — `EditScope` in particular cannot be
//! retrofitted later without breaking the plugin wire format — but every
//! built-in returns `Unsupported`.

pub mod caldav;
pub mod command;
pub mod ics;
pub mod ics_url;

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::calendar::{
    AdmissionBudget, AdmissionError, AdmissionLease, AdmissionMeter, AdmissionPool, CalEvent,
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
            CalendarError::Io(e) => write!(f, "{e}"),
            CalendarError::Admission(e) => write!(f, "{e}"),
        }
    }
}

impl CalendarError {
    /// Whether retrying later might work.
    ///
    /// Narrow on purpose. A **missing** `.ics` file is a configuration mistake,
    /// not a blip — reporting it as transient would both suppress the error and
    /// wrongly mark the whole app as offline.
    pub fn is_transient(&self) -> bool {
        matches!(self, CalendarError::Network(_))
    }
}

/// What a backend can do beyond listing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CalendarCaps {
    pub create: bool,
    pub update: bool,
    pub delete: bool,
    /// The provider expands recurrence itself, so the host must not.
    pub server_expand: bool,
    /// The provider supports conditional/delta fetches via `sync_token`.
    pub incremental: bool,
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

/// The admission budget and shared pool one account's fetches are metered
/// against. Required by every backend constructor.
#[derive(Debug, Clone)]
pub struct AccountAdmission {
    budget: AdmissionBudget,
    pool: Arc<AdmissionPool>,
}

impl AccountAdmission {
    pub fn new(budget: AdmissionBudget, pool: Arc<AdmissionPool>) -> Self {
        AccountAdmission { budget, pool }
    }

    /// `budget` against the process-wide pool every router shares.
    pub fn global(budget: AdmissionBudget) -> Self {
        Self::new(budget, AdmissionPool::global())
    }

    /// `budget` against a private pool at the global ceilings — for tests and
    /// callers that must not contend with live syncs.
    pub fn isolated(budget: AdmissionBudget) -> Self {
        Self::new(
            budget,
            AdmissionPool::new(
                thegn_core::calendar::admission::GLOBAL_MAX_RECORDS,
                thegn_core::calendar::admission::GLOBAL_MAX_BYTES,
            ),
        )
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

/// One fetch's worth of events — always complete.
///
/// A page is only ever built from a fetch that stayed within its admission
/// budget; an overflow is [`CalendarError::Admission`], never a truncated page.
/// So an empty `sync_token` really does mean "replace the cache wholesale" and
/// a cursor is only ever advanced past data that was fully admitted.
///
/// The page owns the [`AdmissionLease`] for what it holds: the global
/// reservation is released when the page is dropped, not when the fetch
/// returns. Fields are private and the page is not `Clone` so the data cannot
/// outlive or escape its accounting.
#[derive(Debug, Default)]
pub struct EventPage {
    events: Vec<CalEvent>,
    /// Ids removed since `sync_token` — only ever non-empty for an incremental
    /// fetch.
    deleted: Vec<String>,
    /// Opaque provider cursor (an ETag, a CalDAV sync-token). Empty means this
    /// was a full fetch and the cache should be replaced wholesale.
    sync_token: String,
    /// True when nothing changed since `sync_token` (an HTTP 304), so `events`
    /// is empty *and* the cache must be left alone.
    unchanged: bool,
    lease: AdmissionLease,
}

impl EventPage {
    /// Nothing changed since `sync_token`: keep the cache, keep the cursor.
    pub fn unchanged(sync_token: impl Into<String>) -> Self {
        EventPage {
            sync_token: sync_token.into(),
            unchanged: true,
            ..Default::default()
        }
    }

    /// Seal a fetch whose events and deletions were admitted through `meter`
    /// while they were parsed. Charges the cursor, then hands the meter's
    /// retained reservation to the page.
    pub fn from_meter(
        mut meter: AdmissionMeter,
        events: Vec<CalEvent>,
        deleted: Vec<String>,
        sync_token: String,
    ) -> Result<Self, CalendarError> {
        // Every event and deletion handed over must have been admitted through
        // this meter; a page is only as accounted as its lease.
        debug_assert!(
            meter.records() >= events.len() + deleted.len(),
            "EventPage::from_meter given unadmitted data"
        );
        meter.charge_retained(sync_token.len())?;
        Ok(EventPage {
            events,
            deleted,
            sync_token,
            unchanged: false,
            lease: meter.into_lease(),
        })
    }

    /// Admit already-materialized values (tests, in-memory sources). Refused
    /// whole when they exceed the budget.
    pub fn try_new(
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
        Self::from_meter(meter, events, deleted, sync_token.into())
    }

    pub fn events(&self) -> &[CalEvent] {
        &self.events
    }

    pub fn deleted(&self) -> &[String] {
        &self.deleted
    }

    pub fn sync_token(&self) -> &str {
        &self.sync_token
    }

    pub fn is_unchanged(&self) -> bool {
        self.unchanged
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

    /// Events overlapping `[from, to]`.
    ///
    /// A provider that cannot expand recurrence returns the masters with their
    /// `recurrence` intact and the host expands; one that can sets
    /// `caps().server_expand`.
    fn list_events<'a>(
        &'a self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>>;

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
pub(crate) fn backend_from_account(
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

/// One configured account plus its backend.
struct AccountBackend {
    name: String,
    hue: Option<thegn_core::theme::Hue>,
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
        Self::from_config_with_pool(cfg, AdmissionPool::global())
    }

    /// Every active account, metered against `pool`.
    pub fn from_config_with_pool(cfg: &CalendarConfig, pool: Arc<AdmissionPool>) -> Self {
        let admission = AccountAdmission::new(cfg.admission_budget(), pool);
        let accounts = cfg
            .active_accounts()
            .into_iter()
            .filter_map(|a| {
                backend_from_account(&a, admission.clone()).map(|inner| AccountBackend {
                    hue: a.hue(),
                    name: a.name.clone(),
                    inner,
                })
            })
            .collect();
        CalendarRouter { accounts }
    }

    pub fn is_configured(&self) -> bool {
        !self.accounts.is_empty()
    }

    /// Fetch every account, handing each result to `sink` as soon as it is
    /// ready.
    ///
    /// The sink owns the page, so a caller that applies and drops it there
    /// releases that account's admission lease before the next account is
    /// fetched — a router never holds more than one account's data, and an
    /// early account cannot starve a later one of the shared budget.
    pub async fn list_events_each(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        tokens: &BTreeMap<String, String>,
        mut sink: impl FnMut(AccountResult),
    ) {
        for a in &self.accounts {
            let token = tokens.get(&a.name).map(String::as_str).unwrap_or("");
            let mut result = a.inner.list_events(from, to, token).await;
            // Stamp identity onto every event so ids are globally unique and the
            // UI can color by source.
            if let Ok(page) = result.as_mut() {
                let source =
                    thegn_core::calendar::SourceId(format!("{}:{}", a.inner.provider_id(), a.name));
                if let Err(e) = page.stamp(&source, a.hue) {
                    result = Err(e);
                }
            }
            sink(AccountResult {
                account: a.name.clone(),
                provider: a.inner.provider_id(),
                result,
            });
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
        self.list_events_each(from, to, tokens, |r| out.push(r))
            .await;
        out
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
