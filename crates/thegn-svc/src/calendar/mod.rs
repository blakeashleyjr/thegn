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

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::calendar::CalEvent;
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
    Unsupported(&'static str),
    Io(String),
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
            CalendarError::Unsupported(op) => write!(f, "{op} is not supported by this provider"),
            CalendarError::Io(e) => write!(f, "{e}"),
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

/// One fetch's worth of events.
#[derive(Debug, Clone, Default)]
pub struct EventPage {
    pub events: Vec<CalEvent>,
    /// Ids removed since `sync_token` — only ever non-empty for an incremental
    /// fetch.
    pub deleted: Vec<String>,
    /// Opaque provider cursor (an ETag, a CalDAV sync-token). Empty means this
    /// was a full fetch and the cache should be replaced wholesale.
    pub sync_token: String,
    /// True when the provider had more than `max_events` to give.
    pub partial: bool,
    /// True when nothing changed since `sync_token` (an HTTP 304), so `events`
    /// is empty *and* the cache must be left alone.
    pub unchanged: bool,
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
pub(crate) fn backend_from_account(a: &CalendarAccount) -> Option<Box<dyn CalendarBackend>> {
    match a.provider {
        CalendarProviderKind::Ics => Some(Box::new(ics::IcsBackend::new(a))),
        CalendarProviderKind::IcsUrl => Some(Box::new(ics_url::IcsUrlBackend::new(a))),
        CalendarProviderKind::CalDav => Some(Box::new(caldav::CalDavBackend::new(a))),
        CalendarProviderKind::Command => Some(Box::new(command::CommandBackend::new(a))),
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
    pub fn from_config(cfg: &CalendarConfig) -> Self {
        let accounts = cfg
            .active_accounts()
            .into_iter()
            .filter_map(|a| {
                backend_from_account(&a).map(|inner| AccountBackend {
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

    /// Fetch every account, returning results **per account**.
    ///
    /// One source failing must never discard another's data, so nothing is
    /// merged here — the caller writes each account's cache independently.
    pub async fn list_events(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        tokens: &BTreeMap<String, String>,
    ) -> Vec<AccountResult> {
        let mut out = Vec::with_capacity(self.accounts.len());
        for a in &self.accounts {
            let token = tokens.get(&a.name).map(String::as_str).unwrap_or("");
            let mut result = a.inner.list_events(from, to, token).await;
            // Stamp identity onto every event so ids are globally unique and the
            // UI can color by source.
            if let Ok(page) = result.as_mut() {
                let source =
                    thegn_core::calendar::SourceId(format!("{}:{}", a.inner.provider_id(), a.name));
                for e in &mut page.events {
                    e.source = source.clone();
                    if e.color.is_none() {
                        e.color = a.hue;
                    }
                }
            }
            out.push(AccountResult {
                account: a.name.clone(),
                provider: a.inner.provider_id(),
                result,
            });
        }
        out
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
