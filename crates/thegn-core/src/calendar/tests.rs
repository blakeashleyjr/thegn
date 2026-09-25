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
            format: ClockFormat::H24,
            show_date: false,
            is_home: false,
        },
        ResolvedClock {
            label: "kathmandu".into(),
            zone: Tz::Asia__Kathmandu,
            format: ClockFormat::H24,
            show_date: false,
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
            format: ClockFormat::H24,
            show_date: false,
            is_home: false,
        },
        ResolvedClock {
            label: "la".into(),
            zone: Tz::America__Los_Angeles,
            format: ClockFormat::H24,
            show_date: false,
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
fn readings_carry_row_format_and_date_policy_without_changing_deltas() {
    let clocks = vec![
        ResolvedClock {
            label: "tokyo".into(),
            zone: Tz::Asia__Tokyo,
            format: ClockFormat::Custom("%I:%M %p".into()),
            show_date: true,
            is_home: false,
        },
        ResolvedClock {
            label: "kathmandu".into(),
            zone: Tz::Asia__Kathmandu,
            format: ClockFormat::H24,
            show_date: false,
            is_home: false,
        },
    ];
    let readings = read_clocks(&clocks, utc(2026, 8, 21, 22, 0), Tz::UTC);
    assert_eq!(readings[0].format, ClockFormat::Custom("%I:%M %p".into()));
    assert!(readings[0].show_date);
    assert_eq!(readings[0].day_delta, 1);
    assert_eq!(readings[1].format, ClockFormat::H24);
    assert!(!readings[1].show_date);
    assert_eq!(readings[1].day_delta, 1);
}

#[test]
fn an_empty_label_falls_back_to_the_zone_city() {
    let clocks = vec![ResolvedClock {
        label: String::new(),
        zone: Tz::America__New_York,
        format: ClockFormat::H24,
        show_date: false,
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
        format: ClockFormat::H24,
        show_date: false,
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
    assert!(backwards.dates_in(Tz::UTC).is_empty());
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
fn a_malformed_recurrence_whose_end_precedes_its_start_is_refused() {
    // Bad provider data is refused by the bounded production expander rather
    // than being converted into a plausible zero-span recurrence.
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
    let mut budget = ExpansionBudget::default();
    assert_eq!(
        e.occurrences_bounded(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC, &mut budget),
        Err(ExpansionError::InvalidSpan)
    );
    assert!(
        e.occurrences(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC)
            .is_empty()
    );
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
    let by_date = expand_by_date(&[single, multi], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC).unwrap();
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
    let by_date = expand_by_date(&[e], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC).unwrap();
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
    )
    .unwrap();
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
    let by_date = expand_by_date(&[e], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC).unwrap();
    assert_eq!(
        by_date.keys().copied().collect::<Vec<_>>(),
        vec![d(2026, 8, 1), d(2026, 8, 2)]
    );
    assert!(
        expand_by_date(&[], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC)
            .unwrap()
            .is_empty()
    );
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

// --- bounded expansion (THE-458) ---------------------------------------------

fn all_day(from: NaiveDate, to: NaiveDate) -> CalEvent {
    CalEvent::new(
        "span",
        "Span",
        EventTime::Date { date: from },
        EventTime::Date { date: to },
    )
}

fn recurring(start: EventTime, end: EventTime, rule: &str) -> CalEvent {
    let mut e = CalEvent::new("r", "Recurring", start, end);
    e.recurrence = Some(Recurrence {
        rules: vec![RRule::parse(rule).unwrap()],
        ..Default::default()
    });
    e
}

fn expand_with(
    events: &[CalEvent],
    from: NaiveDate,
    to: NaiveDate,
    budget: &mut ExpansionBudget,
) -> Result<ExpandedCalendar, ExpansionError> {
    expand_calendar_with_budget(events, from, to, Tz::UTC, budget)
}

#[test]
fn a_century_span_costs_the_window_not_its_lifetime() {
    let event = all_day(d(1900, 1, 1), d(2200, 1, 1));
    let mut budget = ExpansionBudget::default();
    let expanded = expand_with(
        std::slice::from_ref(&event),
        d(2026, 8, 21),
        d(2026, 8, 21),
        &mut budget,
    )
    .unwrap();
    assert_eq!(expanded.by_date.len(), 1);
    assert_eq!(expanded.by_date[&d(2026, 8, 21)].len(), 1);
    assert_eq!(budget.used_bucket_entries(), 1, "one day, not 110k");
    assert_eq!(budget.used_occurrences(), 1);
    assert_eq!(budget.used_recurrence_work(), 0);
    assert_eq!(
        event
            .occupied_dates_in(d(2026, 8, 21), d(2026, 8, 23), Tz::UTC)
            .unwrap()
            .count(),
        3
    );

    // Wholly outside: nothing retained, nothing charged but the visit.
    let mut outside = ExpansionBudget::default();
    let none = expand_with(&[event], d(1800, 1, 1), d(1800, 1, 1), &mut outside).unwrap();
    assert!(none.by_date.is_empty() && none.occurrences.is_empty());
    assert_eq!(outside.used_source_visits(), 1);
    assert_eq!(outside.used_occurrences(), 0);
    assert_eq!(outside.used_retained_bytes(), 0);
}

#[test]
fn chrono_extrema_are_intersected_without_overflow() {
    // The whole representable calendar, queried for one day at each end and
    // in the middle.
    let everything = all_day(NaiveDate::MIN, NaiveDate::MAX);
    for day in [
        NaiveDate::MIN,
        d(2026, 8, 21),
        NaiveDate::MAX.pred_opt().unwrap(),
    ] {
        let mut budget = ExpansionBudget::default();
        let expanded =
            expand_with(std::slice::from_ref(&everything), day, day, &mut budget).unwrap();
        assert_eq!(
            expanded.by_date.keys().copied().collect::<Vec<_>>(),
            vec![day]
        );
        assert_eq!(budget.used_bucket_entries(), 1);
    }
    // A timed span across every representable instant.
    let timeline = CalEvent::new(
        "t",
        "Timeline",
        EventTime::Instant {
            at: DateTime::<Utc>::MIN_UTC,
        },
        EventTime::Instant {
            at: DateTime::<Utc>::MAX_UTC,
        },
    );
    let expanded = expand_calendar(&[timeline], d(2026, 8, 21), d(2026, 8, 21), Tz::UTC).unwrap();
    assert_eq!(expanded.by_date.len(), 1);
    // The final representable date is iterated without stepping past it.
    let last = NaiveDate::MAX;
    let dates: Vec<_> = all_day(last.pred_opt().unwrap(), last)
        .occupied_dates_in(last.pred_opt().unwrap(), last, Tz::UTC)
        .unwrap()
        .collect();
    assert_eq!(dates, vec![last.pred_opt().unwrap()], "DTEND is exclusive");
    let point = all_day(last, last);
    assert_eq!(
        point
            .occupied_dates_in(last, last, Tz::UTC)
            .unwrap()
            .collect::<Vec<_>>(),
        vec![last]
    );
}

#[test]
fn midnight_endings_are_exclusive_but_fractions_past_it_are_not() {
    let day = |e: CalEvent| -> Vec<NaiveDate> {
        e.occupied_dates_in(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC)
            .unwrap()
            .collect()
    };
    // All-day DTEND is exclusive.
    assert_eq!(
        day(all_day(d(2026, 8, 20), d(2026, 8, 22))),
        vec![d(2026, 8, 20), d(2026, 8, 21)]
    );
    // A timed end at exactly 00:00 does not mark the next day...
    let at_midnight = CalEvent::new(
        "m",
        "m",
        zoned(2026, 8, 21, 22, 0, "UTC"),
        zoned(2026, 8, 22, 0, 0, "UTC"),
    );
    assert_eq!(day(at_midnight), vec![d(2026, 8, 21)]);
    // ...but half a second past midnight does.
    let past = CalEvent::new(
        "p",
        "p",
        zoned(2026, 8, 21, 22, 0, "UTC"),
        EventTime::Zoned {
            local: d(2026, 8, 22).and_hms_milli_opt(0, 0, 0, 500).unwrap(),
            zone: TzRef::new("UTC"),
        },
    );
    assert_eq!(day(past), vec![d(2026, 8, 21), d(2026, 8, 22)]);
    // A zero-length event is a point on its start date, midnight included.
    let point = CalEvent::new(
        "z",
        "z",
        zoned(2026, 8, 21, 0, 0, "UTC"),
        zoned(2026, 8, 21, 0, 0, "UTC"),
    );
    assert_eq!(day(point), vec![d(2026, 8, 21)]);
    assert_eq!(
        day(all_day(d(2026, 8, 21), d(2026, 8, 21))),
        vec![d(2026, 8, 21)]
    );
}

#[test]
fn inverted_spans_are_refused_even_on_the_same_day() {
    let same_day = CalEvent::new(
        "i",
        "i",
        zoned(2026, 8, 21, 9, 0, "UTC"),
        zoned(2026, 8, 21, 8, 0, "UTC"),
    );
    let dates = all_day(d(2026, 8, 22), d(2026, 8, 21));
    let instants = CalEvent::new(
        "x",
        "x",
        EventTime::Instant {
            at: utc(2026, 8, 21, 9, 0),
        },
        EventTime::Instant {
            at: utc(2026, 8, 21, 8, 59),
        },
    );
    for e in [same_day, dates, instants] {
        let mut budget = ExpansionBudget::default();
        assert_eq!(
            e.occurrences_bounded(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC, &mut budget),
            Err(ExpansionError::InvalidSpan)
        );
        // In a whole expansion the bad row is skipped and counted, never
        // escalated into a failure of the call.
        let expanded = expand_calendar(
            std::slice::from_ref(&e),
            d(2026, 8, 1),
            d(2026, 8, 31),
            Tz::UTC,
        )
        .unwrap();
        assert_eq!(expanded.skipped, 1);
        assert!(expanded.occurrences.is_empty());
        // Refused even when wholly outside the window: validity is not a
        // property of the query.
        assert!(
            e.occupied_dates_in(d(2030, 1, 1), d(2030, 1, 1), Tz::UTC)
                .is_err()
        );
    }
    assert_eq!(
        expand_calendar(&[], d(2026, 8, 2), d(2026, 8, 1), Tz::UTC),
        Err(ExpansionError::InvalidWindow)
    );
    assert!(
        all_day(d(2026, 8, 1), d(2026, 8, 2))
            .occupied_dates_in(d(2026, 8, 2), d(2026, 8, 1), Tz::UTC)
            .is_err()
    );
}

#[test]
fn a_dst_gap_start_before_a_valid_end_is_a_point_not_an_error() {
    // 02:30→03:10 is 40 minutes of wall time, but 02:30 does not exist on
    // spring-forward day and resolves forward past the end.
    let e = CalEvent::new(
        "g",
        "gap",
        zoned(2026, 3, 8, 2, 30, "America/New_York"),
        zoned(2026, 3, 8, 3, 10, "America/New_York"),
    );
    let home: Tz = "America/New_York".parse().unwrap();
    let got: Vec<_> = e
        .occupied_dates_in(d(2026, 3, 1), d(2026, 3, 31), home)
        .unwrap()
        .collect();
    assert_eq!(got, vec![d(2026, 3, 8)]);
}

#[test]
fn a_count_one_or_rdate_instance_from_decades_ago_still_overlaps_today() {
    // 20 years long, ending tomorrow: today's narrow query must see it, and
    // cheaply — without a 7,000-day walk.
    let start = zoned(2006, 9, 18, 9, 0, "UTC");
    let end = zoned(2026, 9, 19, 9, 0, "UTC");
    let count_one = recurring(start.clone(), end.clone(), "FREQ=DAILY;COUNT=1");
    let mut rdate = CalEvent::new("rd", "RDATE", start.clone(), end);
    rdate.recurrence = Some(Recurrence {
        rdates: vec![start],
        ..Default::default()
    });
    for e in [count_one, rdate] {
        let mut budget = ExpansionBudget::default();
        let got = expand_with(&[e], d(2026, 9, 18), d(2026, 9, 18), &mut budget).unwrap();
        assert_eq!(got.occurrences.len(), 1);
        assert_eq!(got.by_date.len(), 1);
        assert!(
            budget.used_recurrence_work() < 100,
            "{}",
            budget.used_recurrence_work()
        );
    }
}

#[test]
fn an_endless_rule_from_long_ago_is_fast_forwarded_to_the_window() {
    let daily = recurring(
        zoned(1900, 1, 1, 9, 0, "UTC"),
        zoned(1900, 1, 1, 10, 0, "UTC"),
        "FREQ=DAILY",
    );
    let hourly = recurring(
        zoned(1990, 1, 1, 9, 0, "UTC"),
        zoned(1990, 1, 1, 9, 30, "UTC"),
        "FREQ=HOURLY;INTERVAL=5",
    );
    let mut budget = ExpansionBudget::default();
    let got = expand_with(&[daily], d(2026, 8, 21), d(2026, 8, 21), &mut budget).unwrap();
    assert_eq!(got.occurrences.len(), 1);
    assert!(
        budget.used_recurrence_work() < 100,
        "{}",
        budget.used_recurrence_work()
    );

    let mut budget = ExpansionBudget::default();
    let got = expand_with(&[hourly], d(2026, 8, 21), d(2026, 8, 21), &mut budget).unwrap();
    assert!(!got.occurrences.is_empty());
    // Every instance lands on the rule's 5-hour grid from DTSTART.
    let seed = d(1990, 1, 1).and_hms_opt(9, 0, 0).unwrap();
    for o in &got.occurrences {
        let EventTime::Zoned { local, .. } = &o.start else {
            panic!("zoned instance expected")
        };
        assert_eq!(local.signed_duration_since(seed).num_hours() % 5, 0);
    }
    assert!(
        budget.used_recurrence_work() < 500,
        "{}",
        budget.used_recurrence_work()
    );
}

#[test]
fn fast_forward_matches_the_full_walk() {
    // A COUNT that never runs out forces the walk from DTSTART; without COUNT
    // the rule is fast-forwarded. Both must agree on every window — including
    // windows whose month precedes DTSTART's month-of-year, and DTSTARTs whose
    // day-of-month (the 31st, Feb 29) does not exist in every period.
    let cases = [
        ("FREQ=MONTHLY", d(2019, 11, 15)),
        ("FREQ=MONTHLY;INTERVAL=5;BYDAY=-1FR", d(2019, 11, 15)),
        ("FREQ=MONTHLY", d(2019, 1, 31)),
        ("FREQ=MONTHLY;INTERVAL=2;BYMONTHDAY=-1", d(2019, 1, 31)),
        ("FREQ=YEARLY", d(2019, 11, 15)),
        ("FREQ=YEARLY", d(2020, 2, 29)),
        (
            "FREQ=YEARLY;INTERVAL=2;BYMONTH=2;BYMONTHDAY=29",
            d(2020, 2, 29),
        ),
        ("FREQ=YEARLY;BYWEEKNO=1;BYDAY=MO", d(2019, 11, 15)),
        (
            "FREQ=WEEKLY;INTERVAL=3;BYDAY=SU,WE;WKST=SU",
            d(2019, 11, 15),
        ),
        ("FREQ=DAILY;INTERVAL=7", d(2019, 11, 15)),
        // Sub-daily rules step the clock and fast-forward by whole steps.
        ("FREQ=HOURLY;INTERVAL=5", d(2026, 7, 1)),
        ("FREQ=HOURLY;INTERVAL=7;BYHOUR=9,17", d(2026, 7, 1)),
        ("FREQ=MINUTELY;INTERVAL=25", d(2026, 7, 30)),
    ];
    for (rule, day) in cases {
        let start = day.and_hms_opt(9, 0, 0).unwrap();
        let fast = Recurrence {
            rules: vec![RRule::parse(rule).unwrap()],
            ..Default::default()
        };
        let walked = Recurrence {
            rules: vec![RRule::parse(&format!("{rule};COUNT=100000")).unwrap()],
            ..Default::default()
        };
        for (from, to) in [
            (d(2026, 8, 1), d(2026, 8, 3)),
            (d(2026, 2, 1), d(2026, 2, 28)),
            (d(2025, 12, 20), d(2026, 1, 10)),
            (d(2024, 11, 1), d(2024, 11, 30)),
        ] {
            if from < day {
                continue; // before DTSTART: nothing to compare
            }
            let mut b1 = ExpansionBudget::default();
            let mut b2 = ExpansionBudget::default();
            assert_eq!(
                recur::expand_local_bounded(&fast, start, from, to, &mut b1).unwrap(),
                recur::expand_local_bounded(&walked, start, from, to, &mut b2).unwrap(),
                "{rule} from {day} over {from}..{to}"
            );
            assert!(
                b1.used_recurrence_work() <= b2.used_recurrence_work(),
                "{rule} from {day}: fast-forward did more work"
            );
        }
    }
}

#[test]
fn a_long_instance_starting_before_the_window_is_included() {
    // Every Monday for three days: a Wednesday query must see Monday's.
    let e = recurring(
        zoned(2026, 8, 3, 9, 0, "UTC"),
        zoned(2026, 8, 6, 9, 0, "UTC"),
        "FREQ=WEEKLY",
    );
    let got = expand_calendar(&[e], d(2026, 8, 19), d(2026, 8, 19), Tz::UTC).unwrap();
    assert_eq!(got.occurrences.len(), 1);
    assert_eq!(got.occurrences[0].start, zoned(2026, 8, 17, 9, 0, "UTC"));
}

#[test]
fn a_counted_walk_that_exceeds_the_budget_is_refused() {
    // COUNT has to be walked from DTSTART, so a rule seeded at the dawn of the
    // representable calendar cannot be answered within the work ceiling. That
    // is a refusal, not a quietly shortened series.
    let e = recurring(
        zoned(-4000, 1, 1, 9, 0, "UTC"),
        zoned(-4000, 1, 1, 10, 0, "UTC"),
        "FREQ=DAILY;COUNT=4000000",
    );
    let mut budget = ExpansionBudget::default();
    assert_eq!(
        expand_with(
            std::slice::from_ref(&e),
            d(2026, 8, 1),
            d(2026, 8, 31),
            &mut budget
        ),
        Err(ExpansionError::Budget(ExpansionLimit::RecurrenceWork))
    );
    assert!(budget.used_recurrence_work() <= MAX_EXPANSION_RECURRENCE_WORK);
    // Whole-call exhaustion is never a row-local skip.
    assert!(!ExpansionError::Budget(ExpansionLimit::RecurrenceWork).is_row_local());

    // The same shape inside the work ceiling is answered normally.
    let recent = recurring(
        zoned(2020, 1, 1, 9, 0, "UTC"),
        zoned(2020, 1, 1, 10, 0, "UTC"),
        "FREQ=DAILY;COUNT=4000000",
    );
    let got = expand_calendar(&[recent], d(2026, 8, 1), d(2026, 8, 31), Tz::UTC).unwrap();
    assert_eq!(got.occurrences.len(), 31);
}

#[test]
fn a_by_part_cross_product_is_refused_as_it_grows() {
    // 24 × 60 × 60 candidate times per period. The ceiling here is BELOW one
    // period's product, so passing this proves the charge lands per candidate
    // as the product grows — an implementation that built all 86,400 and
    // charged once per period would blow through it.
    let mut rule = RRule::parse("FREQ=DAILY").unwrap();
    rule.by_hour = (0..24).collect();
    rule.by_minute = (0..60).collect();
    rule.by_second = (0..60).collect();
    let mut e = CalEvent::new(
        "x",
        "x",
        zoned(2026, 8, 1, 0, 0, "UTC"),
        zoned(2026, 8, 1, 0, 0, "UTC"),
    );
    e.recurrence = Some(Recurrence {
        rules: vec![rule],
        ..Default::default()
    });
    let mut budget = ExpansionBudget::limited(31, 1, 10_000, 1_000, 1_000, MAX_EXPANSION_BYTES);
    assert_eq!(
        expand_with(
            std::slice::from_ref(&e),
            d(2026, 8, 1),
            d(2026, 8, 31),
            &mut budget
        ),
        Err(ExpansionError::Budget(ExpansionLimit::RecurrenceWork))
    );
    assert!(
        budget.used_recurrence_work() <= 10_000,
        "charged past its ceiling: {}",
        budget.used_recurrence_work()
    );
    assert_eq!(
        budget.used_expanded_locals(),
        0,
        "refused inside one period"
    );
    // Lower bounds, because `spend` leaves the counter untouched when it
    // refuses: an implementation that built the whole 86,400-element product
    // and charged once per period would record a handful of units and ZERO
    // bytes, and would sail through the upper bounds above.
    assert!(
        budget.used_retained_bytes() > 0,
        "candidates were charged as they grew"
    );
    assert!(
        budget.used_recurrence_work() >= 9_000,
        "charged per candidate, not per period: {}",
        budget.used_recurrence_work()
    );

    // Under the DEFAULT budget the same rule is refused too, and the pass's
    // peak transient stays small: candidates and expanded locals are charged
    // in bytes as well as work, so the vectors holding them cannot grow to
    // the work ceiling's worth of instants.
    let mut budget = ExpansionBudget::default();
    assert_eq!(
        expand_with(
            std::slice::from_ref(&e),
            d(2026, 8, 1),
            d(2026, 8, 31),
            &mut budget
        ),
        Err(ExpansionError::Budget(
            ExpansionLimit::MaterializedOccurrences
        ))
    );
    assert!(budget.used_expanded_locals() <= MAX_EXPANSION_OCCURRENCES);
    assert!(
        budget.used_retained_bytes() < 8 * 1024 * 1024,
        "peak transient: {} bytes",
        budget.used_retained_bytes()
    );

    // A yearly BYMONTH × BYDAY list is priced before a single period is built.
    let mut wide = RRule::parse("FREQ=YEARLY").unwrap();
    wide.by_month = vec![1; 4_000];
    wide.by_day = vec![
        ByDay {
            nth: None,
            weekday: Weekday::Mon,
        };
        4_000
    ];
    let rec = Recurrence {
        rules: vec![wide],
        ..Default::default()
    };
    let mut budget = ExpansionBudget::default();
    assert_eq!(
        recur::expand_local_bounded(
            &rec,
            d(2026, 1, 1).and_hms_opt(0, 0, 0).unwrap(),
            d(2026, 1, 1),
            d(2026, 12, 31),
            &mut budget
        ),
        Err(ExpansionError::Budget(ExpansionLimit::RecurrenceWork))
    );
    assert_eq!(budget.used_recurrence_work(), 0, "priced before any work");
    assert_eq!(budget.used_retained_bytes(), 0);
}

#[test]
fn every_rdate_and_exdate_is_charged_even_outside_the_window() {
    let far: Vec<EventTime> = (0..10_000)
        .map(|i| EventTime::Date {
            date: d(1900, 1, 1) + chrono::Days::new(i),
        })
        .collect();
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        rdates: far.clone(),
        exdates: far,
    };
    let seed = d(2026, 8, 1).and_hms_opt(9, 0, 0).unwrap();
    let mut budget = ExpansionBudget::default();
    recur::expand_local_bounded(&rec, seed, d(2026, 8, 1), d(2026, 8, 1), &mut budget).unwrap();
    assert!(budget.used_recurrence_work() >= 20_000);
    let mut small = ExpansionBudget::limited(10, 10, 19_999, 10, 10, MAX_EXPANSION_BYTES);
    assert_eq!(
        recur::expand_local_bounded(&rec, seed, d(2026, 8, 1), d(2026, 8, 1), &mut small),
        Err(ExpansionError::Budget(ExpansionLimit::RecurrenceWork))
    );
}

#[test]
fn a_sub_daily_rule_that_would_flood_the_window_is_refused() {
    // Every second of five months: refused on the retained-instant ceiling,
    // which is charged per instant as the walk produces them.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=SECONDLY").unwrap()],
        ..Default::default()
    };
    let mut budget = ExpansionBudget::default();
    assert_eq!(
        recur::expand_local_bounded(
            &rec,
            d(2026, 8, 1).and_hms_opt(0, 0, 0).unwrap(),
            d(2026, 8, 1),
            d(2026, 12, 31),
            &mut budget
        ),
        Err(ExpansionError::Budget(
            ExpansionLimit::MaterializedOccurrences
        ))
    );
}

#[test]
fn oversized_rows_are_refused_even_when_outside_the_window() {
    let mut huge = all_day(d(1800, 1, 1), d(1800, 1, 2));
    huge.description = "x".repeat(MAX_EVENT_PAYLOAD_BYTES + 1);
    let mut crowded = all_day(d(1800, 1, 1), d(1800, 1, 2));
    crowded.extra = (0..=MAX_EVENT_CHILD_ENTRIES)
        .map(|i| (i.to_string(), String::new()))
        .collect();
    // Recurrence children count too — including every BY* list.
    let mut rule = RRule::parse("FREQ=DAILY").unwrap();
    rule.by_set_pos = vec![1; MAX_EVENT_CHILD_ENTRIES];
    let mut nested = recurring(
        zoned(1800, 1, 1, 9, 0, "UTC"),
        zoned(1800, 1, 1, 10, 0, "UTC"),
        "FREQ=DAILY",
    );
    nested.reminders = vec![Reminder { minutes_before: 1 }];
    nested.recurrence.as_mut().unwrap().rules = vec![rule];
    // A zone name is part of the payload too.
    let tz = CalEvent::new(
        "tz",
        "tz",
        EventTime::Zoned {
            local: d(1800, 1, 1).and_hms_opt(9, 0, 0).unwrap(),
            zone: TzRef::new("z".repeat(MAX_EVENT_PAYLOAD_BYTES)),
        },
        zoned(1800, 1, 1, 10, 0, "UTC"),
    );
    for (e, limit) in [
        (huge, ExpansionLimit::EventPayload),
        (crowded, ExpansionLimit::EventChildren),
        (nested, ExpansionLimit::EventChildren),
        (tz, ExpansionLimit::EventPayload),
    ] {
        let mut budget = ExpansionBudget::default();
        assert_eq!(
            e.occurrences_bounded(d(2026, 8, 1), d(2026, 8, 31), Tz::UTC, &mut budget),
            Err(ExpansionError::Budget(limit))
        );
        assert!(ExpansionError::Budget(limit).is_row_local());
    }
}

#[test]
fn a_row_local_defect_never_fails_the_whole_expansion() {
    // One bad row must cost that row: the alternative blanks a month — and,
    // because the row stays in the cache, silences reminders for good.
    let good = |uid: &str| {
        let mut e = all_day(d(2026, 8, 20), d(2026, 8, 21));
        e.uid = uid.into();
        e
    };
    let inverted = CalEvent::new(
        "inverted",
        "bad",
        zoned(2026, 8, 20, 9, 0, "UTC"),
        zoned(2026, 8, 20, 8, 0, "UTC"),
    );
    let mut huge = good("huge");
    huge.description = "x".repeat(MAX_EVENT_PAYLOAD_BYTES + 1);
    let mut crowded = good("crowded");
    crowded.extra = (0..=MAX_EVENT_CHILD_ENTRIES)
        .map(|i| (i.to_string(), String::new()))
        .collect();

    let rows = vec![good("a"), inverted, huge, crowded, good("b")];
    let got = expand_calendar(&rows, d(2026, 8, 20), d(2026, 8, 20), Tz::UTC).unwrap();
    assert_eq!(got.skipped, 3, "three row-local defects");
    let uids: Vec<&str> = got.occurrences.iter().map(|e| e.uid.as_str()).collect();
    assert_eq!(uids, vec!["a", "b"], "the readable rows still expand");
    assert_eq!(got.by_date[&d(2026, 8, 20)].len(), 2);

    // Shared-dimension exhaustion is NOT row-local: it is a property of the
    // call and still refuses outright.
    for limit in [
        ExpansionLimit::WindowDays,
        ExpansionLimit::SourceVisits,
        ExpansionLimit::RecurrenceWork,
        ExpansionLimit::MaterializedOccurrences,
        ExpansionLimit::BucketEntries,
        ExpansionLimit::RetainedBytes,
    ] {
        assert!(!ExpansionError::Budget(limit).is_row_local(), "{limit}");
    }
    assert!(!ExpansionError::Arithmetic.is_row_local());
    assert!(!ExpansionError::InvalidWindow.is_row_local());
    let mut budget = ExpansionBudget::limited(31, 2, 0, 2, 2, MAX_EXPANSION_BYTES);
    assert_eq!(
        expand_with(&rows, d(2026, 8, 20), d(2026, 8, 20), &mut budget),
        Err(ExpansionError::Budget(ExpansionLimit::SourceVisits))
    );
}

#[test]
fn default_ceilings_admit_a_heavy_but_legitimate_month() {
    // The ceilings exist to stop amplification, not to cap real calendars.
    // `[calendar] max_events` admits 2,000 rows per account; the heaviest
    // legitimate shape is a daily recurrence, which yields one occurrence per
    // day of the widened (month + a week either side) window.
    const WIDENED_DAYS: u64 = 49;
    const ROWS: usize = 500;
    let rows: Vec<CalEvent> = (0..ROWS)
        .map(|i| {
            let mut e = recurring(
                zoned(2026, 1, 1, 9, 0, "UTC"),
                zoned(2026, 1, 1, 10, 0, "UTC"),
                "FREQ=DAILY",
            );
            e.uid = format!("daily-{i}");
            e.title = format!("Daily {i}");
            e
        })
        .collect();
    let from = d(2026, 8, 1);
    let to = from
        .checked_add_days(chrono::Days::new(WIDENED_DAYS - 1))
        .unwrap();
    let mut budget = ExpansionBudget::default();
    let got = expand_with(&rows, from, to, &mut budget).unwrap();
    assert_eq!(got.occurrences.len(), ROWS * WIDENED_DAYS as usize);
    assert_eq!(got.skipped, 0);

    // 500 daily recurrences is already a heavy month; a whole admitted
    // account of them (2,000) must still fit the counted dimensions.
    let factor = 2_000 / ROWS;
    assert!(budget.used_occurrences() * factor <= MAX_EXPANSION_OCCURRENCES);
    // Expanded locals share that ceiling and are the tighter of the two: the
    // walk looks back by the event's own duration, so it produces a few more
    // locals than it keeps.
    assert!(
        budget.used_expanded_locals() * factor <= MAX_EXPANSION_OCCURRENCES,
        "{} locals per row",
        budget.used_expanded_locals() / ROWS
    );
    assert!(budget.used_bucket_entries() * factor <= MAX_EXPANSION_BUCKET_ENTRIES);
    assert!(budget.used_recurrence_work() * factor <= MAX_EXPANSION_RECURRENCE_WORK);
    assert!(budget.used_source_visits() * factor <= MAX_EXPANSION_SOURCE_VISITS);

    // Bytes are the dimension meant to bind first, so they get BOTH a
    // per-occurrence bound — `CalEvent` is a public plugin struct designed to
    // gain fields, and a raw total would fail here looking like an unrelated
    // regression — and the whole-account total DERIVED from that measured
    // cost, because only the total says whether a saturated account still
    // expands. Without it a new field can push production to "month
    // unavailable, reminders stalled" while this test stays green.
    const BYTES_PER_OCCURRENCE: usize = 1024;
    let per = budget.used_retained_bytes() / budget.used_occurrences();
    assert!(
        per <= BYTES_PER_OCCURRENCE,
        "{per} bytes per occurrence: re-derive the byte ceiling"
    );
    let saturated = per * 2_000 * WIDENED_DAYS as usize;
    assert!(
        saturated <= MAX_EXPANSION_BYTES,
        "a saturated account needs {saturated} bytes at {per} each, past the \
         {MAX_EXPANSION_BYTES}-byte ceiling: raise MAX_EXPANSION_BYTES (and its \
         derivation) rather than loosening this bound"
    );
}

#[test]
fn unrepresentable_instance_arithmetic_is_an_error_not_a_wrap() {
    // A daily recurrence at the end of the representable calendar: the next
    // instance's end cannot be formed, and that is an arithmetic failure of
    // the call, not a silently shortened event.
    let last = NaiveDate::MAX;
    let mut e = CalEvent::new(
        "edge",
        "Edge",
        EventTime::Date {
            date: last.pred_opt().unwrap(),
        },
        EventTime::Date { date: last },
    );
    e.recurrence = Some(Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        ..Default::default()
    });
    let mut budget = ExpansionBudget::default();
    assert_eq!(
        e.occurrences_bounded(
            last.pred_opt().unwrap().pred_opt().unwrap(),
            last,
            Tz::UTC,
            &mut budget
        ),
        Err(ExpansionError::Arithmetic)
    );
    assert!(!ExpansionError::Arithmetic.is_row_local());
}

#[test]
fn a_large_multi_day_payload_is_shared_not_cloned_per_day() {
    let mut big = all_day(d(2026, 8, 1), d(2026, 8, 31));
    big.description = "d".repeat(512 * 1024);
    let mut budget = ExpansionBudget::default();
    let got = expand_with(&[big], d(2026, 8, 1), d(2026, 8, 31), &mut budget).unwrap();
    assert_eq!(got.by_date.len(), 30);
    assert_eq!(got.occurrences.len(), 1);
    let one = &got.occurrences[0];
    assert!(got.by_date.values().all(|v| Arc::ptr_eq(&v[0], one)));
    // One payload copy is charged, not thirty.
    assert!(budget.used_retained_bytes() < 2 * 512 * 1024);
    assert!(budget.used_retained_bytes() > 512 * 1024);
    assert!(one.recurrence.is_none());
}

#[test]
fn retained_bytes_are_reserved_before_the_payload_is_cloned() {
    let mut e = all_day(d(2026, 8, 1), d(2026, 8, 3));
    e.description = "d".repeat(4096);
    let mut probe = ExpansionBudget::default();
    expand_with(&[e.clone()], d(2026, 8, 1), d(2026, 8, 31), &mut probe).unwrap();
    let used = probe.used_retained_bytes();

    let mut exact = ExpansionBudget::limited(31, 1, 0, 1, 2, used);
    assert!(expand_with(&[e.clone()], d(2026, 8, 1), d(2026, 8, 31), &mut exact).is_ok());
    let mut short = ExpansionBudget::limited(31, 1, 0, 1, 2, used - 1);
    assert_eq!(
        expand_with(&[e.clone()], d(2026, 8, 1), d(2026, 8, 31), &mut short),
        Err(ExpansionError::Budget(ExpansionLimit::RetainedBytes))
    );
    // Too small for even the payload: refused before the clone, so nothing
    // beyond the occurrence reservation is recorded.
    let mut tiny = ExpansionBudget::limited(31, 1, 0, 1, 2, 1_000);
    assert_eq!(
        expand_with(&[e], d(2026, 8, 1), d(2026, 8, 31), &mut tiny),
        Err(ExpansionError::Budget(ExpansionLimit::RetainedBytes))
    );
    assert_eq!(tiny.used_retained_bytes(), 0);
}

#[test]
fn each_budget_admits_exactly_its_limit_and_refuses_one_more() {
    let two_days = all_day(d(2026, 8, 20), d(2026, 8, 22));
    let rows = vec![two_days.clone(), two_days.clone()];
    let (from, to) = (d(2026, 8, 20), d(2026, 8, 21));
    let bytes = MAX_EXPANSION_BYTES;
    // (window, visits, work, occurrences, bucket entries) → expected refusal
    let cases = [
        ((2, 2, 0, 2, 4), None),
        ((1, 2, 0, 2, 4), Some(ExpansionLimit::WindowDays)),
        ((2, 1, 0, 2, 4), Some(ExpansionLimit::SourceVisits)),
        (
            (2, 2, 0, 1, 4),
            Some(ExpansionLimit::MaterializedOccurrences),
        ),
        ((2, 2, 0, 2, 3), Some(ExpansionLimit::BucketEntries)),
    ];
    for ((w, v, r, o, b), want) in cases {
        let mut budget = ExpansionBudget::limited(w, v, r, o, b, bytes);
        let got = expand_with(&rows, from, to, &mut budget);
        match want {
            None => assert!(got.is_ok(), "{got:?}"),
            Some(limit) => assert_eq!(got, Err(ExpansionError::Budget(limit))),
        }
    }
    // Zero is zero capacity, never "unlimited".
    let mut zero = ExpansionBudget::limited(0, 0, 0, 0, 0, 0);
    assert_eq!(
        expand_with(&[], from, from, &mut zero),
        Err(ExpansionError::Budget(ExpansionLimit::WindowDays))
    );
    let mut work = ExpansionBudget::limited(2, 2, 0, 2, 4, bytes);
    let daily = recurring(
        zoned(2026, 8, 20, 9, 0, "UTC"),
        zoned(2026, 8, 20, 10, 0, "UTC"),
        "FREQ=DAILY",
    );
    assert_eq!(
        expand_with(&[daily], from, to, &mut work),
        Err(ExpansionError::Budget(ExpansionLimit::RecurrenceWork))
    );
    // The default window ceiling.
    assert_eq!(
        expand_calendar(&[], d(2026, 1, 1), d(2036, 1, 10), Tz::UTC),
        Err(ExpansionError::Budget(ExpansionLimit::WindowDays))
    );
}

#[test]
fn the_budget_is_shared_across_many_rows() {
    // Each row is small; together they exhaust the one budget for the call.
    let rows: Vec<CalEvent> = (0..100)
        .map(|i| {
            let mut e = all_day(d(2026, 8, 20), d(2026, 8, 21));
            e.uid = format!("row-{i}");
            e
        })
        .collect();
    let mut budget = ExpansionBudget::limited(31, 1_000, 0, 99, 1_000, MAX_EXPANSION_BYTES);
    assert_eq!(
        expand_with(&rows, d(2026, 8, 1), d(2026, 8, 31), &mut budget),
        Err(ExpansionError::Budget(
            ExpansionLimit::MaterializedOccurrences
        ))
    );
    let mut enough = ExpansionBudget::limited(31, 1_000, 0, 100, 1_000, MAX_EXPANSION_BYTES);
    let got = expand_with(&rows, d(2026, 8, 1), d(2026, 8, 31), &mut enough).unwrap();
    assert_eq!(got.occurrences.len(), 100);
    assert_eq!(enough.used_source_visits(), 100);
}

#[test]
fn multi_day_buckets_share_one_occurrence_with_one_reminder() {
    let mut event = CalEvent::new(
        "trip",
        "Trip",
        zoned(2026, 8, 20, 9, 0, "UTC"),
        zoned(2026, 8, 22, 17, 0, "UTC"),
    );
    event.reminders = vec![Reminder { minutes_before: 10 }];
    let expanded = expand_calendar(&[event], d(2026, 8, 20), d(2026, 8, 22), Tz::UTC).unwrap();
    assert_eq!(expanded.occurrences.len(), 1);
    assert!(Arc::ptr_eq(
        &expanded.occurrences[0],
        &expanded.by_date[&d(2026, 8, 20)][0]
    ));
    assert!(Arc::ptr_eq(
        &expanded.by_date[&d(2026, 8, 20)][0],
        &expanded.by_date[&d(2026, 8, 22)][0]
    ));
    let fired = reminders::due_shared(
        &expanded.occurrences,
        Tz::UTC,
        &[],
        utc(2026, 8, 20, 8, 49).timestamp_millis(),
        utc(2026, 8, 20, 8, 51).timestamp_millis(),
    );
    assert_eq!(fired.len(), 1, "the unique occurrence has one reminder");
}

#[test]
fn expansion_errors_render_fixed_labels() {
    assert_eq!(
        ExpansionError::Budget(ExpansionLimit::BucketEntries).to_string(),
        "calendar expansion budget exhausted (day entries)"
    );
    for (e, s) in [
        (ExpansionError::InvalidWindow, "calendar window is invalid"),
        (
            ExpansionError::InvalidSpan,
            "calendar event span is invalid",
        ),
        (
            ExpansionError::Arithmetic,
            "calendar date arithmetic overflowed",
        ),
    ] {
        assert_eq!(e.to_string(), s);
    }
    for limit in [
        ExpansionLimit::WindowDays,
        ExpansionLimit::SourceVisits,
        ExpansionLimit::RecurrenceWork,
        ExpansionLimit::MaterializedOccurrences,
        ExpansionLimit::RetainedBytes,
        ExpansionLimit::EventPayload,
        ExpansionLimit::EventChildren,
    ] {
        assert!(!limit.to_string().is_empty());
    }
}
