//! CalDAV (RFC 4791) collections.
//!
//! Two requests, both `REPORT`s against the collection URL:
//!
//! - `sync-collection` (RFC 6578) when we hold a sync token — the server sends
//!   only what changed, including tombstones for deleted events. This is the
//!   only provider here with *real* deltas rather than a conditional refetch.
//! - `calendar-query` otherwise — a time-bounded full fetch.
//!
//! The XML handling is deliberately narrow: rather than pulling in a DAV/XML
//! dependency, an in-tree bounded reader validates the DAV/CalDAV structure
//! and fails closed. This follows the owner decision to keep parsing
//! dependency-free.

use std::borrow::Cow;
use std::time::Duration;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::calendar::CalendarWindow;
use thegn_core::config_calendar::CalendarAccount;
use tokio::time::Instant;

use super::ics_url::{Credential, resolve_credential};
use super::{
    AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage, PageDiagnostics,
    PageMode,
};
use crate::http::{
    CalendarHttpClient, CalendarHttpError, ExpectedMedia, MAX_BODY_BYTES, MAX_REQUEST_BYTES,
    discard_body, map_error, read_body, validate_encoding, validate_media,
};
use thegn_core::calendar::AdmissionMeter;

#[path = "caldav_xml.rs"]
mod xml;

pub(super) fn map_transport_error(error: CalendarHttpError) -> CalendarError {
    match error {
        CalendarHttpError::Timeout => CalendarError::Timeout(map_error(error)),
        CalendarHttpError::BodyLimit => CalendarError::BodyLimit(map_error(error)),
        CalendarHttpError::DestinationRefused => CalendarError::Policy(map_error(error)),
        other => CalendarError::Network(map_error(other).into()),
    }
}

pub struct CalDavBackend {
    username: String,
    credential: Credential,
    zone: thegn_core::calendar::Tz,
    calendar_ids: Vec<String>,
    timeout: Duration,
    admission: AccountAdmission,
    http: Option<CalendarHttpClient>,
    init_error: Option<CalendarHttpError>,
}

