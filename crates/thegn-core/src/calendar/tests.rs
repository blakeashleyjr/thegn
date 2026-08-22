use super::*;
use chrono::{Datelike, NaiveDate, TimeZone, Timelike, Weekday};
use chrono_tz::Tz;

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn utc(y: i32, m: u32, day: u32, h: u32, min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, day, h, min, 0).unwrap()
}

// --- grid -------------------------------------------------------------------

#[test]
fn month_starting_exactly_on_week_start_still_pads_to_six_rows() {
    // Sep 2025 starts on a Monday, so with Monday-start it needs no leading
    // days and only 5 natural rows. Fixed-six-weeks must pad it anyway: a popup
    // that changes height as you page months is geometry damage.
    let g = MonthGrid::build(2025, 9, Weekday::Mon, d(2025, 9, 15), true).unwrap();
    assert_eq!(g.weeks.len(), 6);
    assert_eq!(g.weeks[0][0].date, d(2025, 9, 1));
    assert!(g.weeks[0][0].in_month, "no leading days to borrow");

    let natural = MonthGrid::build(2025, 9, Weekday::Mon, d(2025, 9, 15), false).unwrap();
    assert_eq!(natural.weeks.len(), 5, "5 rows without padding");
}

#[test]
fn leap_february_lays_out_correctly() {
    let g = MonthGrid::build(2028, 2, Weekday::Mon, d(2028, 2, 1), true).unwrap();
    let in_month: Vec<_> = g.cells().filter(|c| c.in_month).map(|c| c.date).collect();
    assert_eq!(in_month.len(), 29, "2028 is a leap year");
    assert_eq!(*in_month.last().unwrap(), d(2028, 2, 29));
    assert_eq!(grid::days_in_month(2028, 2), Some(29));
    assert_eq!(grid::days_in_month(2027, 2), Some(28));
    // Centurial rules: 1900 is not a leap year, 2000 is.
    assert_eq!(grid::days_in_month(1900, 2), Some(28));
    assert_eq!(grid::days_in_month(2000, 2), Some(29));
}

#[test]
fn a_month_that_naturally_needs_six_rows_is_unpadded() {
    // Aug 2026 starts on a Saturday; Monday-start needs 5 leading days, so
    // 5 + 31 = 36 cells => 6 rows with no padding at all.
    let g = MonthGrid::build(2026, 8, Weekday::Mon, d(2026, 8, 21), false).unwrap();
    assert_eq!(g.weeks.len(), 6);
    assert_eq!(g.weeks[0][0].date, d(2026, 7, 27));
    assert!(!g.weeks[0][0].in_month, "leading day from July");
}

#[test]
fn grid_spans_the_year_boundary_in_both_directions() {
    // December's trailing cells belong to next January...
    let dec = MonthGrid::build(2026, 12, Weekday::Mon, d(2026, 12, 1), true).unwrap();
    let (_, last) = dec.span();
    assert_eq!(last.year(), 2027);
    // ...and January's leading cells to the previous December.
    let jan = MonthGrid::build(2027, 1, Weekday::Mon, d(2027, 1, 1), true).unwrap();
    let (first, _) = jan.span();
    assert_eq!(first.year(), 2026);
}

#[test]
fn every_week_start_rotates_the_grid_and_the_headers_together() {
    for (start, want_first_col) in [
        (Weekday::Mon, Weekday::Mon),
        (Weekday::Sun, Weekday::Sun),
        (Weekday::Sat, Weekday::Sat),
    ] {
        let g = MonthGrid::build(2026, 8, start, d(2026, 8, 21), true).unwrap();
        for w in &g.weeks {
            assert_eq!(w[0].weekday, want_first_col, "row starts on {start:?}");
        }
        let h = weekday_headers(start, WeekdayStyle::Two);
        let want = match start {
            Weekday::Mon => "Mo",
            Weekday::Sun => "Su",
            _ => "Sa",
        };
        assert_eq!(h[0], want);
        assert_eq!(h.len(), 7);
    }
}

#[test]
fn iso_week_numbers_are_not_hand_rolled_across_the_new_year() {
    // 2027-01-01 is a Friday, which ISO-8601 puts in week 53 of 2026 — the
    // exact case a naive "day-of-year / 7" would get wrong.
    let g = MonthGrid::build(2027, 1, Weekday::Mon, d(2027, 1, 1), true).unwrap();
    let jan1 = g.cells().find(|c| c.date == d(2027, 1, 1)).unwrap();
    assert_eq!(jan1.iso_week, 53);
    assert_eq!(d(2027, 1, 4).iso_week().week(), 1, "the following Monday");
}

#[test]
fn today_is_flagged_and_position_round_trips() {
    let today = d(2026, 8, 21);
    let g = MonthGrid::build(2026, 8, Weekday::Mon, today, true).unwrap();
    assert_eq!(g.cells().filter(|c| c.is_today).count(), 1);
    let (r, c) = g.position(today).unwrap();
    assert_eq!(g.weeks[r][c].date, today);
    assert!(g.position(d(2030, 1, 1)).is_none());
}

