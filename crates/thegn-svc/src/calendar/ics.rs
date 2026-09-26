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
    pub(crate) fn new(a: &CalendarAccount, admission: AccountAdmission) -> Self {
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

    /// Read one file under the document budget, reserving the document
    /// ceiling as transient before any byte is allocated, then parse the
    /// events that can occur in `window` into `out`.
    fn read_one(
        path: &std::path::Path,
        zone: &str,
        window: (NaiveDate, NaiveDate),
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
        // Reserve the ceiling, not the declared size: a file that grows while
        // being read (or a pseudo-file declaring 0) is still bounded by it,
        // and `read_capped` never allocates past it.
        meter
            .reserve_transient(MAX_SOURCE_DOCUMENT_BYTES)
            .map_err(ReadFailure::Admission)?;
        let result = match read_capped(file, declared, MAX_SOURCE_DOCUMENT_BYTES) {
            Err(e) => Err(ReadFailure::Io(e)),
            Ok(None) => Err(ReadFailure::Admission(AdmissionError::new(
                AdmissionLimit::DocumentBytes,
            ))),
            Ok(Some(body)) => match String::from_utf8(body) {
                Err(e) => Err(ReadFailure::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.utf8_error(),
                ))),
                Ok(text) => {
                    thegn_core::calendar::parse_ics_window(&text, zone, Some(window), meter, out)
                        .map_err(ReadFailure::Admission)
                }
            },
        };
        meter.release_transient(MAX_SOURCE_DOCUMENT_BYTES);
        result
    }

    fn read_all(&self, window: (NaiveDate, NaiveDate)) -> Result<EventPage, CalendarError> {
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
            match Self::read_one(p, zone, window, &mut meter, &mut out) {
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
            match Self::read_one(&path, zone, window, &mut meter, &mut out) {
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

/// Read at most `limit` bytes; `Ok(None)` when the source holds more.
///
/// Growth is explicit and capped, so the buffer's capacity never exceeds
/// `limit` even for a file that declared a smaller size (or none — a
/// pseudo-file) and kept growing; `read_to_end`'s doubling could reach twice
/// the cap.
fn read_capped(
    mut r: impl Read,
    declared: usize,
    limit: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut body: Vec<u8> = Vec::with_capacity(declared.min(limit));
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = match r.read(&mut chunk) {
            Ok(0) => return Ok(Some(body)),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if body.len() + n > limit {
            return Ok(None);
        }
        if body.capacity() - body.len() < n {
            let grown = (body.capacity().max(64 * 1024) * 2).min(limit);
            body.reserve_exact(grown.max(body.len() + n) - body.len());
        }
        body.extend_from_slice(&chunk[..n]);
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
        from: NaiveDate,
        to: NaiveDate,
        _sync_token: &'a str,
    ) -> BoxFuture<'a, Result<EventPage, CalendarError>> {
        Box::pin(async move {
            // Admits every event that can occur in the window — including
            // recurrence masters that start far before it — and releases the
            // provably-outside history, so `max_events` counts what the sync
            // horizon needs. The host expands and filters.
            self.read_all((from, to))
        })
    }
}