impl CalDavBackend {
    pub(crate) fn new(a: &CalendarAccount, admission: AccountAdmission) -> Self {
        let configured = !a.url.trim().is_empty();
        let (http, init_error) = if !configured {
            (None, None)
        } else {
            match thegn_core::config_calendar::normalize_remote_calendar_url(&a.url, false) {
                Ok(url) => match CalendarHttpClient::new(&url, a.allow_private_network) {
                    Ok(client) => (Some(client), None),
                    Err(error) => (None, Some(error)),
                },
                Err(_) => (None, Some(CalendarHttpError::InvalidUrl)),
            }
        };
        CalDavBackend {
            username: a.username.clone(),
            credential: resolve_credential(a),
            zone: admission.zone(),
            calendar_ids: a
                .calendar_ids
                .iter()
                .map(|id| {
                    percent_decode(id.trim().trim_matches('/'))
                        .trim_matches('/')
                        .to_string()
                })
                .filter(|id| !id.is_empty())
                .collect(),
            timeout: Duration::from_secs(a.timeout_secs.clamp(5, 120)),
            admission,
            http,
            init_error,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout_for_test(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn stored_cursor_for_test(&self, server_token: &str) -> String {
        scope_token(&self.calendar_ids, self.zone.name(), server_token)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.credential {
            Credential::None | Credential::Unresolved => req,
            Credential::Token(token) if self.username.is_empty() => req.bearer_auth(token),
            Credential::Token(token) => {
                // The common case: Nextcloud/Radicale/Fastmail app passwords.
                req.basic_auth(&self.username, Some(token))
            }
        }
    }
}

/// A time-bounded `calendar-query` for VEVENTs. The exclusive end is the
/// next local midnight resolved in the home zone, already represented in UTC.
pub(super) fn calendar_query_body(window: CalendarWindow) -> String {
    let stamp = |ms| {
        chrono::DateTime::from_timestamp_millis(ms)
            .expect("CalendarWindow contains representable UTC milliseconds")
            .format("%Y%m%dT%H%M%SZ")
            .to_string()
    };
    format!(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT">
        <c:time-range start="{}" end="{}"/>
      </c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#,
        stamp(window.start_ms),
        stamp(window.end_exclusive_ms),
    )
}

const SYNC_COLLECTION_PREFIX: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<d:sync-collection xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:sync-token>"#;
const SYNC_COLLECTION_SUFFIX: &str = r#"</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
</d:sync-collection>"#;

/// A `sync-collection` report resuming from `token`.
///
/// The exact escaped size, including the XML envelope, is checked before any
/// request buffer is allocated.  Escaping is then performed once into that
/// pre-sized buffer; the old replace-chain temporarily held several large
/// intermediate strings before applying the limit.
fn sync_collection_body(token: &str) -> Result<String, CalendarError> {
    let escaped_len = xml_escaped_len(token).ok_or(CalendarError::BodyLimit(
        "calendar sync token exceeds limit",
    ))?;
    let size = SYNC_COLLECTION_PREFIX
        .len()
        .checked_add(escaped_len)
        .and_then(|size| size.checked_add(SYNC_COLLECTION_SUFFIX.len()))
        .ok_or(CalendarError::BodyLimit("calendar request exceeds limit"))?;
    if size > MAX_REQUEST_BYTES {
        return Err(CalendarError::BodyLimit("calendar request exceeds limit"));
    }
    let mut body = String::with_capacity(size);
    body.push_str(SYNC_COLLECTION_PREFIX);
    write_xml_escaped(&mut body, token);
    body.push_str(SYNC_COLLECTION_SUFFIX);
    debug_assert_eq!(body.len(), size);
    Ok(body)
}

fn xml_escaped_len(s: &str) -> Option<usize> {
    s.chars().try_fold(0usize, |size, ch| {
        let extra = match ch {
            '&' => 4,
            '<' | '>' => 3,
            '"' => 5,
            _ => 0,
        };
        size.checked_add(ch.len_utf8())?.checked_add(extra)
    })
}

fn write_xml_escaped(out: &mut String, s: &str) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
}

/// One `<response>` from a multistatus body.
#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DavResponse {
    pub href: String,
    /// The `calendar-data` payload, empty for a tombstone.
    pub ics: String,
    /// True when the response reports the resource as gone (404 status, or a
    /// `sync-collection` removal).
    pub deleted: bool,
}

#[cfg(test)]
pub(crate) fn parse_multistatus(xml: &str) -> Result<(Vec<DavResponse>, String), CalendarError> {
    let mut out = Vec::new();
    let token = self::xml::walk_multistatus(xml, |r| {
        out.push(DavResponse {
            href: r.href,
            ics: self::xml::decode_text(r.ics_raw)?.into_owned(),
            deleted: r.deleted,
        });
        Ok(())
    })?;
    Ok((out, token))
}

/// Parse a multistatus into a page, admitting each resource as it is reached.
fn page_from_multistatus(
    text: &str,
    zone: &str,
    calendar_ids: &[String],
    mode: PageMode,
    mut meter: AdmissionMeter,
) -> Result<EventPage, CalendarError> {
    debug_assert!(matches!(mode, PageMode::Snapshot | PageMode::Delta));
    let mut events = Vec::new();
    let mut deleted = Vec::new();
    let mut event_hrefs = Vec::new();
    let mut deleted_hrefs = Vec::new();
    let mut diagnostics = PageDiagnostics::default();
    let token = xml::walk_multistatus(text, |r| {
        // `calendar_ids` restricts to named collections; a resource outside
        // them is skipped before it is admitted or parsed. Tombstones are
        // filtered the same way, so a deletion elsewhere never reaches us.
        if !href_in_collections(&r.href, calendar_ids) {
            return Ok(());
        }
        if r.deleted {
            // A snapshot is the complete picture: what it omits is already
            // gone, so a stray 404 entry in a `calendar-query` carries nothing.
            if mode != PageMode::Delta {
                return Ok(());
            }
            // Keep a legacy UID fallback for rows cached before href storage.
            let id = uid_from_href(&r.href);
            meter.admit_deletion(id.len())?;
            deleted.push(id);
            deleted_hrefs.push(r.href);
            return Ok(());
        }
        // An entity-escaped resource needs an unescaped copy; reserve it as
        // transient before it exists.
        let copy = if r.ics_raw.contains('&')
            || r.ics_raw.contains("<![CDATA[")
            || r.ics_raw.contains("<!--")
            || r.ics_raw.contains("<?")
        {
            r.ics_raw.len()
        } else {
            0
        };
        meter.reserve_transient(copy)?;
        let ics = match xml::decode_text(r.ics_raw) {
            Ok(ics) => ics,
            Err(error) => {
                meter.release_transient(copy);
                return Err(error);
            }
        };
        let before = events.len();
        let mut report = thegn_core::calendar::ics::IcsReport::default();
        let parsed = thegn_core::calendar::ics::parse_ics_window_reported(
            &ics,
            zone,
            None,
            &mut meter,
            &mut events,
            || Ok(()),
            &mut report,
        );
        event_hrefs.extend(std::iter::repeat_n(r.href, events.len() - before));
        drop(ics);
        meter.release_transient(copy);
        parsed.map_err(CalendarError::from)?;
        if report.is_document_failure() {
            return Err(CalendarError::Parse(
                "calendar resource is incomplete or structurally malformed".into(),
            ));
        }
        diagnostics.absorb(PageDiagnostics::from_report(&report));
        Ok(())
    })?;
    let token = scope_token(calendar_ids, zone, &token);
    EventPage::from_meter_with_hrefs(
        meter,
        mode,
        events,
        deleted,
        event_hrefs,
        deleted_hrefs,
        token,
    )
    .map(|page| page.with_diagnostics(diagnostics))
}

const SCOPE_PREFIX: &str = "tgf2:";

/// A stable (FNV-1a) digest of the effective collection filter, independent of
/// order and duplicates. `DefaultHasher` is not stable across releases, and the
/// digest is persisted inside the cursor.
fn filter_digest(ids: &[String], zone: &str) -> u64 {
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for id in sorted {
        for b in id.bytes().chain(std::iter::once(0)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    for b in zone.bytes().chain(std::iter::once(0)) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Bind a server sync token to the collection filter and floating-time zone it
/// was collected under. An empty token stays empty (it means "no cursor").
pub(crate) fn scope_token(ids: &[String], zone: &str, token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    format!("{SCOPE_PREFIX}{:016x}:{token}", filter_digest(ids, zone))
}

/// The server token inside a stored cursor, or empty (forcing a full fetch)
/// when the cursor was minted under a different filter or predates scoping.
fn unscope_token<'a>(ids: &[String], zone: &str, stored: &'a str) -> &'a str {
    let Some(rest) = stored.strip_prefix(SCOPE_PREFIX) else {
        return "";
    };
    match rest.split_once(':') {
        Some((digest, token)) if digest == format!("{:016x}", filter_digest(ids, zone)) => token,
        _ => "",
    }
}

/// Decode `%XX` escapes (lossy on invalid UTF-8); malformed escapes pass
/// through literally.
fn percent_decode(s: &str) -> Cow<'_, str> {
    if !s.contains('%') {
        return Cow::Borrowed(s);
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (
                (b[i + 1] as char).to_digit(16),
                (b[i + 2] as char).to_digit(16),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

impl CalendarBackend for CalDavBackend {
    fn provider_id(&self) -> &'static str {
        "caldav"
    }

    fn caps(&self) -> CalendarCaps {
        // No write support yet. Incremental sync is not a capability: the page
        // itself carries the token and tombstones (`EventPage::deleted`).
        CalendarCaps::default()
    }

    fn list_events<'a>(
        &'a self,
        from: NaiveDate,
        to: NaiveDate,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        let Some(window) = CalendarWindow::new(from, to, self.zone) else {
            return Box::pin(async { Err(CalendarError::Api("invalid calendar window".into())) });
        };
        self.list_events_for_window(window, sync_token)
    }

    fn list_events_window<'a>(
        &'a self,
        window: &'a CalendarWindow,
        sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        self.list_events_for_window(*window, sync_token)
    }
}

impl CalDavBackend {
    fn list_events_for_window<'a>(
        &'a self,
        window: CalendarWindow,
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
            let deadline = Instant::now() + self.timeout;
            // Reserve the largest body the transport will accept before asking
            // for it; refusal is immediate and typed, and nothing is sent.
            let mut meter = self.admission.meter();
            meter.reserve_transient(MAX_BODY_BYTES)?;