#[test]
fn week_numbers_has_one_entry_per_row() {
    let g = MonthGrid::build(2026, 8, Weekday::Mon, d(2026, 8, 21), true).unwrap();
    assert_eq!(g.week_numbers().len(), g.weeks.len());
}

#[test]
fn grid_build_rejects_an_impossible_month() {
    assert!(MonthGrid::build(2026, 13, Weekday::Mon, d(2026, 1, 1), true).is_none());
    assert!(MonthGrid::build(2026, 0, Weekday::Mon, d(2026, 1, 1), true).is_none());
}

#[test]
fn month_arithmetic_rolls_the_year() {
    assert_eq!(grid::next_month(2026, 12), Some((2027, 1)));
    assert_eq!(grid::prev_month(2026, 1), Some((2025, 12)));
    assert_eq!(grid::next_month(2026, 5), Some((2026, 6)));
    assert_eq!(grid::prev_month(2026, 5), Some((2026, 4)));
    assert_eq!(grid::next_month(2026, 13), None);
    assert_eq!(grid::prev_month(2026, 0), None);
    assert_eq!(month_bounds(2026, 2), Some((d(2026, 2, 1), d(2026, 2, 28))));
}

#[test]
fn weekday_header_styles_have_the_expected_widths() {
    assert_eq!(weekday_headers(Weekday::Mon, WeekdayStyle::One)[0], "M");
    assert_eq!(weekday_headers(Weekday::Mon, WeekdayStyle::Two)[0], "Mo");
    assert_eq!(weekday_headers(Weekday::Mon, WeekdayStyle::Three)[0], "Mon");
    assert_eq!(weekday_headers(Weekday::Sun, WeekdayStyle::Three)[6], "Sat");
}

// --- cursor -----------------------------------------------------------------

#[test]
fn paging_months_from_the_31st_remembers_the_31st() {
    // THE case every naive implementation gets wrong. Jan 31 → Feb clamps to
    // 28, but the *intent* was "the 31st", so paging on to March must land on
    // Mar 31 rather than carrying the clamp forward to Mar 28.
    let today = d(2026, 1, 31);
    let mut c = CalCursor::new(today);
    c.apply(CalNav::NextMonth, today);
    assert_eq!(c.selected(), d(2026, 2, 28), "clamped to February's length");
    c.apply(CalNav::NextMonth, today);
    assert_eq!(c.selected(), d(2026, 3, 31), "the clamp was temporary");
}

#[test]
fn paging_into_a_leap_february_clamps_to_29() {
    let today = d(2028, 1, 31);
    let mut c = CalCursor::new(today);
    c.apply(CalNav::NextMonth, today);
    assert_eq!(c.selected(), d(2028, 2, 29));
}

#[test]
fn choosing_a_day_re_anchors_the_sticky_day_of_month() {
    // Stepping onto Feb 1 is an explicit choice of the 1st, so subsequent month
    // paging must track the 1st, not the old 31st.
    let today = d(2026, 1, 31);
    let mut c = CalCursor::new(today);
    c.apply(CalNav::NextDay, today);
    assert_eq!(c.selected(), d(2026, 2, 1));
    c.apply(CalNav::NextMonth, today);
    assert_eq!(c.selected(), d(2026, 3, 1));
}

#[test]
fn day_and_week_steps_drag_the_visible_month_along() {
    let today = d(2026, 8, 31);
    let mut c = CalCursor::new(today);
    assert!(c.apply(CalNav::NextDay, today));
    assert_eq!(c.selected(), d(2026, 9, 1));
    assert_eq!(c.visible_month(), (2026, 9), "view follows the selection");

    let today = d(2026, 8, 3);
    let mut c = CalCursor::new(today);
    c.apply(CalNav::PrevWeek, today);
    assert_eq!(c.selected(), d(2026, 7, 27));
    assert_eq!(c.visible_month(), (2026, 7));

    let mut c = CalCursor::new(d(2026, 8, 10));
    c.apply(CalNav::NextWeek, d(2026, 8, 10));
    assert_eq!(c.selected(), d(2026, 8, 17));
    c.apply(CalNav::PrevDay, d(2026, 8, 10));
    assert_eq!(c.selected(), d(2026, 8, 16));
}

#[test]
fn year_paging_handles_feb_29() {
    let today = d(2024, 2, 29);
    let mut c = CalCursor::new(today);
    c.apply(CalNav::NextYear, today);
    assert_eq!(c.selected(), d(2025, 2, 28), "2025 is not a leap year");
    // And the sticky anchor restores the 29th on the next leap year.
    c.apply(CalNav::NextYear, today);
    c.apply(CalNav::NextYear, today);
    c.apply(CalNav::NextYear, today);
    assert_eq!(c.selected(), d(2028, 2, 29));
}

