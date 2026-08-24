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

use super::{CalendarBackend, CalendarCaps, CalendarError, EventPage};

/// Refuse to buffer a response larger than this.
const MAX_BODY: usize = 32 << 20;

pub struct CalDavBackend {
    url: String,
    username: String,
    token: String,
    zone: String,
    timeout: Duration,
    max_events: usize,
}

impl CalDavBackend {
    pub fn new(a: &CalendarAccount) -> Self {
        CalDavBackend {
            url: a.url.trim().to_string(),
            username: a.username.clone(),
            token: thegn_core::config::expand_env_ref(&a.token).unwrap_or_default(),
            zone: String::new(),
            timeout: Duration::from_secs(a.timeout_secs.clamp(5, 120)),
            max_events: 0,
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

/// A `sync-collection` report resuming from `token`.
fn sync_collection_body(token: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8" ?>
<d:sync-collection xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:sync-token>{}</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
</d:sync-collection>"#,
        xml_escape(token)
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
        out.push(DavResponse {
            href: href.trim().to_string(),
            ics,
            deleted,
        });
    }
    // The collection-level token sits outside any <response>.
    let token = last_element(xml, "sync-token").unwrap_or_default();
    (out, token.trim().to_string())
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
            if self.url.is_empty() {
                return Err(CalendarError::NotConfigured);
            }
            let client = reqwest::Client::builder()
                .timeout(self.timeout)
                .build()
                .map_err(|e| CalendarError::Network(e.to_string()))?;

            let incremental = !sync_token.is_empty();
            let body = if incremental {
                sync_collection_body(sync_token)
            } else {
                calendar_query_body(from, to)
            };
            let method = reqwest::Method::from_bytes(b"REPORT")
                .map_err(|e| CalendarError::Api(e.to_string()))?;
            let req = client
                .request(method, &self.url)
                .header(
                    reqwest::header::CONTENT_TYPE,
                    "application/xml; charset=utf-8",
                )
                // Depth 1 = the collection's members, not the whole tree.
                .header("Depth", "1")
                .body(body);

            let resp = self
                .auth(req)
                .send()
                .await
                .map_err(|e| CalendarError::Network(e.to_string()))?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED
                || resp.status() == reqwest::StatusCode::FORBIDDEN
            {
                return Err(CalendarError::Auth(format!("HTTP {}", resp.status())));
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
                return self.list_events(from, to, "").await;
            }
            if !resp.status().is_success() && resp.status() != reqwest::StatusCode::MULTI_STATUS {
                return Err(CalendarError::Api(format!("HTTP {}", resp.status())));
            }

            let text = resp
                .text()
                .await
                .map_err(|e| CalendarError::Network(e.to_string()))?;
            if text.len() > MAX_BODY {
                return Err(CalendarError::Api("calendar too large".into()));
            }

            let (responses, token) = parse_multistatus(&text);
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