            // A cursor minted under a different `calendar_ids` filter must not
            // resume: the server's delta would omit collections newly selected.
            let sync_token = unscope_token(&self.calendar_ids, self.zone.name(), sync_token);
            let incremental = !sync_token.is_empty();
            let body = if incremental {
                sync_collection_body(sync_token)?
            } else {
                calendar_query_body(window)
            };
            if body.len() > MAX_REQUEST_BYTES {
                return Err(CalendarError::BodyLimit("calendar request exceeds limit"));
            }
            let method = reqwest::Method::from_bytes(b"REPORT")
                .map_err(|_| CalendarError::Policy("calendar REPORT method unavailable"))?;
            let req = http
                .request(method)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/xml; charset=utf-8",
                )
                // Depth 1 = the collection's members, not the whole tree.
                .header("Depth", "1")
                .body(body);

            let resp = self.auth(req);
            let resp = http
                .send(resp, deadline)
                .await
                .map_err(map_transport_error)?;

            if resp.status().is_redirection() {
                return Err(CalendarError::Policy(map_error(
                    CalendarHttpError::Redirect,
                )));
            }
            validate_encoding(&resp).map_err(|error| CalendarError::Policy(map_error(error)))?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                || resp.status() == reqwest::StatusCode::FORBIDDEN
            {
                let status = resp.status();
                discard_body(resp, deadline)
                    .await
                    .map_err(map_transport_error)?;
                return Err(CalendarError::Auth(format!("HTTP {status}")));
            }
            // A server that has expired or never knew our token answers 409/507.
            // Falling back to a full fetch is the RFC 6578 recovery, and without it
            // the account would be stuck forever.
            if incremental
                && matches!(
                    resp.status(),
                    reqwest::StatusCode::CONFLICT | reqwest::StatusCode::INSUFFICIENT_STORAGE
                )
            {
                tracing::debug!(
                    target: "thegn::calendar",
                    status = %resp.status(),
                    "caldav sync token rejected — falling back to a full fetch"
                );
                discard_body(resp, deadline)
                    .await
                    .map_err(map_transport_error)?;
                return self.fetch_full(window, deadline, meter).await;
            }
            if !resp.status().is_success() && resp.status() != reqwest::StatusCode::MULTI_STATUS {
                let status = resp.status();
                discard_body(resp, deadline)
                    .await
                    .map_err(map_transport_error)?;
                return Err(CalendarError::Api(format!("HTTP {status}")));
            }

