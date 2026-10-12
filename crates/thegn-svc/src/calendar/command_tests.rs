use super::*;
use thegn_core::calendar::admission::{AdmissionBudget, AdmissionMeter};

fn sink() -> AdmittingSink {
    AdmittingSink {
        admitted: Admitted {
            meter: AdmissionMeter::isolated(AdmissionBudget::default()),
            events: Vec::new(),
            deleted: Vec::new(),
            sync_token: String::new(),
            mode: None,
            refusal: None,
            invalid_event: None,
        },
        granted: Vec::new(),
        junk: Vec::new(),
        error: None,
    }
}

#[test]
fn plugin_event_with_empty_uid_is_refused_before_admission() {
    let mut sink = sink();
    let line = r#"{"method":"events","params":{"events":[{"uid":"","title":"meeting","start":{"kind":"date","date":"2026-10-12"},"end":{"kind":"date","date":"2026-10-13"}}]}}"#;

    assert!(matches!(
        sink.events_line(line),
        Err(CalendarError::Parse(message)) if message.contains("invalid event (element 0: empty UID)")
    ));
    assert!(sink.admitted.events.is_empty());
}

#[test]
fn plugin_event_with_mixed_date_and_timed_end_is_refused() {
    let mut sink = sink();
    let line = r#"{"method":"events","params":{"events":[{"uid":"meeting","title":"meeting","start":{"kind":"date","date":"2026-10-12"},"end":{"kind":"zoned","local":"2026-10-12T10:00:00","zone":"UTC"}}]}}"#;

    assert!(matches!(
        sink.events_line(line),
        Err(CalendarError::Parse(message)) if message.contains("invalid event (element 0: mixed all-day and timed endpoints)")
    ));
    assert!(sink.admitted.events.is_empty());
}
