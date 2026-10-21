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

use super::{
    AccountAdmission, CalendarBackend, CalendarCaps, CalendarError, EventPage, PageDiagnostics,
    PageMode,
};

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

/// A complete document whose only content is effective series cancellations is
/// authoritative: it is a Reset, so the host replaces the cache instead of
/// treating the empty page as a suspicious provider glitch. Any other empty
/// snapshot keeps the Snapshot guard.
pub(super) fn snapshot_mode(events_empty: bool, saw_series_tombstone: bool) -> PageMode {
    if events_empty && saw_series_tombstone {
        PageMode::Reset
    } else {
        PageMode::Snapshot
    }
}

pub struct IcsBackend {
    path: String,
    zone: thegn_core::calendar::Tz,
    admission: AccountAdmission,
}

enum ReadFailure {
    Io(std::io::Error),
    Admission(AdmissionError),
    Document,
}

impl IcsBackend {
    pub(crate) fn new(a: &CalendarAccount, admission: AccountAdmission) -> Self {
        IcsBackend {
            path: thegn_core::util::expand_tilde(&a.path),
            zone: admission.zone(),
            admission,
        }
    }

    fn read_one(
        path: &Path,
        zone: &str,
        window: (NaiveDate, NaiveDate),
        meter: &mut AdmissionMeter,
        out: &mut Vec<CalEvent>,
        remaining_bytes: usize,
        deadline: Instant,
    ) -> Result<(usize, PageDiagnostics, bool), ReadFailure> {
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
                        let mut report = thegn_core::calendar::ics::IcsReport::default();
                        thegn_core::calendar::ics::parse_ics_window_reported(
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
                            &mut report,
                        )
                        .map_err(ReadFailure::Admission)?;
                        if report.is_document_failure() {
                            Err(ReadFailure::Document)
                        } else {
                            check_deadline(deadline).map_err(ReadFailure::Io)?;
                            Ok((
                                count,
                                PageDiagnostics::from_report(&report),
                                !report.cancelled_uids().is_empty(),
                            ))
                        }
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
        let zone = self.zone.name();
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
                Ok((_, diagnostics, saw_series_tombstone)) => {
                    let mode = snapshot_mode(out.is_empty(), saw_series_tombstone);
                    return EventPage::from_meter(meter, mode, out, Vec::new(), String::new())
                        .map(|page| page.with_diagnostics(diagnostics));
                }
                Err(ReadFailure::Io(e)) => {
                    return Err(CalendarError::Io(format!("{}: {e}", self.path)));
                }
                Err(ReadFailure::Admission(e)) => return Err(e.into()),
                Err(ReadFailure::Document) => {
                    return Err(CalendarError::Parse(format!(
                        "{}: calendar document is incomplete or structurally malformed",
                        self.path
                    )));
                }
            }
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
        let mut diagnostics = PageDiagnostics::default();
        let mut saw_series_tombstone = false;
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
                Ok((n, file_diagnostics, file_has_series_tombstone)) => {
                    total_bytes += n;
                    diagnostics.absorb(file_diagnostics);
                    saw_series_tombstone |= file_has_series_tombstone;
                }
                Err(ReadFailure::Io(e)) => {
                    return Err(CalendarError::Io(format!("{}: {e}", path.display())));
                }
                Err(ReadFailure::Admission(e)) => return Err(e.into()),
                Err(ReadFailure::Document) => {
                    return Err(CalendarError::Parse(format!(
                        "{}: calendar document is incomplete or structurally malformed",
                        path.display()
                    )));
                }
            }
        }
        let mode = snapshot_mode(out.is_empty(), saw_series_tombstone);
        EventPage::from_meter(meter, mode, out, Vec::new(), String::new())
            .map(|page| page.with_diagnostics(diagnostics))
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
    use thegn_core::config_calendar::{CalendarAccount, CalendarProviderKind};

    fn local_account(path: &Path) -> CalendarAccount {
        CalendarAccount {
            name: "local".into(),
            provider: CalendarProviderKind::Ics,
            path: path.to_string_lossy().into_owned(),
            ..CalendarAccount::default()
        }
    }

    fn test_admission() -> AccountAdmission {
        AccountAdmission::isolated(Default::default(), thegn_core::calendar::Tz::UTC)
    }

    fn test_window() -> (NaiveDate, NaiveDate) {
        (
            NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 31).unwrap(),
        )
    }

    const TOMBSTONE_ONLY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:gone\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    const EMPTY_CALENDAR: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR\r\n";

    /// Regression: this assertion fails on main because an empty tombstone-only
    /// local document is returned as Snapshot and rejected by the host guard.
    #[test]
    fn tombstone_only_local_file_is_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calendar.ics");
        std::fs::write(&path, TOMBSTONE_ONLY).unwrap();
        let backend = IcsBackend::new(&local_account(&path), test_admission());

        let page = backend.read_all(test_window()).unwrap();

        assert_eq!(page.mode(), PageMode::Reset);
        assert!(page.events().is_empty());
    }

    /// Positive control: a genuinely empty document must keep the guard.
    #[test]
    fn empty_local_calendar_stays_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calendar.ics");
        std::fs::write(&path, EMPTY_CALENDAR).unwrap();
        let backend = IcsBackend::new(&local_account(&path), test_admission());

        let page = backend.read_all(test_window()).unwrap();

        assert_eq!(page.mode(), PageMode::Snapshot);
        assert!(page.events().is_empty());
    }

    /// Positive control: a surviving event means this is still an ordinary snapshot.
    #[test]
    fn tombstone_beside_a_live_event_stays_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calendar.ics");
        let text = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:gone\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:live\r\nDTSTART:20260814T090000Z\r\nDTEND:20260814T100000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        std::fs::write(&path, text).unwrap();
        let backend = IcsBackend::new(&local_account(&path), test_admission());

        let page = backend.read_all(test_window()).unwrap();

        assert_eq!(page.mode(), PageMode::Snapshot);
        assert_eq!(page.events().len(), 1);
        assert_eq!(page.events()[0].uid, "live");
    }

    /// Regression: an effective cancellation in a vdir must make the empty
    /// combined document authoritative, even alongside a genuinely empty file.
    #[test]
    fn tombstone_only_vdir_is_a_reset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.ics"), TOMBSTONE_ONLY).unwrap();
        std::fs::write(dir.path().join("b.ics"), EMPTY_CALENDAR).unwrap();
        let backend = IcsBackend::new(&local_account(dir.path()), test_admission());

        let page = backend.read_all(test_window()).unwrap();

        assert_eq!(page.mode(), PageMode::Reset);
        assert!(page.events().is_empty());
    }

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