            validate_media(&resp, ExpectedMedia::Dav)
                .map_err(|error| CalendarError::Policy(map_error(error)))?;
            let text = read_body(resp, deadline)
                .await
                .map_err(|error| match error {
                    CalendarHttpError::BodyLimit => CalendarError::BodyLimit(map_error(error)),
                    CalendarHttpError::Timeout => CalendarError::Timeout(map_error(error)),
                    other => CalendarError::Network(map_error(other).into()),
                })?;
            meter.release_transient(MAX_BODY_BYTES - text.capacity().min(MAX_BODY_BYTES));
            let text = String::from_utf8(text)
                .map_err(|_| CalendarError::Parse("CalDAV response is not UTF-8".into()))?;
            match page_from_multistatus(
                &text,
                self.zone(),
                &self.calendar_ids,
                if incremental {
                    PageMode::Delta
                } else {
                    PageMode::Snapshot
                },
                meter,
            ) {
                // A delta over the account's own budget (a bulk delete, a
                // server re-stamping everything) would be refused again on
                // every tick, because the cursor is — correctly — not
                // advanced. The windowed full fetch is what the cache needs
                // anyway, and it may well fit: try it once, under a fresh
                // meter and the same absolute deadline.
                Err(CalendarError::Admission(a)) if incremental && a.is_account_limit() => {
                    drop(text);
                    tracing::debug!(
                        target: "thegn::calendar",
                        "caldav delta exceeds the admission budget — falling back to a full fetch"
                    );
                    let mut meter = self.admission.meter();
                    meter.reserve_transient(MAX_BODY_BYTES)?;
                    self.fetch_full(window, deadline, meter).await
                }
                other => other,
            }
        })
    }
}

