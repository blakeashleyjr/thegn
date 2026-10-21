//! Shared admission fixtures for the calendar parser and provider seams.
//!
//! Each fixture records the expected verdict independently for its iCalendar
//! and plugin JSON representation. The same table is consumed by core, svc,
//! and host conformance tests.

/// Expected result when an event reaches a calendar admission seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The event is retained by the seam.
    Admit,
    /// The event is rejected or quarantined by the seam.
    Reject,
}

/// One event represented in iCalendar and/or plugin JSON form.
#[derive(Debug, Clone, Copy)]
pub struct Fixture {
    /// Stable fixture name, also used in test verdict reports.
    pub name: &'static str,
    /// VEVENT properties, without the surrounding component markers.
    pub ics: Option<(&'static str, Verdict)>,
    /// One serialized `CalEvent` object and its expected verdict.
    pub json: Option<(&'static str, Verdict)>,
}

/// Events shared across every calendar admission seam.
pub const FIXTURES: &[Fixture] = &[
    Fixture {
        name: "valid_timed",
        ics: Some((
            "UID:matrix-timed\r\nDTSTART:20260814T090000Z\r\nDTEND:20260814T100000Z\r\nSUMMARY:Timed",
            Verdict::Admit,
        )),
        json: Some((
            r#"{"uid":"matrix-timed","title":"Timed","start":{"kind":"instant","at":"2026-08-14T09:00:00Z"},"end":{"kind":"instant","at":"2026-08-14T10:00:00Z"}}"#,
            Verdict::Admit,
        )),
    },
    Fixture {
        name: "valid_all_day",
        ics: Some((
            "UID:matrix-all-day\r\nDTSTART;VALUE=DATE:20260814\r\nDTEND;VALUE=DATE:20260815\r\nSUMMARY:All day",
            Verdict::Admit,
        )),
        json: Some((
            r#"{"uid":"matrix-all-day","title":"All day","start":{"kind":"date","date":"2026-08-14"},"end":{"kind":"date","date":"2026-08-15"}}"#,
            Verdict::Admit,
        )),
    },
    Fixture {
        name: "valid_zero_duration",
        ics: Some((
            "UID:matrix-zero\r\nDTSTART:20260814T090000Z\r\nDTEND:20260814T090000Z\r\nSUMMARY:Zero",
            Verdict::Admit,
        )),
        json: Some((
            r#"{"uid":"matrix-zero","title":"Zero","start":{"kind":"instant","at":"2026-08-14T09:00:00Z"},"end":{"kind":"instant","at":"2026-08-14T09:00:00Z"}}"#,
            Verdict::Admit,
        )),
    },
    Fixture {
        name: "valid_cross_zone",
        ics: Some((
            "UID:matrix-cross-zone\r\nDTSTART;TZID=Europe/London:20260814T090000\r\nDTEND;TZID=America/New_York:20260814T100000\r\nSUMMARY:Cross zone",
            Verdict::Admit,
        )),
        json: Some((
            r#"{"uid":"matrix-cross-zone","title":"Cross zone","start":{"kind":"zoned","local":"2026-08-14T09:00:00","zone":"Europe/London"},"end":{"kind":"zoned","local":"2026-08-14T10:00:00","zone":"America/New_York"}}"#,
            Verdict::Admit,
        )),
    },
    Fixture {
        name: "reversed_end",
        ics: Some((
            "UID:matrix-reversed\r\nDTSTART:20260814T100000Z\r\nDTEND:20260814T090000Z\r\nSUMMARY:Reversed",
            Verdict::Reject,
        )),
        json: Some((
            r#"{"uid":"matrix-reversed","title":"Reversed","start":{"kind":"instant","at":"2026-08-14T10:00:00Z"},"end":{"kind":"instant","at":"2026-08-14T09:00:00Z"}}"#,
            Verdict::Reject,
        )),
    },
    Fixture {
        name: "mixed_shape",
        ics: Some((
            "UID:matrix-mixed\r\nDTSTART;VALUE=DATE:20260814\r\nDTEND:20260814T100000Z\r\nSUMMARY:Mixed",
            Verdict::Reject,
        )),
        json: Some((
            r#"{"uid":"matrix-mixed","title":"Mixed","start":{"kind":"date","date":"2026-08-14"},"end":{"kind":"instant","at":"2026-08-14T10:00:00Z"}}"#,
            Verdict::Reject,
        )),
    },
    Fixture {
        name: "unknown_zone",
        ics: Some((
            "UID:matrix-unknown-zone\r\nDTSTART;TZID=Mars/Olympus:20260814T090000\r\nDTEND;TZID=Mars/Olympus:20260814T100000\r\nSUMMARY:Unknown zone",
            Verdict::Reject,
        )),
        json: Some((
            r#"{"uid":"matrix-unknown-zone","title":"Unknown zone","start":{"kind":"zoned","local":"2026-08-14T09:00:00","zone":"Mars/Olympus"},"end":{"kind":"zoned","local":"2026-08-14T10:00:00","zone":"Mars/Olympus"}}"#,
            Verdict::Reject,
        )),
    },
    Fixture {
        name: "dtend_and_duration",
        ics: Some((
            "UID:matrix-end-duration\r\nDTSTART:20260814T090000Z\r\nDTEND:20260814T100000Z\r\nDURATION:PT1H\r\nSUMMARY:Duplicate end",
            Verdict::Reject,
        )),
        json: None,
    },
    Fixture {
        name: "incompatible_recurrence",
        ics: Some((
            "UID:matrix-recurrence\r\nDTSTART;VALUE=DATE:20260814\r\nDTEND;VALUE=DATE:20260815\r\nRDATE:20260815T090000Z\r\nSUMMARY:Bad recurrence",
            Verdict::Reject,
        )),
        json: Some((
            r#"{"uid":"matrix-recurrence","title":"Bad recurrence","start":{"kind":"date","date":"2026-08-14"},"end":{"kind":"date","date":"2026-08-15"},"recurrence":{"rdates":[{"kind":"instant","at":"2026-08-15T09:00:00Z"}]}}"#,
            Verdict::Reject,
        )),
    },
    Fixture {
        name: "empty_uid",
        ics: Some((
            "DTSTART:20260814T090000Z\r\nDTEND:20260814T100000Z\r\nSUMMARY:No UID",
            Verdict::Admit,
        )),
        json: Some((
            r#"{"uid":"","title":"Empty UID","start":{"kind":"instant","at":"2026-08-14T09:00:00Z"},"end":{"kind":"instant","at":"2026-08-14T10:00:00Z"}}"#,
            Verdict::Reject,
        )),
    },
];

/// Wrap VEVENT properties in a minimal complete calendar document.
pub fn ics_document(properties: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\n{properties}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

#[cfg(test)]
#[path = "conformance_tests.rs"]
mod tests;
