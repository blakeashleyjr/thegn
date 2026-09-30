//! Bounded local `.ics` files and vdirs.
//!
//! Files are selected deterministically, opened through core's no-follow
//! regular-file seam, and admitted whole-account. Any incomplete scan or read
//! is an error: `EventPage` is complete-or-error, so returning a prefix would
//! incorrectly authorize replacement of the account cache.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use futures_util::future::BoxFuture;
use thegn_core::calendar::admission::{AdmissionLimit, MAX_SOURCE_DOCUMENT_BYTES};
use thegn_core::calendar::{AdmissionError, AdmissionMeter, CalEvent};
use thegn_core::config_calendar::CalendarAccount;

use super::{AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage};

/// Maximum eligible `.ics` files in one vdir.
pub const MAX_FILES: usize = 20_000;
/// Bounds work spent inspecting unrelated names as well as eligible entries.
pub const MAX_DIRECTORY_ENTRIES: usize = MAX_FILES * 4;
/// Total bytes accepted from one local source, across all files.
pub const MAX_AGGREGATE_SOURCE_BYTES: usize = 256 << 20;
/// Cooperative wall-clock ceiling for directory scan, reads, and parsing.
pub const SOURCE_DEADLINE: Duration = Duration::from_secs(30);
const READ_CHUNK_BYTES: usize = 64 * 1024;
const _: () = assert!(MAX_AGGREGATE_SOURCE_BYTES > MAX_SOURCE_DOCUMENT_BYTES);
const _: () = assert!(MAX_DIRECTORY_ENTRIES > MAX_FILES);

pub struct IcsBackend {
    path: String,
    zone: String,
    admission: AccountAdmission,
}

enum ReadFailure {
    Io(std::io::Error),
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

    fn read_one(
        path: &Path,
        zone: &str,
        window: (NaiveDate, NaiveDate),
        meter: &mut AdmissionMeter,
        out: &mut Vec<CalEvent>,
        remaining_bytes: usize,
        deadline: Instant,
    ) -> Result<usize, ReadFailure> {
        check_deadline(deadline).map_err(ReadFailure::Io)?;
        let file = thegn_core::fsperm::open_regular_file_nofollow(path).map_err(ReadFailure::Io)?;
        let declared =
            usize::try_from(file.metadata().map_err(ReadFailure::Io)?.len()).unwrap_or(usize::MAX);
        if declared > MAX_SOURCE_DOCUMENT_BYTES {
            return Err(ReadFailure::Admission(AdmissionError::new(
                AdmissionLimit::DocumentBytes,
            )));
        }
        let allowance = remaining_bytes.min(MAX_SOURCE_DOCUMENT_BYTES);
        if declared > allowance {
            return Err(ReadFailure::Admission(AdmissionError::new(
                if remaining_bytes < MAX_SOURCE_DOCUMENT_BYTES {
                    AdmissionLimit::AggregateSourceBytes
                } else {
                    AdmissionLimit::DocumentBytes
                },
            )));
        }
        if allowance == 0 {
            return Err(ReadFailure::Admission(AdmissionError::new(
                AdmissionLimit::AggregateSourceBytes,
            )));
        }
        meter
            .reserve_transient(allowance)
            .map_err(ReadFailure::Admission)?;
        let result = match read_capped(file, declared, allowance, deadline) {
            Err(e) => Err(ReadFailure::Io(e)),
            Ok(None) => Err(ReadFailure::Admission(AdmissionError::new(
                if remaining_bytes < MAX_SOURCE_DOCUMENT_BYTES {
                    AdmissionLimit::AggregateSourceBytes
                } else {
                    AdmissionLimit::DocumentBytes
                },
            ))),
            Ok(Some(body)) => {
                let count = body.len();
                match String::from_utf8(body) {
                    Err(e) => Err(ReadFailure::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        e.utf8_error(),
                    ))),
                    Ok(text) => {
                        check_deadline(deadline).map_err(ReadFailure::Io)?;
                        thegn_core::calendar::ics::parse_ics_window_checked(
                            &text,
                            zone,
                            Some(window),
                            meter,
                            out,
                            || {
                                check_deadline(deadline).map_err(|_| {
                                    AdmissionError::new(AdmissionLimit::SourceDeadline)
                                })
                            },
                        )
                        .map_err(ReadFailure::Admission)?;
                        check_deadline(deadline).map_err(ReadFailure::Io)?;
                        Ok(count)
                    }
                }
            }
        };
        meter.release_transient(allowance);
        result
    }

    fn read_all(&self, window: (NaiveDate, NaiveDate)) -> Result<EventPage, CalendarError> {
        let p = Path::new(&self.path);
        let metadata = std::fs::symlink_metadata(p)
            .map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
        let deadline = Instant::now() + SOURCE_DEADLINE;
        let zone = if self.zone.is_empty() {
            "UTC"
        } else {
            &self.zone
        };
        let mut meter = self.admission.meter();
        let mut out = Vec::new();

        if metadata.file_type().is_file() {
            match Self::read_one(
                p,
                zone,
                window,
                &mut meter,
                &mut out,
                MAX_AGGREGATE_SOURCE_BYTES,
                deadline,
            ) {
                Ok(_) => {}
                Err(ReadFailure::Io(e)) => {
                    return Err(CalendarError::Io(format!("{}: {e}", self.path)));
                }
                Err(ReadFailure::Admission(e)) => return Err(e.into()),
            }
            return EventPage::from_meter(meter, out, Vec::new(), String::new());
        }
        if !metadata.file_type().is_dir() {
            return Err(CalendarError::Io(format!(
                "{} is not a regular calendar file or directory",
                self.path
            )));
        }

        let entries =
            std::fs::read_dir(p).map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
        let mut candidates = Vec::new();
        let mut visited = 0;
        for entry in entries {
            check_deadline(deadline)
                .map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
            let entry = entry.map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
            visited += 1;
            if visited > MAX_DIRECTORY_ENTRIES {
                return Err(CalendarError::Io(format!(
                    "{} has more than {MAX_DIRECTORY_ENTRIES} directory entries",
                    self.path
                )));
            }
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "ics") {
                continue;
            }
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| CalendarError::Io(format!("{}: {e}", path.display())))?;
            if meta.file_type().is_symlink() {
                return Err(CalendarError::Io(format!(
                    "{} is a symlink, not a regular calendar file",
                    path.display()
                )));
            }
            if !meta.file_type().is_file() {
                return Err(CalendarError::Io(format!(
                    "{} is not a regular calendar file",
                    path.display()
                )));
            }
            candidates.push(path);
        }
        candidates = select_candidates(candidates)?;
        let mut total_bytes = 0usize;
        for path in candidates {
            check_deadline(deadline)
                .map_err(|e| CalendarError::Io(format!("{}: {e}", self.path)))?;
            let remaining = MAX_AGGREGATE_SOURCE_BYTES.saturating_sub(total_bytes);
            match Self::read_one(
                path.as_path(),
                zone,
                window,
                &mut meter,
                &mut out,
                remaining,
                deadline,
            ) {
                Ok(n) => total_bytes += n,
                Err(ReadFailure::Io(e)) => {
                    return Err(CalendarError::Io(format!("{}: {e}", path.display())));
                }
                Err(ReadFailure::Admission(e)) => return Err(e.into()),
            }
        }
        EventPage::from_meter(meter, out, Vec::new(), String::new())
    }
}