#[test]
fn month_paging_rolls_the_year_in_both_directions() {
    let today = d(2026, 12, 15);
    let mut c = CalCursor::new(today);
    c.apply(CalNav::NextMonth, today);
    assert_eq!(c.visible_month(), (2027, 1));
    c.apply(CalNav::PrevMonth, today);
    c.apply(CalNav::PrevMonth, today);
    assert_eq!(c.visible_month(), (2026, 11));
    c.apply(CalNav::PrevYear, today);
    assert_eq!(c.visible_month(), (2025, 11));
}

#[test]
fn today_and_goto_jump_and_report_whether_anything_moved() {
    let today = d(2026, 8, 21);
    let mut c = CalCursor::new(d(2026, 3, 4));
    assert!(c.apply(CalNav::Today, today));
    assert_eq!(c.selected(), today);
    assert_eq!(c.visible_month(), (2026, 8));
    // A no-op nav reports no change, so the caller can skip a repaint.
    assert!(!c.apply(CalNav::Today, today));
    assert!(c.apply(CalNav::Goto(d(2020, 1, 2)), today));
    assert_eq!(c.selected(), d(2020, 1, 2));
}

#[test]
fn first_and_last_of_month_respect_month_length() {
    let today = d(2026, 2, 15);
    let mut c = CalCursor::new(today);
    c.apply(CalNav::FirstOfMonth, today);
    assert_eq!(c.selected(), d(2026, 2, 1));
    c.apply(CalNav::LastOfMonth, today);
    assert_eq!(c.selected(), d(2026, 2, 28));
}

#[test]
fn visible_range_includes_the_borrowed_neighbour_days() {
    // An event on Jan 31 must appear in February's first cell, so the query
    // range has to cover the whole grid, not just the calendar month.
    let c = CalCursor::new(d(2026, 2, 10));
    let (from, to) = c.visible_range(Weekday::Mon, true).unwrap();
    assert!(from < d(2026, 2, 1), "leading days precede the 1st");
    assert!(to > d(2026, 2, 28), "trailing days follow the 28th");
    let g = c.grid(Weekday::Mon, d(2026, 2, 10), true).unwrap();
    assert_eq!(g.span(), (from, to));
}

// --- timezones --------------------------------------------------------------

#[test]
fn zone_lookup_is_case_insensitive_and_rejects_junk() {
    assert_eq!(
        resolve_zone("America/New_York"),
        Some(Tz::America__New_York)
    );
    assert_eq!(
        resolve_zone("america/new_york"),
        Some(Tz::America__New_York)
    );
    assert_eq!(resolve_zone("  UTC  "), Some(Tz::UTC));
    assert!(resolve_zone("Mars/Olympus_Mons").is_none());
    assert!(resolve_zone("").is_none());
}

#[test]
fn zone_suggestions_lead_with_the_case_fix() {
    let s = tz::suggest_zones("America/New_york", 5);
    assert_eq!(s.first(), Some(&"America/New_York"));
    // A bare city name still finds its zone — the region half is what people
    // most often get wrong.
    assert!(tz::suggest_zones("Tokyo", 5).contains(&"Asia/Tokyo"));
    assert!(tz::suggest_zones("", 5).is_empty());
    assert!(tz::suggest_zones("Tokyo", 0).is_empty());
    // A transposition has no substring overlap, so only the fuzzy fallback can
    // recover it.
    assert!(
        tz::suggest_zones("America/New_Yrok", 5).contains(&"America/New_York"),
        "fuzzy fallback should recover a transposed name"
    );
    assert!(tz::suggest_zones("Toyko", 5).contains(&"Asia/Tokyo"));
}

#[test]
fn world_clock_deltas_are_computed_at_the_instant_not_stored() {
    // Mid-January: New York is on EST (-5), London on GMT (+0).
    let clocks = vec![
        ResolvedClock {
            label: "nyc".into(),
            zone: Tz::America__New_York,
            format: String::new(),
            is_home: false,
        },
        ResolvedClock {
            label: "kathmandu".into(),
            zone: Tz::Asia__Kathmandu,
            format: String::new(),
            is_home: false,
        },
    ];
    let winter = read_clocks(&clocks, utc(2026, 1, 15, 12, 0), Tz::Europe__London);
    assert_eq!(winter[0].delta_from_home_mins, -5 * 60);
    assert_eq!(winter[0].abbrev, "EST");
    assert!(!winter[0].is_dst);
    // Nepal is +5:45 — a 45-minute offset no whole-hour model can express.
    assert_eq!(winter[1].delta_from_home_mins, 5 * 60 + 45);

    // Mid-July: New York is on EDT (-4) and London on BST (+1), so the delta
    // narrows to -5 only because both offsets are evaluated at `now`.
    let summer = read_clocks(&clocks, utc(2026, 7, 15, 12, 0), Tz::Europe__London);
    assert_eq!(summer[0].abbrev, "EDT");
    assert!(summer[0].is_dst);
    assert_eq!(summer[0].delta_from_home_mins, -5 * 60);
    assert_eq!(summer[0].utc_offset_secs, -4 * 3600);
}

