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
use thegn_core::secretref::BareAs;

use super::{AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage, PageMode};
use crate::http::{
    CalendarHttpClient, CalendarHttpError, ExpectedMedia, MAX_BODY_BYTES, MAX_REQUEST_BYTES,
    bounded_header_value, discard_body, map_error, read_body, validate_encoding, validate_media,
};
use tokio::time::Instant;

// The transient reservation a remote fetch takes is the transport's body cap;
// the admission layer's document ceiling must cover it.
const _: () = assert!(MAX_BODY_BYTES <= thegn_core::calendar::admission::MAX_SOURCE_DOCUMENT_BYTES);

const ZONE_CURSOR_PREFIX: &str = "tgz1:";

pub(super) enum Credential {
    None,
    Token(String),
    Unresolved,
}

pub(super) fn resolve_credential(a: &CalendarAccount) -> Credential {
    match crate::secret::resolve_outcome(&a.token, BareAs::Literal, &format!("calendar:{}", a.name))
    {
        crate::secret::SecretResolution::Absent => Credential::None,
        crate::secret::SecretResolution::Resolved(value) => Credential::Token(value),
        crate::secret::SecretResolution::Failed => Credential::Unresolved,
    }
}

pub(super) fn request_etag<'a>(zone: &str, stored: &'a str) -> &'a str {
    let Some(rest) = stored.strip_prefix(ZONE_CURSOR_PREFIX) else {
        return "";
    };
    let Some((cursor_zone, etag)) = rest.split_once(':') else {
        return "";
    };
    if cursor_zone == zone { etag } else { "" }
}

pub(super) fn stored_cursor(zone: &str, etag: &str) -> String {
    if etag.is_empty() {
        String::new()
    } else {
        format!("{ZONE_CURSOR_PREFIX}{zone}:{etag}")
    }
}

fn request_etag_over_limit(zone: &str, stored: &str) -> bool {
    request_etag(zone, stored).len() > MAX_REQUEST_BYTES
}

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
    credential: Credential,
    zone: thegn_core::calendar::Tz,
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
            credential: resolve_credential(a),
            zone: admission.zone(),
            timeout: Duration::from_secs(a.timeout_secs.clamp(5, 120)),
            admission,
            http,
            init_error,
        }
    }
}

impl CalendarBackend for IcsUrlBackend {
    fn provider_id(&self) -> &'static str {
        "ics_url"
    }

    fn caps(&self) -> CalendarCaps {
        // No write support. The ETag round trip is the delta protocol and is
        // carried by the returned page, not by a capability bit.
        CalendarCaps::default()
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
            if matches!(&self.credential, Credential::Unresolved) {
                return Err(CalendarError::Auth(
                    "calendar token reference could not be resolved".into(),
                ));
            }
            let http = self.http.as_ref().expect("checked above");
            // Reserve the largest body the transport will accept before asking
            // for it, so concurrent fetches can't jointly exceed the shared
            // budget. Refusal is immediate and typed; nothing is sent.
            let mut meter = self.admission.meter();
            meter.reserve_transient(MAX_BODY_BYTES)?;
            let cursor_etag = request_etag(self.zone.name(), sync_token);
            if request_etag_over_limit(self.zone.name(), sync_token) {
                return Err(CalendarError::BodyLimit("calendar validator exceeds limit"));
            }
            let mut req = http.request(reqwest::Method::GET);
            if !cursor_etag.is_empty() {
                req = req.header(reqwest::header::IF_NONE_MATCH, cursor_etag);
            }
            if let Credential::Token(token) = &self.credential {
                if self.username.is_empty() {
                    req = req.bearer_auth(token);
                } else {
                    req = req.basic_auth(&self.username, Some(token));
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
                if cursor_etag.is_empty() {
                    return Err(CalendarError::Api(
                        "server returned 304 without a matching calendar cursor".into(),
                    ));
                }
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
            let zone = self.zone.name();
            // Admission happens while parsing: the first event or byte over
            // the budget refuses the whole fetch, the ETag is not advanced,
            // and the cached calendar stays as it was. A subscribed feed is a
            // whole document (often years of history), so only events that
            // can occur in the sync window count toward `max_events`.
            let mut events = Vec::new();
            let mut report = thegn_core::calendar::ics::IcsReport::default();
            thegn_core::calendar::ics::parse_ics_window_reported(
                &body,
                zone,
                Some((from, to)),
                &mut meter,
                &mut events,
                || Ok(()),
                &mut report,
            )?;
            if report.is_document_failure() {
                return Err(CalendarError::Parse(
                    "calendar document is incomplete or structurally malformed".into(),
                ));
            }
            drop(body);
            EventPage::from_meter(
                meter,
                PageMode::Snapshot,
                events,
                Vec::new(),
                stored_cursor(self.zone.name(), &etag),
            )
        })
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::{MAX_REQUEST_BYTES, request_etag, request_etag_over_limit, stored_cursor};

    #[test]
    fn ics_cursor_requires_the_current_zone_and_rejects_legacy_etags() {
        let stored = stored_cursor("Europe/Berlin", "\"rev-1\"");
        assert_eq!(request_etag("Europe/Berlin", &stored), "\"rev-1\"");
        assert_eq!(request_etag("Asia/Kolkata", &stored), "");
        assert_eq!(request_etag("Europe/Berlin", "\"legacy-etag\""), "");
        assert_eq!(stored_cursor("Europe/Berlin", ""), "");
    }

    #[test]
    fn wrapped_cursor_prefix_does_not_push_an_in_limit_etag_over_request_limit() {
        let etag = "x".repeat(MAX_REQUEST_BYTES);
        let stored = stored_cursor("Europe/Berlin", &etag);
        assert!(stored.len() > MAX_REQUEST_BYTES);
        assert!(!request_etag_over_limit("Europe/Berlin", &stored));
        assert!(request_etag_over_limit(
            "Europe/Berlin",
            &stored_cursor("Europe/Berlin", &("x".repeat(MAX_REQUEST_BYTES + 1)))
        ));
    }
}
