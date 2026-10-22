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

use super::{
    AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage, PageDiagnostics,
};
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
            let mode =
                super::ics::snapshot_mode(events.is_empty(), !report.cancelled_uids().is_empty());
            let diagnostics = PageDiagnostics::from_report(&report);
            drop(body);
            EventPage::from_meter(
                meter,
                mode,
                events,
                Vec::new(),
                stored_cursor(self.zone.name(), &etag),
            )
            .map(|page| page.with_diagnostics(diagnostics))
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

#[cfg(test)]
mod tombstone_tests {
    use super::super::PageMode;
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;
    use thegn_core::config_calendar::{CalendarAccount, CalendarProviderKind};

    const TOMBSTONE_ONLY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:gone\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const EMPTY_CALENDAR: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";

    async fn feed(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(any(move |_request: Request| async move {
                    let mut response = (StatusCode::OK, Body::from(body)).into_response();
                    response.headers_mut().insert(
                        "content-type",
                        HeaderValue::from_static("text/calendar; charset=utf-8"),
                    );
                    response
                })),
            )
            .await
            .unwrap();
        });
        (format!("http://{address}/feed.ics"), task)
    }

    fn admission() -> AccountAdmission {
        AccountAdmission::isolated(Default::default(), thegn_core::calendar::Tz::UTC)
    }

    fn window() -> (NaiveDate, NaiveDate) {
        (
            NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
        )
    }

    async fn fetch(body: &'static str) -> EventPage {
        let (url, task) = feed(body).await;
        let account = CalendarAccount {
            name: "feed".into(),
            provider: CalendarProviderKind::IcsUrl,
            url,
            allow_private_network: true,
            ..CalendarAccount::default()
        };
        let backend = IcsUrlBackend::new(&account, admission());
        let (from, to) = window();
        let result = backend.list_events(from, to, "").await;
        task.abort();
        result.unwrap()
    }

    /// Regression: this assertion fails on main because the complete empty feed
    /// remains Snapshot and the host rejects it as a suspicious empty response.
    #[tokio::test]
    async fn tombstone_only_feed_is_a_reset() {
        let page = fetch(TOMBSTONE_ONLY).await;

        assert_eq!(page.mode(), PageMode::Reset);
        assert!(page.events().is_empty());
    }

    /// Positive control: an empty feed without effective tombstones stays guarded.
    #[tokio::test]
    async fn empty_feed_stays_a_snapshot() {
        let page = fetch(EMPTY_CALENDAR).await;

        assert_eq!(page.mode(), PageMode::Snapshot);
        assert!(page.events().is_empty());
    }
}
