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

use std::time::Duration;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::config_calendar::CalendarAccount;
use tokio::time::Instant;

use super::{CalendarBackend, CalendarCaps, CalendarError, EventPage};
use crate::http::{
    CalendarHttpClient, CalendarHttpError, ExpectedMedia, MAX_REQUEST_BYTES, discard_body,
    map_error, read_body, validate_encoding, validate_media,
};

pub struct CalDavBackend {
    username: String,
    token: String,
    zone: String,
    timeout: Duration,
    max_events: usize,
    http: Option<CalendarHttpClient>,
    init_error: Option<CalendarHttpError>,
}

impl CalDavBackend {
    pub fn new(a: &CalendarAccount) -> Self {
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
            max_events: 0,
            http,
            init_error,
        }
    }

    pub fn with_zone(mut self, zone: &str) -> Self {
        self.zone = zone.to_string();
        self
    }

    pub fn with_max_events(mut self, n: usize) -> Self {
        self.max_events = n;
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

fn xml_unescape(s: &str) -> String {
    // `&amp;` last, or `&amp;lt;` would wrongly become `<`.
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// One `<response>` from a multistatus body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DavResponse {
    pub href: String,
    /// The `calendar-data` payload, empty for a tombstone.
    pub ics: String,
    /// True when the response reports the resource as gone (404 status, or a
    /// `sync-collection` removal).
    pub deleted: bool,
}

/// Pull the `<response>` elements and the `<sync-token>` out of a multistatus.
///
/// Namespace prefixes vary by server (`d:`, `D:`, none), so tags are matched on
/// their local name.
pub(crate) fn parse_multistatus(xml: &str) -> (Vec<DavResponse>, String) {
    parse_multistatus_checked(xml).unwrap_or_default()
}

fn parse_multistatus_checked(xml: &str) -> Result<(Vec<DavResponse>, String), CalendarError> {
    let mut out = Vec::new();
    for block in split_elements(xml, "response") {
        let href = first_element(&block, "href").unwrap_or_default();
        let ics = first_element(&block, "calendar-data").unwrap_or_default();
        // A per-response status of 404 is how `sync-collection` reports a
        // deletion; a response with no calendar-data at all is one too.
        let status = first_element(&block, "status").unwrap_or_default();
        let deleted = status.contains("404") || ics.trim().is_empty();
        if href.trim().is_empty() {
            continue;
        }
        if href.len() > MAX_REQUEST_BYTES {
            return Err(CalendarError::BodyLimit(
                "calendar resource href exceeds limit",
            ));
        }
        out.push(DavResponse {
            href: href.trim().to_string(),
            ics,
            deleted,
        });
    }
    // The collection-level token sits outside any <response>.
    let token = last_element(xml, "sync-token").unwrap_or_default();
    if token.len() > MAX_REQUEST_BYTES {
        return Err(CalendarError::BodyLimit(
            "calendar sync token exceeds limit",
        ));
    }
    Ok((out, token.trim().to_string()))
}

/// Every `<...name>…</...name>` body in `xml`, prefix-insensitive.
fn split_elements(xml: &str, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some((body, tail)) = take_element(rest, name) {
        out.push(body);
        rest = tail;
    }
    out
}

fn first_element(xml: &str, name: &str) -> Option<String> {
    take_element(xml, name).map(|(body, _)| body)
}

fn last_element(xml: &str, name: &str) -> Option<String> {
    split_elements(xml, name).pop()
}

/// Find the first `<name>` element, returning `(unescaped body, remainder)`.
fn take_element<'a>(xml: &'a str, name: &str) -> Option<(String, &'a str)> {
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
            return Some((String::new(), xml.get(gt + 1..)?));
        }
        // Find the matching close tag by local name.
        let after = gt + 1;
        let close_rel = xml.get(after..)?.find(&format!("</{local}>"))?;
        let end = after + close_rel;
        let body = xml.get(after..end)?;
        let tail = xml.get(end + local.len() + 3..).unwrap_or("");
        return Some((xml_unescape(body), tail));
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
                .map_err(|error| CalendarError::Network(map_error(error).into()))?;

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
                    .map_err(|error| CalendarError::Network(map_error(error).into()))?;
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
                    .map_err(|error| CalendarError::Network(map_error(error).into()))?;
                return self.fetch_full(from, to, deadline).await;
            }
            if !resp.status().is_success() && resp.status() != reqwest::StatusCode::MULTI_STATUS {
                let status = resp.status();
                discard_body(resp, deadline)
                    .await
                    .map_err(|error| CalendarError::Network(map_error(error).into()))?;
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
            let text = String::from_utf8(text)
                .map_err(|_| CalendarError::Parse("CalDAV response is not UTF-8".into()))?;

            let (responses, token) = parse_multistatus_checked(&text)?;
            let zone = if self.zone.is_empty() {
                "UTC"
            } else {
                &self.zone
            };
            let mut events = Vec::new();
            let mut deleted = Vec::new();
            for r in responses {
                if r.deleted {
                    // The href is all a tombstone carries, so it has to be the id.
                    // `uid_from_href` mirrors what the fetch path stores.
                    deleted.push(uid_from_href(&r.href));
                    continue;
                }
                events.extend(thegn_core::calendar::parse_ics(&r.ics, zone));
            }
            let partial = self.max_events > 0 && events.len() > self.max_events;
            if partial {
                events.truncate(self.max_events);
            }
            Ok(EventPage {
                events,
                deleted,
                sync_token: token,
                partial,
                unchanged: false,
            })
        })
    }
}

impl CalDavBackend {
    /// A 409/507 token recovery is exactly one additional REPORT, sharing the
    /// original absolute deadline and never recursively refreshing it.
    async fn fetch_full(
        &self,
        from: NaiveDate,
        to: NaiveDate,
        deadline: Instant,
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
            .map_err(|error| CalendarError::Network(map_error(error).into()))?;
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
                .map_err(|error| CalendarError::Network(map_error(error).into()))?;
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
        let text = String::from_utf8(bytes)
            .map_err(|_| CalendarError::Parse("CalDAV response is not UTF-8".into()))?;
        self.page_from_multistatus(text)
    }

    fn page_from_multistatus(&self, text: String) -> Result<EventPage, CalendarError> {
        let (responses, token) = parse_multistatus_checked(&text)?;
        let zone = if self.zone.is_empty() {
            "UTC"
        } else {
            &self.zone
        };
        let mut events = Vec::new();
        let mut deleted = Vec::new();
        for r in responses {
            if r.deleted {
                deleted.push(uid_from_href(&r.href));
            } else {
                events.extend(thegn_core::calendar::parse_ics(&r.ics, zone));
            }
        }
        let partial = self.max_events > 0 && events.len() > self.max_events;
        if partial {
            events.truncate(self.max_events);
        }
        Ok(EventPage {
            events,
            deleted,
            sync_token: token,
            partial,
            unchanged: false,
        })
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
            parse_multistatus_checked(&xml),
            Err(CalendarError::BodyLimit(_))
        ));
    }
}