fn select_candidates(mut paths: Vec<PathBuf>) -> Result<Vec<PathBuf>, CalendarError> {
    paths.sort();
    paths.dedup();
    if paths.len() > MAX_FILES {
        return Err(CalendarError::Io(format!(
            "vdir has {} eligible calendar files; limit is {MAX_FILES}",
            paths.len()
        )));
    }
    Ok(paths)
}

fn check_deadline(deadline: Instant) -> std::io::Result<()> {
    if Instant::now() >= deadline {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "local calendar source exceeded its time budget",
        ))
    } else {
        Ok(())
    }
}

/// Capped growth avoids `read_to_end` capacity doubling past the admitted size.
fn read_capped(
    mut reader: impl Read,
    declared: usize,
    limit: usize,
    deadline: Instant,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut body = Vec::with_capacity(declared.min(limit));
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        check_deadline(deadline)?;
        let n = match reader.read(&mut chunk) {
            Ok(0) => return Ok(Some(body)),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if body.len() + n > limit {
            return Ok(None);
        }
        if body.capacity() - body.len() < n {
            let grown = body
                .capacity()
                .max(READ_CHUNK_BYTES)
                .saturating_mul(2)
                .min(limit);
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
        Box::pin(async move { self.read_all((from, to)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_reader_accepts_exact_budget_and_refuses_the_first_excess_chunk() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let exact = read_capped(std::io::Cursor::new(b"12345678"), 0, 8, deadline)
            .unwrap()
            .unwrap();
        assert_eq!(exact, b"12345678");
        assert!(exact.capacity() <= 8);

        let over = read_capped(std::io::Cursor::new(b"123456789"), 0, 8, deadline).unwrap();
        assert!(over.is_none(), "the source must be refused, not truncated");
    }

    #[test]
    fn capped_reader_rechecks_growing_sources_before_buffer_growth() {
        struct Growing {
            reads: usize,
        }
        impl Read for Growing {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                out.fill(b'x');
                Ok(out.len())
            }
        }

        let mut reader = Growing { reads: 0 };
        let result = read_capped(&mut reader, 0, 8, Instant::now() + Duration::from_secs(1));
        assert!(matches!(result, Ok(None)));
        // The first fixed-size chunk proves the source exceeds the limit; no
        // second read or buffer growth is needed to decide refusal.
        assert_eq!(reader.reads, 1);
    }

    #[test]
    fn candidate_selection_is_stable_and_refuses_truncation() {
        let names = ["z.ics", "a.ics", "m.ics"];
        let first = select_candidates(names.iter().map(PathBuf::from).collect()).unwrap();
        let second = select_candidates(names.iter().rev().map(PathBuf::from).collect()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first[0], PathBuf::from("a.ics"));
        let excess = (0..=MAX_FILES)
            .map(|n| PathBuf::from(format!("{n:05}.ics")))
            .collect();
        assert!(matches!(
            select_candidates(excess),
            Err(CalendarError::Io(_))
        ));
        assert_eq!(MAX_DIRECTORY_ENTRIES, MAX_FILES * 4);
    }
}