#[test]
fn day_delta_marks_a_clock_on_a_different_calendar_date() {
    let clocks = vec![
        ResolvedClock {
            label: "tokyo".into(),
            zone: Tz::Asia__Tokyo,
            format: String::new(),
            is_home: false,
        },
        ResolvedClock {
            label: "la".into(),
            zone: Tz::America__Los_Angeles,
            format: String::new(),
            is_home: false,
        },
    ];
    // 22:00 UTC: Tokyo is already tomorrow, LA still today.
    let r = read_clocks(&clocks, utc(2026, 8, 21, 22, 0), Tz::UTC);
    assert_eq!(r[0].day_delta, 1);
    assert_eq!(r[1].day_delta, 0);
    // 02:00 UTC: LA is still yesterday.
    let r = read_clocks(&clocks, utc(2026, 8, 21, 2, 0), Tz::UTC);
    assert_eq!(r[1].day_delta, -1);
}

#[test]
fn an_empty_label_falls_back_to_the_zone_city() {
    let clocks = vec![ResolvedClock {
        label: String::new(),
        zone: Tz::America__New_York,
        format: String::new(),
        is_home: false,
    }];
    let r = read_clocks(&clocks, utc(2026, 1, 15, 12, 0), Tz::UTC);
    assert_eq!(r[0].label, "New York", "underscores become spaces");
    assert_eq!(ResolvedClock::label_from_zone(Tz::UTC), "UTC");
}

#[test]
fn a_zone_without_an_abbreviation_renders_a_numeric_offset() {
    // tzdb has no letter abbreviation for Kathmandu; it must not leak "+0545".
    let clocks = vec![ResolvedClock {
        label: "ktm".into(),
        zone: Tz::Asia__Kathmandu,
        format: String::new(),
        is_home: false,
    }];
    let r = read_clocks(&clocks, utc(2026, 1, 15, 12, 0), Tz::UTC);
    assert_eq!(r[0].abbrev, "+05:45");
}

#[test]
fn offset_and_delta_formatting() {
    assert_eq!(tz::fmt_offset(5 * 3600 + 45 * 60), "+05:45");
    assert_eq!(tz::fmt_offset(-5 * 3600), "-05:00");
    assert_eq!(tz::fmt_offset(0), "+00:00");
    assert_eq!(tz::fmt_delta(0), "", "no marker when there's no difference");
    assert_eq!(tz::fmt_delta(7 * 60), "+7h");
    assert_eq!(tz::fmt_delta(-6 * 60), "-6h");
    assert_eq!(tz::fmt_delta(5 * 60 + 30), "+5h30");
    assert_eq!(tz::fmt_delta(-30), "-30m");
}

#[test]
fn ambiguous_local_times_resolve_to_the_earlier_instant() {
    // 2026-11-01 01:30 America/New_York happens twice (EDT then EST). RFC 5545
    // and every mainstream client take the first.
    let local = d(2026, 11, 1).and_hms_opt(1, 30, 0).unwrap();
    let got = tz::resolve_local(local, Tz::America__New_York, GapPolicy::ShiftForward).unwrap();
    assert_eq!(got, utc(2026, 11, 1, 5, 30), "05:30Z is the EDT reading");
}

#[test]
fn nonexistent_local_times_follow_the_gap_policy() {
    // 2026-03-08 02:30 America/New_York does not exist — the clock jumps 02:00
    // straight to 03:00.
    let local = d(2026, 3, 8).and_hms_opt(2, 30, 0).unwrap();
    let z = Tz::America__New_York;
    let fwd = tz::resolve_local(local, z, GapPolicy::ShiftForward).unwrap();
    assert_eq!(fwd, utc(2026, 3, 8, 7, 30), "03:30 EDT");
    assert!(tz::resolve_local(local, z, GapPolicy::Skip).is_none());
    let back = tz::resolve_local(local, z, GapPolicy::Earliest).unwrap();
    assert_eq!(
        back,
        utc(2026, 3, 8, 6, 59),
        "01:59 EST, just before the gap"
    );

    // Distinct times inside the gap must stay distinct. Scanning for the first
    // valid instant instead would collapse all three onto 03:00 and fire three
    // separate events at the same moment.
    let mut seen = Vec::new();
    for minute in [15, 30, 45] {
        let l = d(2026, 3, 8).and_hms_opt(2, minute, 0).unwrap();
        seen.push(tz::resolve_local(l, z, GapPolicy::ShiftForward).unwrap());
    }
    assert_eq!(
        seen,
        vec![
            utc(2026, 3, 8, 7, 15),
            utc(2026, 3, 8, 7, 30),
            utc(2026, 3, 8, 7, 45)
        ],
        "each keeps its position within the hour"
    );
    // A perfectly ordinary time is unaffected by any policy.
    let plain = d(2026, 6, 1).and_hms_opt(9, 0, 0).unwrap();
    for p in [
        GapPolicy::ShiftForward,
        GapPolicy::Skip,
        GapPolicy::Earliest,
    ] {
        assert_eq!(
            tz::resolve_local(plain, z, p).unwrap(),
            utc(2026, 6, 1, 13, 0)
        );
    }
}

