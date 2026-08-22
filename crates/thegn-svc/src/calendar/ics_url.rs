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
use thegn_core::config_calendar::CalendarAccount;

use super::{CalendarBackend, CalendarCaps, CalendarError, EventPage};

/// Refuse to buffer a calendar larger than this.
const MAX_BODY: usize = 32 << 20;

pub struct IcsUrlBackend {
    url: String,
    username: String,
    token: String,
    zone: String,
    timeout: Duration,
    max_events: usize,
}

impl IcsUrlBackend {
    pub fn new(a: &CalendarAccount) -> Self {
        IcsUrlBackend {
            // `webcal://` is just https with a scheme that tells the OS to hand
            // the link to a calendar app; over the wire it is an ordinary GET.
            url: a
                .url
                .trim()
                .replacen("webcal://", "https://", 1)
                .to_string(),
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

    async fn list_events(
        &self,
        _from: NaiveDate,
        _to: NaiveDate,
        sync_token: &str,
    ) -> Result<EventPage, CalendarError> {
        if self.url.is_empty() {
            return Err(CalendarError::NotConfigured);
        }
        let client = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| CalendarError::Network(e.to_string()))?;
        let mut req = client.get(&self.url);
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
        let resp = req
            .send()
            .await
            .map_err(|e| CalendarError::Network(e.to_string()))?;

        if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
            // Nothing changed. Return the SAME token and `unchanged`, so the
            // caller leaves the cache exactly as it is rather than reading an
            // empty page as "the calendar was emptied".
            return Ok(EventPage {
                sync_token: sync_token.to_string(),
                unchanged: true,
                ..Default::default()
            });
        }
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED
            || resp.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(CalendarError::Auth(format!("HTTP {}", resp.status())));
        }
        if !resp.status().is_success() {
            return Err(CalendarError::Api(format!("HTTP {}", resp.status())));
        }
        let etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if let Some(len) = resp.content_length()
            && len as usize > MAX_BODY
        {
            return Err(CalendarError::Api(format!(
                "calendar too large ({len} bytes)"
            )));
        }
        let body = resp
            .text()
            .await
            .map_err(|e| CalendarError::Network(e.to_string()))?;
        if body.len() > MAX_BODY {
            return Err(CalendarError::Api("calendar too large".into()));
        }
        let zone = if self.zone.is_empty() {
            "UTC"
        } else {
            &self.zone
        };
        let mut events = thegn_core::calendar::parse_ics(&body, zone);
        let partial = self.max_events > 0 && events.len() > self.max_events;
        if partial {
            events.truncate(self.max_events);
        }
        Ok(EventPage {
            events,
            sync_token: etag,
            partial,
            ..Default::default()
        })
    }
}
