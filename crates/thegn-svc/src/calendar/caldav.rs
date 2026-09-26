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
//! stack we extract the handful of elements the two reports actually define.
//! CalDAV bodies are machine-generated and shallow, and the alternative is a
//! large dependency for a couple of tag lookups.

use std::borrow::Cow;
use std::time::Duration;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::config_calendar::CalendarAccount;
use tokio::time::Instant;

use super::{AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage};
use crate::http::{
    CalendarHttpClient, CalendarHttpError, ExpectedMedia, MAX_BODY_BYTES, MAX_REQUEST_BYTES,
    discard_body, map_error, read_body, validate_encoding, validate_media,
};
use thegn_core::calendar::AdmissionMeter;

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
    token: String,
    zone: String,
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

    #[cfg(test)]
    pub(crate) fn with_timeout_for_test(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if self.token.is_empty() {
            return req;
        }
        if self.username.is_empty() {
            req.bearer_auth(&self.token)
        } else {
            // The common case: Nextcloud/Radicale/Fastmail app passwords.
            req.basic_auth(&self.username, Some(&self.token))
        }
    }
}

/// A time-bounded `calendar-query` for VEVENTs.
fn calendar_query_body(from: NaiveDate, to: NaiveDate) -> String {
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
        from.format("%Y%m%dT000000Z"),
        to.format("%Y%m%dT235959Z"),
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

/// Undo the five predefined XML entities. Borrowed when there are none, which
/// is the common case for calendar data.
fn xml_unescape(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    // One pass into one buffer no larger than the input (every entity is
    // longer than its character), so an unescaped copy peaks at 1× — the
    // replace chain it supersedes held two full-size strings at once. Each
    // entity is decoded exactly once, so `&amp;lt;` stays `&lt;`.
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let (ch, len) = [
            ("&lt;", '<'),
            ("&gt;", '>'),
            ("&quot;", '"'),
            ("&apos;", '\''),
            ("&amp;", '&'),
        ]
        .iter()
        .find(|(e, _)| rest.starts_with(e))
        .map_or(('&', 1), |(e, c)| (*c, e.len()));
        out.push(ch);
        rest = &rest[len..];
    }
    out.push_str(rest);
    Cow::Owned(out)
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

/// One `<response>`, borrowed from the body: `ics` is still XML-escaped so the
/// caller can account for an unescaped copy before making one.
struct DavResponseRef<'a> {
    href: String,
    ics_raw: &'a str,
    deleted: bool,
}

/// Pull the `<response>` elements and the `<sync-token>` out of a multistatus.
///
/// Namespace prefixes vary by server (`d:`, `D:`, none), so tags are matched on
/// their local name.
#[cfg(test)]
pub(crate) fn parse_multistatus(xml: &str) -> (Vec<DavResponse>, String) {
    let mut out = Vec::new();
    let token = walk_multistatus(xml, |r| {
        out.push(DavResponse {
            href: r.href,
            ics: xml_unescape(r.ics_raw).into_owned(),
            deleted: r.deleted,
        });
        Ok(())
    });
    match token {
        Ok(token) => (out, token),
        Err(_) => Default::default(),
    }
}

/// Visit each `<response>` in document order, one at a time, then return the
/// collection-level sync token.
///
/// Nothing is collected here: each response is handed to `visit` straight off
/// the body, so the caller admits (or refuses) it before the next is looked at.
fn walk_multistatus<'a>(
    xml: &'a str,
    mut visit: impl FnMut(DavResponseRef<'a>) -> Result<(), CalendarError>,
) -> Result<String, CalendarError> {
    let mut rest = xml;
    while let Some((block, tail)) = take_element(rest, "response") {
        rest = tail;
        let href = first_element(block, "href").unwrap_or_default();
        let ics_raw = first_element(block, "calendar-data").unwrap_or_default();
        // A per-response status of 404 is how `sync-collection` reports a
        // deletion; a response with no calendar-data at all is one too.
        let status = first_element(block, "status").unwrap_or_default();
        let deleted = status.contains("404") || ics_raw.trim().is_empty();
        if href.trim().is_empty() {
            continue;
        }
        // Size the raw value before unescaping it (the unescaped form is never
        // longer), so an enormous href costs nothing to refuse.
        if href.trim().len() > MAX_REQUEST_BYTES {
            return Err(CalendarError::BodyLimit(
                "calendar resource href exceeds limit",
            ));
        }
        let href = xml_unescape(href.trim());
        if href.len() > MAX_REQUEST_BYTES {
            return Err(CalendarError::BodyLimit(
                "calendar resource href exceeds limit",
            ));
        }
        visit(DavResponseRef {
            href: href.into_owned(),
            ics_raw,
            deleted,
        })?;
    }
    // The collection-level token sits outside any <response>.
    let mut token = "";
    let mut rest = xml;
    while let Some((body, tail)) = take_element(rest, "sync-token") {
        token = body;
        rest = tail;
    }
    if token.trim().len() > MAX_REQUEST_BYTES {
        return Err(CalendarError::BodyLimit(
            "calendar sync token exceeds limit",
        ));
    }
    let token = xml_unescape(token.trim());
    if token.len() > MAX_REQUEST_BYTES {
        return Err(CalendarError::BodyLimit(
            "calendar sync token exceeds limit",
        ));
    }
    Ok(token.trim().to_string())
}

