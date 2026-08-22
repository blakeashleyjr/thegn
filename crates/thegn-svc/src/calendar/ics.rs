//! Local `.ics` files.
//!
//! `path` may be a single file **or a directory**, and the directory form is
//! deliberate: one `.ics` per event in a folder is exactly the vdir layout that
//! vdirsyncer and khal already write, so this one backend transparently serves
//! anyone with that setup and no extra configuration.
//!
//! A thin shell over [`thegn_core::calendar::parse_ics`] — the parsing itself
//! is pure and lives in core, under the coverage gate.

use chrono::NaiveDate;
use thegn_core::calendar::CalEvent;
use thegn_core::config_calendar::CalendarAccount;

use super::{CalendarBackend, CalendarCaps, CalendarError, EventPage};

/// Cap on files read from a vdir, so a runaway directory can't stall a sync.
const MAX_FILES: usize = 20_000;

pub struct IcsBackend {
    path: String,
    /// Zone for floating times that name no `TZID`.
    zone: String,
    max_events: usize,
}

impl IcsBackend {
    pub fn new(a: &CalendarAccount) -> Self {
        IcsBackend {
            path: thegn_core::util::expand_tilde(&a.path),
            zone: String::new(),
            max_events: 0,
        }
    }

    /// Set the zone floating times are anchored to (the resolved home zone).
    pub fn with_zone(mut self, zone: &str) -> Self {
        self.zone = zone.to_string();
        self
    }

    pub fn with_max_events(mut self, n: usize) -> Self {
        self.max_events = n;
        self
    }

    fn read_all(&self) -> Result<Vec<CalEvent>, CalendarError> {
        let p = std::path::Path::new(&self.path);
        if !p.exists() {
            // A missing file is configuration, not a blip — see
            // `CalendarError::is_transient`.
            return Err(CalendarError::Io(format!("no such path: {}", self.path)));
        }
        let zone = if self.zone.is_empty() {
            "UTC"
        } else {
            &self.zone
        };
        if p.is_file() {
            let body = std::fs::read_to_string(p)
                .map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
            return Ok(thegn_core::calendar::parse_ics(&body, zone));
        }
        let entries =
            std::fs::read_dir(p).map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
        let mut out = Vec::new();
        for entry in entries.flatten().take(MAX_FILES) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("ics") {
                continue;
            }
            // One unreadable file in a vdir must not lose the other hundred.
            match std::fs::read_to_string(&path) {
                Ok(body) => out.extend(thegn_core::calendar::parse_ics(&body, zone)),
                Err(e) => tracing::debug!(
                    target: "thegn::calendar",
                    file = %path.display(),
                    error = %e,
                    "skipping unreadable .ics"
                ),
            }
        }
        Ok(out)
    }
}

impl CalendarBackend for IcsBackend {
    fn provider_id(&self) -> &'static str {
        "ics"
    }

    fn caps(&self) -> CalendarCaps {
        CalendarCaps::default()
    }

    async fn list_events(
        &self,
        _from: NaiveDate,
        _to: NaiveDate,
        _sync_token: &str,
    ) -> Result<EventPage, CalendarError> {
        // Deliberately returns everything rather than pre-filtering by the
        // window: recurrence masters can sit far outside it and still produce
        // occurrences inside, so the host expands and filters.
        let mut events = self.read_all()?;
        let partial = self.max_events > 0 && events.len() > self.max_events;
        if partial {
            events.truncate(self.max_events);
        }
        Ok(EventPage {
            events,
            partial,
            ..Default::default()
        })
    }
}
