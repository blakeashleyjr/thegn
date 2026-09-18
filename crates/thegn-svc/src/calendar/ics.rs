//! Local `.ics` files.
//!
//! `path` may be a single file **or a directory**, and the directory form is
//! deliberate: one `.ics` per event in a folder is exactly the vdir layout that
//! vdirsyncer and khal already write, so this one backend transparently serves
//! anyone with that setup and no extra configuration.
//!
//! A thin shell over [`thegn_core::calendar::parse_ics`] — the parsing itself
//! is pure and lives in core, under the coverage gate.

use std::io::Read;

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::calendar::admission::{AdmissionLimit, MAX_SOURCE_DOCUMENT_BYTES};
use thegn_core::calendar::{AdmissionError, AdmissionMeter, CalEvent};
use thegn_core::config_calendar::CalendarAccount;

use super::{AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage};

/// Cap on files read from a vdir, so a runaway directory can't stall a sync.
const MAX_FILES: usize = 20_000;

pub struct IcsBackend {
    path: String,
    /// Zone for floating times that name no `TZID`.
    zone: String,
    admission: AccountAdmission,
}

/// Why one file could not be read.
enum ReadFailure {
    /// Unreadable or not UTF-8 — a vdir skips it.
    Io(std::io::Error),
    /// Over the admission budget — never skipped, or the calendar would look
    /// complete without it.
    Admission(AdmissionError),
}

impl IcsBackend {
    pub fn new(a: &CalendarAccount, admission: AccountAdmission) -> Self {
        IcsBackend {
            path: thegn_core::util::expand_tilde(&a.path),
            zone: String::new(),
            admission,
        }
    }

    /// Set the zone floating times are anchored to (the resolved home zone).
    pub fn with_zone(mut self, zone: &str) -> Self {
        self.zone = zone.to_string();
        self
    }

    /// Read one file under the document budget, reserving it as transient
    /// before its bytes are allocated, then parse it into `out`.
    fn read_one(
        path: &std::path::Path,
        zone: &str,
        meter: &mut AdmissionMeter,
        out: &mut Vec<CalEvent>,
    ) -> Result<(), ReadFailure> {
        let file = std::fs::File::open(path).map_err(ReadFailure::Io)?;
        let declared = file.metadata().map_err(ReadFailure::Io)?.len();
        let declared = usize::try_from(declared).unwrap_or(usize::MAX);
        if declared > MAX_SOURCE_DOCUMENT_BYTES {
            return Err(ReadFailure::Admission(AdmissionError::new(
                AdmissionLimit::DocumentBytes,
            )));
        }
        // Reserve the file as declared; a file that grows while being read is
        // still stopped at the document cap by `take`.
        meter
            .reserve_transient(MAX_SOURCE_DOCUMENT_BYTES)
            .map_err(ReadFailure::Admission)?;
        let mut body = Vec::with_capacity(declared);
        let read = file
            .take(MAX_SOURCE_DOCUMENT_BYTES as u64 + 1)
            .read_to_end(&mut body);
        let result = match read {
            Err(e) => Err(ReadFailure::Io(e)),
            Ok(n) if n > MAX_SOURCE_DOCUMENT_BYTES => Err(ReadFailure::Admission(
                AdmissionError::new(AdmissionLimit::DocumentBytes),
            )),
            Ok(_) => match String::from_utf8(body) {
                Err(e) => Err(ReadFailure::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.utf8_error(),
                ))),
                Ok(text) => thegn_core::calendar::parse_ics_admitted(&text, zone, meter, out)
                    .map_err(ReadFailure::Admission),
            },
        };
        meter.release_transient(MAX_SOURCE_DOCUMENT_BYTES);
        result
    }

    fn read_all(&self) -> Result<EventPage, CalendarError> {
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
        let mut meter = self.admission.meter();
        let mut out = Vec::new();
        if p.is_file() {
            match Self::read_one(p, zone, &mut meter, &mut out) {
                Ok(()) => {}
                Err(ReadFailure::Io(e)) => {
                    return Err(CalendarError::Io(format!("{}: {e}", self.path)));
                }
                Err(ReadFailure::Admission(e)) => return Err(e.into()),
            }
            return EventPage::from_meter(meter, out, Vec::new(), String::new());
        }
        let entries =
            std::fs::read_dir(p).map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
        for entry in entries.flatten().take(MAX_FILES) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("ics") {
                continue;
            }
            match Self::read_one(&path, zone, &mut meter, &mut out) {
                Ok(()) => {}
                // One unreadable file in a vdir must not lose the other hundred.
                Err(ReadFailure::Io(e)) => tracing::debug!(
                    target: "thegn::calendar",
                    file = %path.display(),
                    error = %e,
                    "skipping unreadable .ics"
                ),
                // But an over-budget one stops the whole fetch: publishing the
                // rest would present a partial calendar as complete.
                Err(ReadFailure::Admission(e)) => return Err(e.into()),
            }
        }
        EventPage::from_meter(meter, out, Vec::new(), String::new())
    }
}

impl CalendarBackend for IcsBackend {
    fn provider_id(&self) -> &'static str {
        "ics"
    }

    fn caps(&self) -> CalendarCaps {
        CalendarCaps::default()
    }

    fn list_events<'a>(
        &'a self,
        _from: NaiveDate,
        _to: NaiveDate,
        _sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        Box::pin(async move {
            // Deliberately returns everything rather than pre-filtering by the
            // window: recurrence masters can sit far outside it and still produce
            // occurrences inside, so the host expands and filters. The account's
            // admission budget bounds how much "everything" can be.
            self.read_all()
        })
    }
}