impl CalDavBackend {
    fn zone(&self) -> &str {
        self.zone.name()
    }

    /// A 409/507 token recovery is exactly one additional REPORT, sharing the
    /// original absolute deadline and never recursively refreshing it. It
    /// reuses the first request's meter (and its body reservation).
    async fn fetch_full(
        &self,
        window: CalendarWindow,
        deadline: Instant,
        mut meter: AdmissionMeter,
    ) -> Result<EventPage, CalendarError> {
        let http = self.http.as_ref().expect("initialized backend");
        let body = calendar_query_body(window);
        let method = reqwest::Method::from_bytes(b"REPORT")
            .map_err(|_| CalendarError::Policy("calendar REPORT method unavailable"))?;
        let req = self.auth(
            http.request(method)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/xml; charset=utf-8",
                )
                .header("Depth", "1")
                .body(body),
        );
        let response = http
            .send(req, deadline)
            .await
            .map_err(map_transport_error)?;
        if response.status().is_redirection() {
            return Err(CalendarError::Policy(map_error(
                CalendarHttpError::Redirect,
            )));
        }
        validate_encoding(&response).map_err(|error| CalendarError::Policy(map_error(error)))?;
        if !response.status().is_success() && response.status() != reqwest::StatusCode::MULTI_STATUS
        {
            let status = response.status();
            discard_body(response, deadline)
                .await
                .map_err(map_transport_error)?;
            return Err(CalendarError::Api(format!("HTTP {status}")));
        }
        validate_media(&response, ExpectedMedia::Dav)
            .map_err(|error| CalendarError::Policy(map_error(error)))?;
        let bytes = read_body(response, deadline)
            .await
            .map_err(|error| match error {
                CalendarHttpError::BodyLimit => CalendarError::BodyLimit(map_error(error)),
                CalendarHttpError::Timeout => CalendarError::Timeout(map_error(error)),
                other => CalendarError::Network(map_error(other).into()),
            })?;
        meter.release_transient(MAX_BODY_BYTES - bytes.capacity().min(MAX_BODY_BYTES));
        let text = String::from_utf8(bytes)
            .map_err(|_| CalendarError::Parse("CalDAV response is not UTF-8".into()))?;
        // Whatever the original request was, this page is a `calendar-query`
        // snapshot: it is authoritative regardless of the token it returns.
        page_from_multistatus(
            &text,
            self.zone(),
            &self.calendar_ids,
            PageMode::Snapshot,
            meter,
        )
    }
}

/// Whether a resource href lives in one of `ids` (already trimmed of
/// slashes). An empty list means every collection. An id matches the
/// resource's parent collection when it equals that collection's path or is a
/// trailing run of its path segments, so both `work` and `/dav/cals/work/`
/// select `/dav/cals/work/e1.ics`. Fails closed: a non-empty filter never
/// admits a href with no parent collection.
pub(crate) fn href_in_collections(href: &str, ids: &[String]) -> bool {
    if ids.is_empty() {
        return true;
    }
    let path = match href.find("://") {
        Some(i) => href[i + 3..].find('/').map_or("", |j| &href[i + 3 + j..]),
        None => href,
    };
    let path = path.split(['?', '#']).next().unwrap_or(path);
    // Compare decoded: servers percent-encode hrefs while users type plain ids.
    let path = percent_decode(path);
    let Some((parent, _)) = path.trim_end_matches('/').rsplit_once('/') else {
        return false;
    };
    let parent = parent.trim_matches('/');
    if parent.is_empty() {
        return false;
    }
    ids.iter().any(|id| {
        let id = percent_decode(id);
        let id = id.as_ref();
        parent == id
            || parent
                .strip_suffix(id)
                .is_some_and(|rest| rest.ends_with('/'))
    })
}