#[test]
fn tz_ref_round_trips_an_unknown_zone_instead_of_failing() {
    // A zone this build's tzdb doesn't know must survive a cache/plugin round
    // trip rather than poisoning the whole payload.
    let r = TzRef::new("Mars/Olympus_Mons");
    let json = serde_json::to_string(&r).unwrap();
    assert_eq!(json, "\"Mars/Olympus_Mons\"");
    let back: TzRef = serde_json::from_str(&json).unwrap();
    assert_eq!(back, r);
    assert!(back.resolve().is_none());
    assert_eq!(TzRef::new("UTC").resolve(), Some(Tz::UTC));
    assert_eq!(r.to_string(), "Mars/Olympus_Mons");
}

// --- event model ------------------------------------------------------------

fn zoned(y: i32, m: u32, day: u32, h: u32, min: u32, zone: &str) -> EventTime {
    EventTime::Zoned {
        local: d(y, m, day).and_hms_opt(h, min, 0).unwrap(),
        zone: TzRef::new(zone),
    }
}

#[test]
fn a_zoned_event_time_resolves_through_its_own_zone_not_home() {
    let t = zoned(2026, 1, 15, 9, 0, "America/Chicago");
    // Home is London, but the event names Chicago, so Chicago wins.
    assert_eq!(
        t.instant_in(Tz::Europe__London, GapPolicy::ShiftForward),
        Some(utc(2026, 1, 15, 15, 0))
    );
}

#[test]
fn a_wall_clock_event_keeps_its_local_time_across_dst() {
    // The whole reason EventTime::Zoned stores wall time: 09:00 Chicago is
    // 15:00Z in January and 14:00Z in July. Storing an instant would drift.
    let jan = zoned(2026, 1, 15, 9, 0, "America/Chicago");
    let jul = zoned(2026, 7, 15, 9, 0, "America/Chicago");
    let h = Tz::UTC;
    assert_eq!(
        jan.instant_in(h, GapPolicy::ShiftForward).unwrap().hour(),
        15
    );
    assert_eq!(
        jul.instant_in(h, GapPolicy::ShiftForward).unwrap().hour(),
        14
    );
}

#[test]
fn an_all_day_date_never_shifts_across_a_zone() {
    // Christmas is Dec 25 in Auckland and in Honolulu alike. Round-tripping a
    // floating date through an instant would move it by a day.
    let t = EventTime::Date {
        date: d(2026, 12, 25),
    };
    for home in [Tz::Pacific__Auckland, Tz::Pacific__Honolulu, Tz::UTC] {
        assert_eq!(t.date_in(home), Some(d(2026, 12, 25)));
    }
    assert!(t.is_all_day());
    assert!(!zoned(2026, 12, 25, 9, 0, "UTC").is_all_day());
}

#[test]
fn an_instant_event_time_buckets_into_the_viewers_local_date() {
    // 23:30Z on the 21st is already the 22nd in Tokyo.
    let t = EventTime::Instant {
        at: utc(2026, 8, 21, 23, 30),
    };
    assert_eq!(t.date_in(Tz::UTC), Some(d(2026, 8, 21)));
    assert_eq!(t.date_in(Tz::Asia__Tokyo), Some(d(2026, 8, 22)));
    assert_eq!(
        t.instant_in(Tz::UTC, GapPolicy::ShiftForward),
        Some(utc(2026, 8, 21, 23, 30))
    );
}

#[test]
fn an_unknown_event_zone_falls_back_to_home_rather_than_vanishing() {
    let t = zoned(2026, 6, 1, 12, 0, "Mars/Olympus_Mons");
    assert_eq!(
        t.instant_in(Tz::UTC, GapPolicy::ShiftForward),
        Some(utc(2026, 6, 1, 12, 0))
    );
}

#[test]
fn a_multi_day_event_marks_every_day_it_touches() {
    let e = CalEvent::new(
        "trip",
        "Conference",
        zoned(2026, 8, 20, 9, 0, "UTC"),
        zoned(2026, 8, 22, 17, 0, "UTC"),
    );
    assert_eq!(
        e.dates_in(Tz::UTC),
        vec![d(2026, 8, 20), d(2026, 8, 21), d(2026, 8, 22)]
    );
}