/// Parse a multistatus into a page, admitting each resource as it is reached.
fn page_from_multistatus(
    text: &str,
    zone: &str,
    mut meter: AdmissionMeter,
) -> Result<EventPage, CalendarError> {
    let mut events = Vec::new();
    let mut deleted = Vec::new();
    let token = walk_multistatus(text, |r| {
        if r.deleted {
            // The href is all a tombstone carries, so it has to be the id.
            // `uid_from_href` mirrors what the fetch path stores.
            let id = uid_from_href(&r.href);
            meter.admit_deletion(id.len())?;
            deleted.push(id);
            return Ok(());
        }
        // An entity-escaped resource needs an unescaped copy; reserve it as
        // transient before it exists.
        let copy = if r.ics_raw.contains('&') {
            r.ics_raw.len()
        } else {
            0
        };
        meter.reserve_transient(copy)?;
        let ics = xml_unescape(r.ics_raw);
        let parsed = thegn_core::calendar::parse_ics_admitted(&ics, zone, &mut meter, &mut events);
        drop(ics);
        meter.release_transient(copy);
        parsed.map_err(CalendarError::from)
    })?;
    EventPage::from_meter(meter, events, deleted, token)
}

fn first_element<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    take_element(xml, name).map(|(body, _)| body)
}

/// Find the first `<name>` element, returning `(raw body, remainder)`, both
/// borrowed from `xml`.
fn take_element<'a>(xml: &'a str, name: &str) -> Option<(&'a str, &'a str)> {
    let mut search = 0usize;
    loop {
        let rel = xml.get(search..)?.find('<')?;
        let lt = search + rel;
        let gt = xml.get(lt..)?.find('>')? + lt;
        let tag = xml.get(lt + 1..gt)?;
        // Skip closing tags, comments and declarations.
        if tag.starts_with('/') || tag.starts_with('?') || tag.starts_with('!') {
            search = gt + 1;
            continue;
        }
        // `name` or `prefix:name`, then whitespace / `>` / `/>`.
        let local = tag
            .split([' ', '\t', '\n', '\r', '/'])
            .next()
            .unwrap_or(tag);
        let matches = local == name || local.rsplit(':').next() == Some(name);
        if !matches {
            search = gt + 1;
            continue;
        }
        // A self-closing element has an empty body.
        if tag.ends_with('/') {
            return Some(("", xml.get(gt + 1..)?));
        }
        // Find the matching close tag by local name.
        let after = gt + 1;
        let close_rel = xml.get(after..)?.find(&format!("</{local}>"))?;
        let end = after + close_rel;
        let body = xml.get(after..end)?;
        let tail = xml.get(end + local.len() + 3..).unwrap_or("");
        return Some((body, tail));
    }
}

impl CalendarBackend for CalDavBackend {
    fn provider_id(&self) -> &'static str {
        "caldav"
    }

    fn caps(&self) -> CalendarCaps {
        CalendarCaps {
            // `sync-collection` gives real deltas AND tombstones — the only
            // provider here that can, which is why `EventPage::deleted` exists.
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
            let http = self.http.as_ref().expect("checked above");
            let deadline = Instant::now() + self.timeout;
            // Reserve the largest body the transport will accept before asking
            // for it; refusal is immediate and typed, and nothing is sent.
            let mut meter = self.admission.meter();
            meter.reserve_transient(MAX_BODY_BYTES)?;

            let incremental = !sync_token.is_empty();
            let body = if incremental {
                sync_collection_body(sync_token)?
            } else {
                calendar_query_body(from, to)
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
                return self.fetch_full(from, to, deadline, meter).await;
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
            match page_from_multistatus(&text, self.zone(), meter) {
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
                    self.fetch_full(from, to, deadline, meter).await
                }
                other => other,
            }
        })
    }
}

impl CalDavBackend {
    fn zone(&self) -> &str {
        if self.zone.is_empty() {
            "UTC"
        } else {
            &self.zone
        }
    }

    /// A 409/507 token recovery is exactly one additional REPORT, sharing the
    /// original absolute deadline and never recursively refreshing it. It
    /// reuses the first request's meter (and its body reservation).
    async fn fetch_full(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        deadline: Instant,
        mut meter: AdmissionMeter,
    ) -> Result<EventPage, CalendarError> {
        let http = self.http.as_ref().expect("initialized backend");
        let body = calendar_query_body(from, to);
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
        page_from_multistatus(&text, self.zone(), meter)
    }
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
        let xml = format!("<multistatus><sync-token>{oversized}</sync-token></multistatus>");
        assert!(matches!(
            walk_multistatus(&xml, |_| Ok(())),
            Err(CalendarError::BodyLimit(_))
        ));
    }
}
