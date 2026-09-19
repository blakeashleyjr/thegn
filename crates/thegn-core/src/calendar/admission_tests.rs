use super::*;
use crate::calendar::{CalEvent, EventTime, Recurrence, Reminder, TzRef};
use chrono::NaiveDate;

fn budget(n: usize) -> AdmissionBudget {
    AdmissionBudget::new(n).unwrap()
}

fn event(uid: &str) -> CalEvent {
    let day = NaiveDate::from_ymd_opt(2026, 8, 21).unwrap();
    CalEvent::new(
        uid,
        uid,
        EventTime::Zoned {
            local: day.and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
        EventTime::Zoned {
            local: day.and_hms_opt(10, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
    )
}

#[test]
fn zero_is_rejected_and_never_unlimited() {
    assert_eq!(
        AdmissionBudget::new(0),
        Err(AdmissionConfigError { value: 0 })
    );
    let (b, problem) = AdmissionBudget::clamped(0);
    assert_eq!(
        b.max_events(),
        DEFAULT_MAX_EVENTS,
        "a legacy zero runs as the default budget, never unlimited"
    );
    assert!(
        problem
            .unwrap()
            .to_string()
            .contains("0 does not mean unlimited")
    );
}

#[test]
fn the_supported_range_is_inclusive_at_both_ends() {
    assert_eq!(budget(MIN_MAX_EVENTS).max_events(), 1);
    assert_eq!(budget(MAX_MAX_EVENTS).max_events(), MAX_MAX_EVENTS);
    assert!(AdmissionBudget::new(MAX_MAX_EVENTS + 1).is_err());
    let (b, problem) = AdmissionBudget::clamped(usize::MAX);
    assert_eq!(b.max_events(), MAX_MAX_EVENTS);
    assert!(problem.is_some());
    let (b, problem) = AdmissionBudget::clamped(42);
    assert_eq!(b.max_events(), 42);
    assert!(problem.is_none());
    assert_eq!(AdmissionBudget::default().max_events(), DEFAULT_MAX_EVENTS);
}

#[test]
fn records_are_admitted_up_to_exactly_the_budget() {
    let mut m = AdmissionMeter::isolated(budget(2));
    for _ in 0..2 {
        m.begin_event();
        m.admit_event().unwrap();
    }
    m.begin_event();
    assert_eq!(
        m.admit_event(),
        Err(AdmissionError::new(AdmissionLimit::AccountRecords))
    );
    assert_eq!(m.records(), 2);
    assert!(!m.has_record_room());
}

#[test]
fn deletions_share_the_record_budget() {
    let mut m = AdmissionMeter::isolated(budget(3));
    m.begin_event();
    m.admit_event().unwrap();
    m.admit_deletion(4).unwrap();
    m.admit_deletion(4).unwrap();
    assert_eq!(
        m.admit_deletion(4),
        Err(AdmissionError::new(AdmissionLimit::AccountRecords))
    );
}

#[test]
fn per_event_bytes_and_children_are_exact() {
    let mut m = AdmissionMeter::isolated(budget(10));
    m.begin_event();
    m.charge_event(MAX_EVENT_BYTES, MAX_EVENT_CHILDREN).unwrap();
    assert_eq!(
        m.charge_event(1, 0),
        Err(AdmissionError::new(AdmissionLimit::EventBytes))
    );
    assert_eq!(
        m.charge_event(0, 1),
        Err(AdmissionError::new(AdmissionLimit::EventChildren))
    );
    // A new event starts from zero, and the abandoned one is given back.
    m.begin_event();
    assert_eq!(m.retained_bytes(), 0);
    m.charge_event(10, 1).unwrap();
    assert_eq!(m.retained_bytes(), 10);
}

#[test]
fn the_account_byte_budget_scales_with_max_events() {
    assert_eq!(budget(1).account_bytes(), MIN_ACCOUNT_BYTES);
    assert_eq!(
        budget(DEFAULT_MAX_EVENTS).account_bytes(),
        MIN_ACCOUNT_BYTES
    );
    assert_eq!(
        budget(8_000).account_bytes(),
        8_000 * ACCOUNT_BYTES_PER_EVENT
    );
    assert!(budget(8_000).account_bytes() > MIN_ACCOUNT_BYTES);
    assert!(budget(MAX_MAX_EVENTS).account_bytes() <= MAX_ACCOUNT_BYTES);
    // One maximal account plus one in-flight body fits the global pool.
    const { assert!(MAX_ACCOUNT_BYTES + MAX_SOURCE_DOCUMENT_BYTES <= GLOBAL_MAX_BYTES) };
}

#[test]
fn per_account_bytes_are_bounded_across_events() {
    let b = budget(MAX_MAX_EVENTS);
    let mut m = AdmissionMeter::isolated(b);
    let per = MAX_EVENT_BYTES;
    let full = b.account_bytes() / per;
    for _ in 0..full {
        m.begin_event();
        m.charge_event(per, 0).unwrap();
        m.admit_event().unwrap();
    }
    m.begin_event();
    let rest = b.account_bytes() - full * per;
    m.charge_event(rest, 0).unwrap();
    assert_eq!(m.retained_bytes(), b.account_bytes());
    assert_eq!(
        m.charge_event(1, 0),
        Err(AdmissionError::new(AdmissionLimit::AccountBytes))
    );
}

#[test]
fn the_global_pool_is_shared_and_refuses_without_waiting() {
    let pool = AdmissionPool::new(3, 1_000);
    let mut a = AdmissionMeter::new(budget(10), pool.clone());
    let mut b = AdmissionMeter::new(budget(10), pool.clone());
    for _ in 0..2 {
        a.begin_event();
        a.admit_event().unwrap();
    }
    b.begin_event();
    b.admit_event().unwrap();
    // The fourth record in the process is refused, whichever meter asks.
    a.begin_event();
    let e = a.admit_event().unwrap_err();
    assert_eq!(e.limit, AdmissionLimit::GlobalRecords);
    assert!(e.is_contention());
    assert_eq!(pool.in_use().0, 3);

    // Bytes too, and a refusal leaves no partial reservation behind.
    b.begin_event();
    b.charge_event(900, 0).unwrap();
    assert_eq!(
        a.charge_event(200, 0),
        Err(AdmissionError::new(AdmissionLimit::GlobalBytes))
    );
    assert_eq!(pool.in_use(), (3, 900));
}

#[test]
fn a_lease_releases_exactly_what_it_holds_on_drop() {
    let pool = AdmissionPool::new(100, 10_000);
    let mut m = AdmissionMeter::new(budget(10), pool.clone());
    m.reserve_transient(5_000).unwrap();
    m.begin_event();
    m.charge_event(100, 1).unwrap();
    m.admit_event().unwrap();
    m.admit_deletion(10).unwrap();
    assert_eq!(pool.in_use(), (2, 5_000 + 100 + 10 + ENTRY_OVERHEAD));

    // Sealing drops the transient document but keeps what is retained.
    let lease = m.into_lease();
    assert_eq!(pool.in_use(), (2, 100 + 10 + ENTRY_OVERHEAD));
    assert_eq!(lease.records(), 2);
    assert_eq!(lease.bytes(), 100 + 10 + ENTRY_OVERHEAD);
    drop(lease);
    assert_eq!(pool.in_use(), (0, 0));
}

#[test]
fn an_unfinished_event_is_released_when_sealing() {
    let pool = AdmissionPool::new(100, 10_000);
    let mut m = AdmissionMeter::new(budget(10), pool.clone());
    m.begin_event();
    m.charge_event(500, 0).unwrap();
    let lease = m.into_lease();
    assert_eq!(lease.bytes(), 0);
    assert_eq!(pool.in_use(), (0, 0));
}

#[test]
fn transient_documents_have_their_own_ceiling() {
    let mut m = AdmissionMeter::isolated(budget(1));
    assert_eq!(
        m.reserve_transient(MAX_SOURCE_DOCUMENT_BYTES + 1),
        Err(AdmissionError::new(AdmissionLimit::DocumentBytes))
    );
    m.reserve_transient(MAX_SOURCE_DOCUMENT_BYTES).unwrap();
    assert_eq!(m.transient_bytes(), MAX_SOURCE_DOCUMENT_BYTES);
    // Transient bytes don't consume the account's retained budget.
    assert_eq!(m.retained_bytes(), 0);
    m.release_transient(usize::MAX);
    assert_eq!(m.transient_bytes(), 0);
}

#[test]
fn rollback_returns_to_a_checkpoint() {
    let pool = AdmissionPool::new(100, 10_000);
    let mut m = AdmissionMeter::new(budget(10), pool.clone());
    m.admit_deletion(1).unwrap();
    let cp = m.checkpoint();
    m.admit_materialized(&event("a")).unwrap();
    m.admit_deletion(1).unwrap();
    m.rollback(cp);
    assert_eq!(m.records(), 1);
    assert_eq!(pool.in_use(), (1, 1 + ENTRY_OVERHEAD));
}

#[test]
fn an_empty_lease_accepts_nothing_but_nothing() {
    let mut l = AdmissionLease::empty();
    l.reserve(0, 0).unwrap();
    assert_eq!(
        l.reserve(0, 1),
        Err(AdmissionError::new(AdmissionLimit::GlobalBytes))
    );
}

#[test]
fn the_footprint_counts_every_retained_part() {
    let small = event("a");
    let (base, children) = event_footprint(&small).unwrap();
    assert_eq!(children, 0);

    let mut big = event("a");
    big.description = "x".repeat(1000);
    big.extra.insert("X-K".into(), "v".repeat(10));
    big.reminders.push(Reminder { minutes_before: 5 });
    big.recurrence = Some(Recurrence {
        rules: vec![crate::calendar::RRule::parse("FREQ=WEEKLY;BYDAY=MO,TU").unwrap()],
        rdates: vec![EventTime::Zoned {
            local: NaiveDate::from_ymd_opt(2026, 9, 1)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            zone: TzRef::new("Europe/Berlin"),
        }],
        exdates: Vec::new(),
    });
    let (bytes, children) = event_footprint(&big).unwrap();
    assert!(bytes >= base + 1000 + 3 + 10 + "Europe/Berlin".len());
    // extra + reminder + rule + 2 BYDAY + rdate
    assert_eq!(children, 6);

    // A decoded event over the per-event budget is refused.
    let mut huge = event("h");
    huge.description = "x".repeat(MAX_EVENT_BYTES);
    let mut m = AdmissionMeter::isolated(budget(10));
    assert_eq!(
        m.admit_materialized(&huge),
        Err(AdmissionError::new(AdmissionLimit::EventBytes))
    );
}

#[test]
fn errors_are_value_free_and_distinguish_contention() {
    for limit in [
        AdmissionLimit::AccountRecords,
        AdmissionLimit::AccountBytes,
        AdmissionLimit::EventBytes,
        AdmissionLimit::EventChildren,
        AdmissionLimit::LineBytes,
        AdmissionLimit::Nesting,
        AdmissionLimit::DocumentBytes,
        AdmissionLimit::Messages,
        AdmissionLimit::GlobalRecords,
        AdmissionLimit::GlobalBytes,
        AdmissionLimit::Arithmetic,
    ] {
        let e = AdmissionError::new(limit);
        assert!(!e.to_string().is_empty());
        assert_eq!(
            e.is_account_limit(),
            matches!(
                limit,
                AdmissionLimit::AccountRecords | AdmissionLimit::AccountBytes
            )
        );
        assert_eq!(
            e.is_contention(),
            matches!(
                limit,
                AdmissionLimit::GlobalRecords | AdmissionLimit::GlobalBytes
            )
        );
    }
    assert!(
        AdmissionError::new(AdmissionLimit::AccountRecords)
            .to_string()
            .contains("raise [calendar] max_events")
    );
}

#[test]
fn the_global_pool_is_one_process_wide_instance() {
    let a = AdmissionPool::global();
    let b = AdmissionPool::global();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
}