#[test]
fn an_all_day_events_exclusive_end_does_not_bleed_onto_the_next_day() {
    // RFC 5545: a one-day all-day event has DTEND on the FOLLOWING midnight.
    // Marking that day too would put a dot on a day the event doesn't occupy.
    let one = CalEvent::new(
        "x",
        "Holiday",
        EventTime::Date {
            date: d(2026, 8, 21),
        },
        EventTime::Date {
            date: d(2026, 8, 22),
        },
    );
    assert_eq!(one.dates_in(Tz::UTC), vec![d(2026, 8, 21)]);
    assert!(one.all_day());

    let two = CalEvent::new(
        "y",
        "Long weekend",
        EventTime::Date {
            date: d(2026, 8, 21),
        },
        EventTime::Date {
            date: d(2026, 8, 23),
        },
    );
    assert_eq!(two.dates_in(Tz::UTC), vec![d(2026, 8, 21), d(2026, 8, 22)]);

    // Same rule for a timed event landing exactly on midnight.
    let midnight = CalEvent::new(
        "z",
        "Overnight",
        zoned(2026, 8, 21, 22, 0, "UTC"),
        zoned(2026, 8, 22, 0, 0, "UTC"),
    );
    assert_eq!(midnight.dates_in(Tz::UTC), vec![d(2026, 8, 21)]);
}

#[test]
fn a_single_day_event_yields_exactly_that_day() {
    let e = CalEvent::new(
        "s",
        "Standup",
        zoned(2026, 8, 21, 9, 30, "UTC"),
        zoned(2026, 8, 21, 10, 0, "UTC"),
    );
    assert_eq!(e.dates_in(Tz::UTC), vec![d(2026, 8, 21)]);
    // An end before the start is malformed data, not a panic or an empty grid.
    let backwards = CalEvent::new(
        "b",
        "Bad",
        zoned(2026, 8, 21, 9, 0, "UTC"),
        zoned(2026, 8, 20, 9, 0, "UTC"),
    );
    assert_eq!(backwards.dates_in(Tz::UTC), vec![d(2026, 8, 21)]);
}

#[test]
fn event_ids_are_namespaced_by_source() {
    let mut e = CalEvent::new(
        "abc",
        "Thing",
        zoned(2026, 8, 21, 9, 0, "UTC"),
        zoned(2026, 8, 21, 10, 0, "UTC"),
    );
    e.source = SourceId("ics:work".into());
    assert_eq!(e.id().as_str(), "ics:work/abc");
    assert_eq!(e.id().to_string(), "ics:work/abc");
    // The same uid from two accounts must not collide.
    let mut other = e.clone();
    other.source = SourceId("ics:home".into());
    assert_ne!(e.id(), other.id());
}

#[test]
fn a_minimal_plugin_event_deserializes_and_unknown_fields_are_ignored() {
    // THE plugin-API contract: a four-field event is valid, and a newer plugin
    // sending extra keys must not break an older thegn.
    let json = r#"{
        "uid": "1",
        "title": "Standup",
        "start": {"kind":"zoned","local":"2026-08-21T09:30:00","zone":"UTC"},
        "end":   {"kind":"zoned","local":"2026-08-21T10:00:00","zone":"UTC"},
        "some_future_field": {"nested": true}
    }"#;
    let e: CalEvent = serde_json::from_str(json).unwrap();
    assert_eq!(e.title, "Standup");
    assert_eq!(e.status, EventStatus::Confirmed);
    assert_eq!(e.busy, Busy::Busy);
    assert!(e.reminders.is_empty());
    assert!(e.extra.is_empty());
    assert_eq!(e.dates_in(Tz::UTC), vec![d(2026, 8, 21)]);
}

#[test]
fn every_event_time_shape_round_trips_through_json() {
    for t in [
        EventTime::Date {
            date: d(2026, 8, 21),
        },
        zoned(2026, 8, 21, 9, 30, "America/New_York"),
        EventTime::Instant {
            at: utc(2026, 8, 21, 9, 30),
        },
    ] {
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<EventTime>(&s).unwrap(), t);
    }
}

#[test]
fn date_range_overlap_is_half_open() {
    let r = DateRange::new(utc(2026, 8, 1, 0, 0), utc(2026, 9, 1, 0, 0));
    // Touching the end is not an overlap, so adjacent months tile cleanly.
    assert!(!r.overlaps(utc(2026, 9, 1, 0, 0), utc(2026, 9, 2, 0, 0)));
    // Touching the start is not either.
    assert!(!r.overlaps(utc(2026, 7, 30, 0, 0), utc(2026, 8, 1, 0, 0)));
    assert!(r.overlaps(utc(2026, 8, 15, 0, 0), utc(2026, 8, 16, 0, 0)));
    // An event spanning the whole window counts.
    assert!(r.overlaps(utc(2026, 1, 1, 0, 0), utc(2027, 1, 1, 0, 0)));
}

// --- locale -----------------------------------------------------------------

