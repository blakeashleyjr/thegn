use super::{FIXTURES, Verdict, ics_document};
use crate::calendar::admission::{AdmissionBudget, AdmissionMeter};
use crate::calendar::ics::{IcsDiagnostic, IcsReport, parse_ics_window_reported};
use crate::calendar::{CalEvent, validate::validate_event};
use std::collections::HashSet;

#[test]
#[allow(
    clippy::disallowed_macros,
    reason = "conformance matrix prints every fixture x seam verdict before asserting"
)]
fn conformance_core_parser_matches_every_fixture() {
    let mut table = Vec::new();
    for fixture in FIXTURES {
        if let Some((properties, expected)) = fixture.ics {
            let mut out = Vec::new();
            let mut report = IcsReport::default();
            let result = parse_ics_window_reported(
                &ics_document(properties),
                "UTC",
                None,
                &mut AdmissionMeter::isolated(AdmissionBudget::default()),
                &mut out,
                || Ok(()),
                &mut report,
            );
            let actual = if result.is_ok() && out.len() == 1 {
                Verdict::Admit
            } else {
                Verdict::Reject
            };
            table.push((fixture.name, "core ICS parser", expected, actual));
        }
        if let Some((json, expected)) = fixture.json {
            let actual = match serde_json::from_str::<CalEvent>(json) {
                Ok(event) if validate_event(&event).is_ok() => Verdict::Admit,
                _ => Verdict::Reject,
            };
            table.push((fixture.name, "core JSON validator", expected, actual));
        }
    }
    eprintln!("calendar conformance core table: {table:#?}");
    assert!(
        table
            .iter()
            .all(|(_, _, expected, actual)| expected == actual),
        "calendar conformance disagreement: {table:#?}"
    );
}

#[test]
#[allow(
    clippy::disallowed_macros,
    reason = "conformance matrix prints every fixture x seam verdict before asserting"
)]
fn conformance_fixture_names_are_unique() {
    let mut names = HashSet::new();
    let table: Vec<_> = FIXTURES
        .iter()
        .map(|fixture| {
            (
                fixture.name,
                fixture.ics.map(|(_, verdict)| verdict),
                fixture.json.map(|(_, verdict)| verdict),
                names.insert(fixture.name),
            )
        })
        .collect();
    eprintln!("calendar fixture name and verdict table: {table:#?}");
    assert!(
        table.iter().all(|(_, _, _, unique)| *unique),
        "duplicate calendar fixture name: {table:#?}"
    );
}

#[test]
fn a_duration_at_the_timedelta_limit_does_not_panic_the_window_filter() {
    let mut out = Vec::new();
    let mut report = IcsReport::default();
    let result = parse_ics_window_reported(
        &ics_document(
            "UID:t\r\nDTSTART:20260814T090000Z\r\nDURATION:PT9223372036854775S\r\nSUMMARY:t",
        ),
        "UTC",
        Some((
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(),
        )),
        &mut AdmissionMeter::isolated(AdmissionBudget::default()),
        &mut out,
        || Ok(()),
        &mut report,
    );

    assert!(result.is_ok());
    assert!(out.is_empty());
    assert!(report.diagnostics().contains(&IcsDiagnostic::InvalidEvent));
}

#[test]
fn extreme_durations_are_quarantined_not_admitted() {
    for duration in ["P15000000W", "PT9223372036854775807S"] {
        let mut out = Vec::new();
        let mut report = IcsReport::default();
        let result = parse_ics_window_reported(
            &ics_document(&format!(
                "UID:t\r\nDTSTART:20260814T090000Z\r\nDURATION:{duration}\r\nSUMMARY:t"
            )),
            "UTC",
            None,
            &mut AdmissionMeter::isolated(AdmissionBudget::default()),
            &mut out,
            || Ok(()),
            &mut report,
        );

        assert!(result.is_ok(), "duration {duration}");
        assert!(out.is_empty(), "duration {duration}");
        assert!(
            report.diagnostics().contains(&IcsDiagnostic::InvalidEvent),
            "duration {duration}"
        );
    }
}