/// The event uid a collection href refers to: the last path segment with its
/// `.ics` extension removed, which is the convention every CalDAV server uses.
pub(crate) fn uid_from_href(href: &str) -> String {
    href.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(href)
        .trim_end_matches(".ics")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_request_size_accounts_for_xml_expansion_and_envelope() {
        let envelope = SYNC_COLLECTION_PREFIX.len() + SYNC_COLLECTION_SUFFIX.len();
        let plain = "x".repeat(MAX_REQUEST_BYTES - envelope);
        assert!(sync_collection_body(&plain).is_ok());
        assert!(sync_collection_body(&format!("{plain}x")).is_err());

        let expanded = "&".repeat((MAX_REQUEST_BYTES - envelope) / 5 + 1);
        assert!(sync_collection_body(&expanded).is_err());
    }

    #[test]
    fn sync_request_escapes_once_into_the_bounded_buffer() {
        let body = sync_collection_body("a&<b\"").unwrap();
        assert!(body.contains("a&amp;&lt;b&quot;"));
        assert!(!body.contains("&amp;lt;"));
    }

    #[test]
    fn retained_sync_tokens_are_bounded_before_publication() {
        let oversized = "x".repeat(MAX_REQUEST_BYTES + 1);
        let xml = format!(
            "<multistatus xmlns=\"DAV:\"><sync-token>{oversized}</sync-token></multistatus>"
        );
        assert!(matches!(
            xml::walk_multistatus(&xml, |_| Ok(())),
            Err(CalendarError::BodyLimit(_))
        ));
    }

    fn multistatus() -> String {
        let one = |href: &str, uid: &str| {
            format!(
                "<d:response><d:href>{href}</d:href><d:propstat><d:prop><c:calendar-data>BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:{uid}\nSUMMARY:{uid}\nDTSTART:20300101T100000Z\nDTEND:20300101T110000Z\nEND:VEVENT\nEND:VCALENDAR</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
            )
        };
        format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">{}{}<d:response><d:href>/dav/personal/gone.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response><d:sync-token>t1</d:sync-token></d:multistatus>",
            one("/dav/work/a.ics", "work-a"),
            one("https://x.example/dav/personal/b.ics", "personal-b"),
        )
    }

    fn page(ids: &[&str]) -> EventPage {
        let ids: Vec<String> = ids.iter().map(|s| (*s).to_string()).collect();
        let meter =
            AccountAdmission::isolated(Default::default(), thegn_core::calendar::Tz::UTC).meter();
        page_from_multistatus(&multistatus(), "UTC", &ids, PageMode::Delta, meter).unwrap()
    }

    fn resource(href: &str, calendar_data: &str) -> String {
        format!(
            "<d:response><d:href>{href}</d:href><d:propstat><d:prop><c:calendar-data>{calendar_data}</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
        )
    }

    fn test_meter() -> AdmissionMeter {
        AccountAdmission::isolated(Default::default(), thegn_core::calendar::Tz::UTC).meter()
    }

    #[test]
    fn a_truncated_caldav_resource_fails_snapshot_and_delta_pages() {
        let good = resource(
            "/cal/good.ics",
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:good\nDTSTART:20300101T100000Z\nEND:VEVENT\nEND:VCALENDAR",
        );
        let truncated = resource(
            "/cal/cut.ics",
            "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:cut\nDTSTART:20300102T100000Z\n",
        );
        let xml = format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">{good}{truncated}</d:multistatus>"
        );

        for mode in [PageMode::Snapshot, PageMode::Delta] {
            let error = page_from_multistatus(&xml, "UTC", &[], mode, test_meter()).unwrap_err();
            assert!(matches!(error, CalendarError::Parse(_)), "{error:?}");
        }
    }

    #[test]
    fn balanced_envelope_less_caldav_resource_remains_accepted() {
        let data = "BEGIN:VEVENT\nUID:bare\nDTSTART:20300101T100000Z\nEND:VEVENT";
        let xml = format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">{}</d:multistatus>",
            resource("/cal/bare.ics", data)
        );
        let page =
            page_from_multistatus(&xml, "UTC", &[], PageMode::Snapshot, test_meter()).unwrap();
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].uid, "bare");
    }

    #[test]
    fn empty_calendar_ids_admit_every_collection() {
        let p = page(&[]);
        assert_eq!(p.events.len(), 2);
        assert_eq!(p.deleted, vec!["gone".to_string()]);
        assert_eq!(
            p.event_hrefs,
            vec!["/dav/work/a.ics", "https://x.example/dav/personal/b.ics"]
        );
        assert_eq!(p.events[0].uid, "work-a");
        assert_ne!(uid_from_href(&p.event_hrefs[0]), p.events[0].uid);
        assert_eq!(p.deleted_hrefs, vec!["/dav/personal/gone.ics"]);
    }

    #[test]
    fn calendar_ids_restrict_events_and_tombstones_to_named_collections() {
        let p = page(&["work"]);
        assert_eq!(p.events.len(), 1);
        assert!(p.events.iter().all(|e| e.title == "work-a"));
        assert!(p.deleted.is_empty());
        let p = page(&["personal"]);
        assert_eq!(p.events.len(), 1);
        assert_eq!(p.deleted, vec!["gone".to_string()]);
        assert_eq!(page(&["nope"]).events.len(), 0);
    }

    #[test]
    fn one_resource_preserves_href_for_every_vevent_component() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
          <d:response><d:href>/cal/random-server-key.ics</d:href><d:propstat><d:prop><c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:meeting-a@example