#[test]
fn week_start_auto_follows_the_locale_region() {
    assert_eq!(resolve_week_start(None, Some("en_US.UTF-8")), Weekday::Sun);
    assert_eq!(resolve_week_start(None, Some("en_GB.UTF-8")), Weekday::Mon);
    assert_eq!(resolve_week_start(None, Some("de_DE")), Weekday::Mon);
    assert_eq!(resolve_week_start(None, Some("ar_EG")), Weekday::Sat);
    // Hyphen separators and modifiers parse the same way.
    assert_eq!(resolve_week_start(None, Some("en-US")), Weekday::Sun);
    assert_eq!(
        resolve_week_start(None, Some("ca_ES@valencia")),
        Weekday::Mon
    );
    // No usable signal falls back to ISO.
    for l in [None, Some("C"), Some("POSIX"), Some(""), Some("en")] {
        assert_eq!(resolve_week_start(None, l), Weekday::Mon);
    }
    // An explicit setting always wins over the locale.
    assert_eq!(
        resolve_week_start(Some(Weekday::Sat), Some("en_US.UTF-8")),
        Weekday::Sat
    );
}

#[test]
fn time_format_auto_follows_the_locale_region() {
    assert!(resolve_time_format(None, Some("en_US.UTF-8")));
    assert!(!resolve_time_format(None, Some("de_DE.UTF-8")));
    assert!(!resolve_time_format(None, Some("fr_FR")));
    assert!(!resolve_time_format(None, None));
    // Explicit wins.
    assert!(!resolve_time_format(Some(false), Some("en_US.UTF-8")));
    assert!(resolve_time_format(Some(true), Some("de_DE.UTF-8")));
}

// --- occurrences and day bucketing ------------------------------------------

#[test]
fn a_non_recurring_event_occurs_once_inside_the_window_and_not_outside() {
    let e = CalEvent::new(
        "one",
        "Kickoff",
        zoned(2026, 8, 21, 9, 0, "UTC"),
        zoned(2026, 8, 21, 10, 0, "UTC"),
    );
    let occ = e.occurrences(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC);
    assert_eq!(occ.len(), 1);
    assert_eq!(occ[0].start, e.start);
    assert_eq!(occ[0].end, e.end);
    assert!(
        e.occurrences(d(2026, 9, 1), d(2026, 9, 30), Tz::UTC)
            .is_empty(),
        "outside the window it does not occur"
    );
}

#[test]
fn an_empty_recurrence_is_treated_as_non_recurring() {
    // A provider may hand back a `Recurrence` with no rules at all; that must
    // not be mistaken for "expand me" and produce nothing.
    let mut e = CalEvent::new(
        "one",
        "Kickoff",
        zoned(2026, 8, 21, 9, 0, "UTC"),
        zoned(2026, 8, 21, 10, 0, "UTC"),
    );
    e.recurrence = Some(Recurrence::default());
    assert_eq!(
        e.occurrences(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC).len(),
        1
    );
}

#[test]
fn each_occurrence_keeps_the_events_wall_clock_duration() {
    let mut e = CalEvent::new(
        "weekly",
        "Sync",
        zoned(2026, 3, 1, 9, 0, "America/Chicago"),
        zoned(2026, 3, 1, 9, 45, "America/Chicago"),
    );
    e.recurrence = Some(Recurrence {
        rules: vec![RRule::parse("FREQ=WEEKLY;BYDAY=SU").unwrap()],
        ..Default::default()
    });
    let occ = e.occurrences(d(2026, 3, 1), d(2026, 3, 22), Tz::UTC);
    assert_eq!(occ.len(), 4);
    for o in &occ {
        // 45 minutes of WALL time on every instance, including the one after
        // the DST transition.
        match (&o.start, &o.end) {
            (EventTime::Zoned { local: a, .. }, EventTime::Zoned { local: b, .. }) => {
                assert_eq!((*b - *a).num_minutes(), 45);
                assert_eq!(a.hour(), 9);
            }
            other => panic!("expected zoned pair, got {other:?}"),
        }
    }
}

#[test]
fn an_all_day_recurrence_keeps_its_day_length() {
    let mut e = CalEvent::new(
        "hol",
        "Holiday",
        EventTime::Date {
            date: d(2026, 1, 1),
        },
        EventTime::Date {
            date: d(2026, 1, 2),
        },
    );
    e.recurrence = Some(Recurrence {
        rules: vec![RRule::parse("FREQ=YEARLY").unwrap()],
        ..Default::default()
    });
    let occ = e.occurrences(d(2026, 1, 1), d(2028, 12, 31), Tz::UTC);
    assert_eq!(occ.len(), 3);
    for o in &occ {
        // Still a floating one-day span, not promoted to a zoned time.
        match (&o.start, &o.end) {
            (EventTime::Date { date: a }, EventTime::Date { date: b }) => {
                assert_eq!((*b - *a).num_days(), 1);
            }
            other => panic!("expected two dates, got {other:?}"),
        }
    }
}

#[test]
fn a_malformed_event_whose_end_precedes_its_start_has_a_zero_span() {
    // Bad provider data must not produce a negative duration or a panic.
    let mut e = CalEvent::new(
        "bad",
        "Backwards",
        zoned(2026, 8, 21, 9, 0, "UTC"),
        zoned(2026, 8, 20, 9, 0, "UTC"),
    );
    e.recurrence = Some(Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;COUNT=2").unwrap()],
        ..Default::default()
    });
    let occ = e.occurrences(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC);
    assert_eq!(occ.len(), 2);
    assert_eq!(occ[0].start, occ[0].end, "clamped to a zero-length span");
}

