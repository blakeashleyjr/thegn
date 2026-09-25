//! Subscribed `.ics` / `webcal://` URLs.
//!
//! This is the backend that covers "sync with my calendar provider" for most
//! people with no OAuth at all: Google, Outlook, Fastmail, Nextcloud and Proton
//! all publish a secret ICS URL.
//!
//! The `ETag` **is** the incremental story. A conditional GET that comes back
//! `304 Not Modified` costs one round trip and no parsing, which is what makes
//! a 15-minute poll cheap enough to be the default.

use std::time::Duration;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::config_calendar::CalendarAccount;

use super::{AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage};
use crate::http::{
    CalendarHttpClient, CalendarHttpError, ExpectedMedia, MAX_BODY_BYTES, MAX_REQUEST_BYTES,
    bounded_header_value, discard_body, map_error, read_body, validate_encoding, validate_media,
};
use tokio::time::Instant;

// The transient reservation a remote fetch takes is the transport's body cap;
// the admission layer's document ceiling must cover it.
const _: () = assert!(MAX_BODY_BYTES <= thegn_core::calendar::admission::MAX_SOURCE_DOCUMENT_BYTES);

pub(super) fn map_transport_error(error: CalendarHttpError) -> CalendarError {
    match error {
        CalendarHttpError::Timeout => CalendarError::Timeout(map_error(error)),
        CalendarHttpError::BodyLimit => CalendarError::BodyLimit(map_error(error)),
        CalendarHttpError::DestinationRefused => CalendarError::Policy(map_error(error)),
        other => CalendarError::Network(map_error(other).into()),
    }
}

pub struct IcsUrlBackend {
    username: String,
    token: String,
    zone: String,
    timeout: Duration,
    admission: AccountAdmission,
    http: Option<CalendarHttpClient>,
    init_error: Option<CalendarHttpError>,
}

impl IcsUrlBackend {
    pub(crate) fn new(a: &CalendarAccount, admission: AccountAdmission) -> Self {
        let configured = !a.url.trim().is_empty();
        let (http, init_error) = if !configured {
            (None, None)
        } else {
            match thegn_core::config_calendar::normalize_remote_calendar_url(&a.url, true) {
                Ok(url) => match CalendarHttpClient::new(&url, a.allow_private_network) {
                    Ok(client) => (Some(client), None),
                    Err(error) => (None, Some(error)),
                },
                Err(_) => (None, Some(CalendarHttpError::InvalidUrl)),
            }
        };
        IcsUrlBackend {
            username: a.username.clone(),
            token: thegn_core::config::expand_env_ref(&a.token).unwrap_or_default(),
            zone: String::new(),
            timeout: Duration::from_secs(a.timeout_secs.clamp(5, 120)),
            admission,
            http,
            init_error,
        }
    }

    pub fn with_zone(mut self, zone: &str) -> Self {
        self.zone = zone.to_string();
        self
    }
}

impl CalendarBackend for IcsUrlBackend {
    fn provider_id(&self) -> &'static str {
        "ics_url"
    }

    fn caps(&self) -> CalendarCaps {
        CalendarCaps {
            // The ETag round trip is the delta protocol.
            incremental: true,
            ..Default::default()
        }
    }

    fn list_events<'a>(
        &'a self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        Box::pin(async move {
            if self.http.is_none() && self.init_error.is_none() {
                return Err(CalendarError::NotConfigured);
            }
            if let Some(error) = self.init_error {
                return Err(CalendarError::Policy(map_error(error)));
            }
            if sync_token.len() > MAX_REQUEST_BYTES {
                return Err(CalendarError::BodyLimit("calendar validator exceeds limit"));
            }
            let http = self.http.as_ref().expect("checked above");
            // Reserve the largest body the transport will accept before asking
            // for it, so concurrent fetches can't jointly exceed the shared
            // budget. Refusal is immediate and typed; nothing is sent.
            let mut meter = self.admission.meter();
            meter.reserve_transient(MAX_BODY_BYTES)?;
            let mut req = http.request(reqwest::Method::GET);
            if !sync_token.is_empty() {
                req = req.header(reqwest::header::IF_NONE_MATCH, sync_token);
            }
            if !self.token.is_empty() {
                if self.username.is_empty() {
                    req = req.bearer_auth(&self.token);
                } else {
                    req = req.basic_auth(&self.username, Some(&self.token));
                }
            }
            let deadline = Instant::now() + self.timeout;
            let resp = http
                .send(req, deadline)
                .await
                .map_err(map_transport_error)?;

            // 304 is the one protocol-level 3xx compatibility response: it is
            // not a redirect and preserves the existing ETag cache behavior.
            // Every other 3xx is refused before Location is inspected.
            if resp.status().is_redirection() && resp.status() != reqwest::StatusCode::NOT_MODIFIED
            {
                return Err(CalendarError::Policy(map_error(
                    CalendarHttpError::Redirect,
                )));
            }
            validate_encoding(&resp).map_err(|error| CalendarError::Policy(map_error(error)))?;

            if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
                // Nothing changed. Return the SAME token and `unchanged`, so the
                // caller leaves the cache exactly as it is rather than reading an
                // empty page as "the calendar was emptied".
                return Ok(EventPage::unchanged(sync_token));
            }
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                || resp.status() == reqwest::StatusCode::FORBIDDEN
            {
                let status = resp.status();
                discard_body(resp, deadline)
                    .await
                    .map_err(map_transport_error)?;
                return Err(CalendarError::Auth(format!("HTTP {status}")));
            }
            if !resp.status().is_success() {
                let status = resp.status();
                discard_body(resp, deadline)
                    .await
                    .map_err(map_transport_error)?;
                return Err(CalendarError::Api(format!("HTTP {status}")));
            }
            validate_media(&resp, ExpectedMedia::Ics)
                .map_err(|error| CalendarError::Policy(map_error(error)))?;
            let etag = bounded_header_value(&resp, &reqwest::header::ETAG)
                .map_err(|error| CalendarError::BodyLimit(map_error(error)))?
                .unwrap_or_default();
            let body = read_body(resp, deadline)
                .await
                .map_err(|error| match error {
                    CalendarHttpError::BodyLimit => CalendarError::BodyLimit(map_error(error)),
                    CalendarHttpError::Timeout => CalendarError::Timeout(map_error(error)),
                    other => CalendarError::Network(map_error(other).into()),
                })?;
            // Shrink the reservation to the buffer actually allocated.
            meter.release_transient(MAX_BODY_BYTES - body.capacity().min(MAX_BODY_BYTES));
            let body = String::from_utf8(body)
                .map_err(|_| CalendarError::Parse("calendar response is not UTF-8".into()))?;
            let zone = if self.zone.is_empty() {
                "UTC"
            } else {
                &self.zone
            };
            // Admission happens while parsing: the first event or byte over
            // the budget refuses the whole fetch, the ETag is not advanced,
            // and the cached calendar stays as it was. A subscribed feed is a
            // whole document (often years of history), so only events that
            // can occur in the sync window count toward `max_events`.
            let mut events = Vec::new();
            thegn_core::calendar::parse_ics_window(
                &body,
                zone,
                Some((from, to)),
                &mut meter,
                &mut events,
            )?;
            drop(body);
            EventPage::from_meter(meter, events, Vec::new(), etag)
        })
    }
}