DTSTART:20300101T100000Z
DTEND:20300101T110000Z
END:VEVENT
BEGIN:VEVENT
UID:meeting-b@example
DTSTART:20300102T100000Z
DTEND:20300102T110000Z
END:VEVENT
END:VCALENDAR</c:calendar-data></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
        </d:multistatus>"#;
        let meter =
            AccountAdmission::isolated(Default::default(), thegn_core::calendar::Tz::UTC).meter();
        let page = page_from_multistatus(xml, "UTC", &[], PageMode::Snapshot, meter).unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.uid.as_str())
                .collect::<Vec<_>>(),
            ["meeting-a@example", "meeting-b@example"]
        );
        assert_eq!(
            page.event_hrefs,
            ["/cal/random-server-key.ics", "/cal/random-server-key.ics"]
        );
    }

    #[test]
    fn collection_matching_is_segment_aligned_and_fails_closed() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(href_in_collections("/dav/cals/work/e.ics", &ids(&["work"])));
        assert!(href_in_collections(
            "/dav/cals/work/e.ics",
            &ids(&["cals/work"])
        ));
        assert!(href_in_collections(
            "/dav/cals/work/e.ics",
            &ids(&["dav/cals/work"])
        ));
        assert!(!href_in_collections(
            "/dav/cals/homework/e.ics",
            &ids(&["work"])
        ));
        assert!(!href_in_collections("e.ics", &ids(&["work"])));
        assert!(!href_in_collections("/e.ics", &ids(&["work"])));
    }

    #[test]
    fn cursor_is_bound_to_the_calendar_filter() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let work = ids(&["work"]);
        let stored = scope_token(&work, "UTC", "srv-1");
        assert_eq!(unscope_token(&work, "UTC", &stored), "srv-1");
        // Order and duplicates do not matter.
        assert_eq!(
            unscope_token(
                &ids(&["b", "a"]),
                "UTC",
                &scope_token(&ids(&["a", "b", "a"]), "UTC", "t")
            ),
            "t"
        );
        // A changed filter, an unscoped legacy token, and garbage all force a
        // full fetch.
        assert_eq!(
            unscope_token(&ids(&["work", "personal"]), "UTC", &stored),
            ""
        );
        assert_eq!(unscope_token(&[], "UTC", &stored), "");
        assert_eq!(unscope_token(&work, "UTC", "srv-1"), "");
        assert_eq!(unscope_token(&work, "UTC", "tgf1:zz"), "");
        assert_eq!(
            unscope_token(&work, "UTC", "tgf1:0123456789abcdef:srv-1"),
            ""
        );
        assert_eq!(scope_token(&work, "UTC", ""), "");
        assert_eq!(unscope_token(&work, "Europe/Berlin", &stored), "");
    }

    #[test]
    fn page_token_carries_the_filter_scope() {
        let ids = vec!["work".to_string()];
        let meter =
            AccountAdmission::isolated(Default::default(), thegn_core::calendar::Tz::UTC).meter();
        let p = page_from_multistatus(&multistatus(), "UTC", &ids, PageMode::Delta, meter).unwrap();
        assert_eq!(unscope_token(&ids, "UTC", p.sync_token()), "t1");
        assert_eq!(
            unscope_token(&["personal".to_string()], "UTC", p.sync_token()),
            ""
        );
    }

    #[test]
    fn collection_matching_compares_percent_decoded() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(href_in_collections(
            "/dav/My%20Cal/e.ics",
            &ids(&["My Cal"])
        ));
        assert!(href_in_collections(
            "/dav/My Cal/e.ics",
            &ids(&["My%20Cal"])
        ));
        assert!(!href_in_collections("/dav/My%20Cal/e.ics", &ids(&["My"])));
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn a_forbidden_calendar_data_propstat_fails_the_delta_instead_of_a_tombstone() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><c:calendar-data/></d:prop><d:status>HTTP/1.1 403 Forbidden</d:status></d:propstat></d:response></d:multistatus>"#;
        assert!(matches!(
            page_from_multistatus(xml, "UTC", &[], PageMode::Delta, test_meter()),
            Err(CalendarError::Api(_))
        ));
    }

    #[test]
    fn an_empty_or_garbage_caldav_body_fails_the_page() {
        for body in [
            "",
            "not xml at all",
            "<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>/a.ics",
        ] {
            assert!(matches!(
                page_from_multistatus(body, "UTC", &[], PageMode::Snapshot, test_meter()),
                Err(CalendarError::Parse(_))
            ));
        }
    }

    #[test]
    fn a_foreign_namespace_response_is_rejected() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><x:response xmlns:x="urn:example"><x:href>/a.ics</x:href><x:propstat><x:prop><c:calendar-data>BEGIN:VCALENDAR