#[test]
fn expand_by_date_buckets_every_day_an_event_touches() {
    // The shape the month grid and the agenda both read.
    let single = CalEvent::new(
        "s",
        "Standup",
        zoned(2026, 8, 21, 9, 30, "UTC"),
        zoned(2026, 8, 21, 10, 0, "UTC"),
    );
    let multi = CalEvent::new(
        "m",
        "Conference",
        zoned(2026, 8, 20, 9, 0, "UTC"),
        zoned(2026, 8, 22, 17, 0, "UTC"),
    );
    let by_date = expand_by_date(&[single, multi], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC);
    assert_eq!(by_date[&d(2026, 8, 20)].len(), 1);
    assert_eq!(by_date[&d(2026, 8, 21)].len(), 2, "both touch the 21st");
    assert_eq!(by_date[&d(2026, 8, 22)].len(), 1);
    assert!(!by_date.contains_key(&d(2026, 8, 23)));
}

#[test]
fn expand_by_date_materializes_recurrences_as_plain_events() {
    // Each bucket must be meaningful on its own — the UI never re-expands, and
    // a cached day would otherwise carry a rule that regenerates the series.
    let mut e = CalEvent::new(
        "w",
        "Weekly",
        zoned(2026, 8, 3, 9, 0, "UTC"),
        zoned(2026, 8, 3, 9, 30, "UTC"),
    );
    e.recurrence = Some(Recurrence {
        rules: vec![RRule::parse("FREQ=WEEKLY;BYDAY=MO").unwrap()],
        ..Default::default()
    });
    let by_date = expand_by_date(&[e], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC);
    let mondays: Vec<_> = by_date.keys().copied().collect();
    assert_eq!(
        mondays,
        vec![
            d(2026, 8, 3),
            d(2026, 8, 10),
            d(2026, 8, 17),
            d(2026, 8, 24),
            d(2026, 8, 31)
        ]
    );
    for evs in by_date.values() {
        assert_eq!(evs.len(), 1);
        assert!(
            evs[0].recurrence.is_none(),
            "an instance must not still carry the rule"
        );
    }
}

#[test]
fn expand_by_date_orders_a_day_all_day_first_then_by_time() {
    // The order an agenda reads in.
    let afternoon = CalEvent::new(
        "pm",
        "Review",
        zoned(2026, 8, 21, 15, 0, "UTC"),
        zoned(2026, 8, 21, 16, 0, "UTC"),
    );
    let morning = CalEvent::new(
        "am",
        "Standup",
        zoned(2026, 8, 21, 9, 30, "UTC"),
        zoned(2026, 8, 21, 10, 0, "UTC"),
    );
    let all_day = CalEvent::new(
        "ad",
        "Ooo: Jo",
        EventTime::Date {
            date: d(2026, 8, 21),
        },
        EventTime::Date {
            date: d(2026, 8, 22),
        },
    );
    let by_date = expand_by_date(
        &[afternoon, morning, all_day],
        d(2026, 8, 21),
        d(2026, 8, 21),
        Tz::UTC,
    );
    let titles: Vec<_> = by_date[&d(2026, 8, 21)]
        .iter()
        .map(|e| e.title.as_str())
        .collect();
    assert_eq!(titles, vec!["Ooo: Jo", "Standup", "Review"]);
}

#[test]
fn expand_by_date_clips_to_the_requested_window() {
    // A multi-day event straddling the edge contributes only its in-window days.
    let e = CalEvent::new(
        "trip",
        "Trip",
        zoned(2026, 7, 30, 9, 0, "UTC"),
        zoned(2026, 8, 2, 17, 0, "UTC"),
    );
    let by_date = expand_by_date(&[e], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC);
    assert_eq!(
        by_date.keys().copied().collect::<Vec<_>>(),
        vec![d(2026, 8, 1), d(2026, 8, 2)]
    );
    assert!(expand_by_date(&[], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC).is_empty());
}

#[test]
fn an_all_day_date_resolves_to_midnight_in_the_home_zone() {
    // The `EventTime::Date` arm of `instant_in`: a floating date has to become
    // *some* instant to sort alongside timed events.
    let t = EventTime::Date {
        date: d(2026, 8, 21),
    };
    let utc = t.instant_in(Tz::UTC, GapPolicy::ShiftForward).unwrap();
    assert_eq!(utc, self::utc(2026, 8, 21, 0, 0));
    // Midnight in Tokyo is 15:00Z the day before.
    let tokyo = t
        .instant_in(Tz::Asia__Tokyo, GapPolicy::ShiftForward)
        .unwrap();
    assert_eq!(tokyo, self::utc(2026, 8, 20, 15, 0));
}
