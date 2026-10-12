use super::*;
use thegn_core::calendar::{EventTime, TzRef};
use thegn_core::store::CalendarStore;

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