BEGIN:VEVENT
UID:foreign
DTSTART:20300101T100000Z
DTEND:20300101T110000Z
END:VEVENT
END:VCALENDAR</c:calendar-data></x:prop><x:status>HTTP/1.1 200 OK</x:status></x:propstat></x:response></d:multistatus>"#;
        assert!(matches!(
            page_from_multistatus(xml, "UTC", &[], PageMode::Snapshot, test_meter()),
            Err(CalendarError::Parse(_))
        ));
    }

    #[test]
    fn a_404_propstat_on_calendar_data_is_not_a_tombstone() {
        let xml = r#"<d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><c:calendar-data/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response></d:multistatus>"#;
        assert!(matches!(
            page_from_multistatus(xml, "UTC", &[], PageMode::Delta, test_meter()),
            Err(CalendarError::Api(_))
        ));
    }

    #[test]
    fn caldav_page_sums_diagnostics_across_resources() {
        let resource_data = |uid: &str| {
            format!(
                "BEGIN:VCALENDAR\nBEGIN:VEVENT\nUID:good-{uid}\nDTSTART:20300101T100000Z\nEND:VEVENT\nBEGIN:VEVENT\nUID:bad-{uid}\nEND:VEVENT\nEND:VCALENDAR"
            )
        };
        let first = resource("/cal/one.ics", &resource_data("one"));
        let second = resource("/cal/two.ics", &resource_data("two"));
        let xml = format!(
            "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">{first}{second}</d:multistatus>"
        );

        let page =
            page_from_multistatus(&xml, "UTC", &[], PageMode::Snapshot, test_meter()).unwrap();

        assert!(
            page.diagnostics().quarantined_events >= 2,
            "diagnostics: {:?}",
            page.diagnostics()
        );
    }
}
