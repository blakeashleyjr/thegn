use super::*;
use thegn_core::calendar::conformance::{FIXTURES, Verdict};
use thegn_core::calendar::{EventTime, TzRef};
use thegn_core::store::CalendarStore;

#[test]
fn conformance_cache_decode_matches_every_fixture() {
    let mut table = Vec::new();
    for fixture in FIXTURES {
        if let Some((json, expected)) = fixture.json {
            let event = serde_json::from_str::<CalEvent>(json).unwrap();
            let db = Db::open_memory().unwrap();
            let start = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
            let end = NaiveDate::from_ymd_opt(2026, 8, 20).unwrap();
            let mut row = row_of(&event, None);
            if row.start_ms == 0 && row.end_ms == 0 {
                let index_time = start
                    .and_hms_opt(9, 0, 0)
                    .unwrap()
                    .and_utc()
                    .timestamp_millis();
                row.start_ms = index_time;
                row.end_ms = index_time;
            }
            db.replace_calendar_account("conformance", &[row]).unwrap();
            let cached = load_cached_window(
                &db,
                CalendarWindow::new(start, end, chrono_tz::UTC).unwrap(),
            )
            .unwrap();
            let actual = if cached.events.len() == 1 && cached.skipped == 0 {
                Verdict::Admit
            } else {
                Verdict::Reject
            };
            let shape_ok = match expected {
                Verdict::Admit => cached.events.len() == 1 && cached.skipped == 0,
                Verdict::Reject => cached.events.is_empty() && cached.skipped == 1,
            };
            table.push((
                fixture.name,
                "host cache decode",
                expected,
                actual,
                shape_ok,
            ));
        }
    }
    tracing::info!(?table, "calendar conformance host cache table");
    assert!(
        table
            .iter()
            .all(|(_, _, expected, actual, shape_ok)| expected == actual && *shape_ok),
        "calendar conformance disagreement: {table:#?}"
    );
}

#[test]
fn cache_load_skips_decodable_events_that_fail_domain_validation() {
    let db = Db::open_memory().unwrap();
    let start = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
    let end = start.succ_opt().unwrap();
    let valid = CalEvent::new(
        "valid",
        "meeting",
        EventTime::Date { date: start },
        EventTime::Date { date: end },
    );
    let invalid = CalEvent::new(
        "  ",
        "invalid",
        EventTime::Zoned {
            local: start.and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
        EventTime::Zoned {
            local: start.and_hms_opt(10, 0, 0).unwrap(),
            zone: TzRef::new("UTC"),
        },
    );
    let rows = [row_of(&valid, None), row_of(&invalid, None)];
    db.replace_calendar_account("test", &rows).unwrap();

    let cached = load_cached_window(
        &db,
        CalendarWindow::new(start, end, chrono_tz::UTC).unwrap(),
    )
    .unwrap();
    assert_eq!(cached.events.len(), 1);
    assert_eq!(cached.events[0].uid, "valid");
    assert_eq!(cached.skipped, 1);
}
