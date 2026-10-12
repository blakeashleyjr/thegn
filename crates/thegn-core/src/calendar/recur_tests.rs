use super::*;
use crate::calendar::{GapPolicy, TzRef};
use chrono::{NaiveDate, Timelike};

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}
fn dt(y: i32, m: u32, day: u32, h: u32, mi: u32) -> NaiveDateTime {
    d(y, m, day).and_hms_opt(h, mi, 0).unwrap()
}

/// Expand a rule string from `start` over a window, returning dates only.
fn dates(rule: &str, start: NaiveDateTime, from: NaiveDate, to: NaiveDate) -> Vec<NaiveDate> {
    let rec = Recurrence {
        rules: vec![RRule::parse(rule).unwrap()],
        ..Default::default()
    };
    expand_local(&rec, start, from, to)
        .into_iter()
        .map(|t| t.date())
        .collect()
}

// --- parsing ----------------------------------------------------------------

#[test]
fn parses_a_full_rule_and_round_trips_it() {
    let r = RRule::parse(
        "RRULE:FREQ=MONTHLY;INTERVAL=2;BYDAY=-1FR;BYSETPOS=-1;BYMONTH=3,6;WKST=SU;COUNT=5",
    )
    .unwrap();
    assert_eq!(r.freq, Freq::Monthly);
    assert_eq!(r.interval, 2);
    assert_eq!(r.count, Some(5));
    assert_eq!(
        r.by_day,
        vec![ByDay {
            nth: Some(-1),
            weekday: Weekday::Fri
        }]
    );
    assert_eq!(r.by_set_pos, vec![-1]);
    assert_eq!(r.by_month, vec![3, 6]);
    assert_eq!(r.wkst, Weekday::Sun);
    // Every part survives a round trip — a rule the expander can't fully honour
    // must still come back out of the cache intact.
    assert_eq!(RRule::parse(&r.to_rrule()).unwrap(), r);
}

#[test]
fn a_rule_without_freq_is_rejected() {
    assert!(matches!(
        RRule::parse("INTERVAL=2;BYDAY=MO"),
        Err(RecurError::BadFreq(_))
    ));
    assert!(matches!(
        RRule::parse("FREQ=FORTNIGHTLY"),
        Err(RecurError::BadFreq(_))
    ));
}

#[test]
fn a_zero_interval_is_rejected_not_rewritten() {
    // INTERVAL=0 is invalid; it neither advances nor may silently become 1.
    assert!(matches!(
        RRule::parse("FREQ=DAILY;INTERVAL=0"),
        Err(RecurError::OutOfRange(_))
    ));
    assert!(matches!(
        RRule::parse("FREQ=DAILY;INTERVAL=x"),
        Err(RecurError::BadValue(_))
    ));
    assert_eq!(
        RRule::parse("FREQ=DAILY;INTERVAL=4294967295")
            .unwrap()
            .interval,
        u32::MAX
    );
}

#[test]
fn unknown_parts_are_ignored_rather_than_fatal() {
    let r = RRule::parse("FREQ=DAILY;X-SOMETHING=7;INTERVAL=3").unwrap();
    assert_eq!(r.freq, Freq::Daily);
    assert_eq!(r.interval, 3);
}

#[test]
fn ics_datetimes_parse_in_all_three_shapes() {
    assert_eq!(parse_ics_datetime("20260821"), Some(dt(2026, 8, 21, 0, 0)));
    assert_eq!(
        parse_ics_datetime("20260821T093000"),
        Some(dt(2026, 8, 21, 9, 30))
    );
    assert_eq!(
        parse_ics_datetime("20260821T093000Z"),
        Some(dt(2026, 8, 21, 9, 30))
    );
    assert!(parse_ics_datetime("nonsense").is_none());
}

// --- the classic RFC traps --------------------------------------------------

#[test]
fn bymonthday_31_skips_short_months_it_does_not_clamp() {
    // THE most-failed RFC 5545 rule. Clamping would invent meetings on Feb 28,
    // Apr 30, Jun 30, Sep 30 and Nov 30 that the calendar does not have.
    let got = dates(
        "FREQ=MONTHLY;BYMONTHDAY=31",
        dt(2026, 1, 31, 9, 0),
        d(2026, 1, 1),
        d(2026, 12, 31),
    );
    assert_eq!(
        got,
        vec![
            d(2026, 1, 31),
            d(2026, 3, 31),
            d(2026, 5, 31),
            d(2026, 7, 31),
            d(2026, 8, 31),
            d(2026, 10, 31),
            d(2026, 12, 31),
        ]
    );
}

#[test]
fn a_monthly_rule_seeded_on_the_31st_also_skips_short_months() {
    // Same rule, implied rather than spelled out via BYMONTHDAY.
    let got = dates(
        "FREQ=MONTHLY",
        dt(2026, 1, 31, 9, 0),
        d(2026, 1, 1),
        d(2026, 6, 30),
    );
    assert_eq!(
        got,
        vec![d(2026, 1, 31), d(2026, 3, 31), d(2026, 5, 31)],
        "February and April have no 31st"
    );
}

#[test]
fn a_yearly_feb_29_rule_only_fires_in_leap_years() {
    let got = dates(
        "FREQ=YEARLY",
        dt(2024, 2, 29, 9, 0),
        d(2024, 1, 1),
        d(2032, 12, 31),
    );
    assert_eq!(
        got,
        vec![d(2024, 2, 29), d(2028, 2, 29), d(2032, 2, 29)],
        "never clamped to the 28th"
    );
}

#[test]
fn negative_byday_selects_from_the_end_of_the_month() {
    // "the last Friday of the month"
    let got = dates(
        "FREQ=MONTHLY;BYDAY=-1FR",
        dt(2026, 1, 30, 9, 0),
        d(2026, 1, 1),
        d(2026, 4, 30),
    );
    assert_eq!(
        got,
        vec![
            d(2026, 1, 30),
            d(2026, 2, 27),
            d(2026, 3, 27),
            d(2026, 4, 24)
        ]
    );
}

#[test]
fn positive_nth_byday_selects_from_the_start() {
    // "the second Tuesday" — patch Tuesday.
    let got = dates(
        "FREQ=MONTHLY;BYDAY=2TU",
        dt(2026, 1, 13, 9, 0),
        d(2026, 1, 1),
        d(2026, 3, 31),
    );
    assert_eq!(got, vec![d(2026, 1, 13), d(2026, 2, 10), d(2026, 3, 10)]);
}

#[test]
fn bysetpos_picks_from_the_periods_candidate_list() {
    // "the last WEEKDAY of the month" — the canonical BYSETPOS example.
    let got = dates(
        "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1",
        dt(2026, 1, 30, 9, 0),
        d(2026, 1, 1),
        d(2026, 5, 31),
    );
    assert_eq!(
        got,
        vec![
            d(2026, 1, 30), // Fri
            d(2026, 2, 27), // Fri
            d(2026, 3, 31), // Tue
            d(2026, 4, 30), // Thu
            d(2026, 5, 29), // Fri
        ]
    );
}

#[test]
fn bysetpos_also_counts_from_the_front() {
    let got = dates(
        "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=1",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2026, 3, 31),
    );
    assert_eq!(got, vec![d(2026, 1, 1), d(2026, 2, 2), d(2026, 3, 2)]);
}

#[test]
fn count_and_exdate_interact_the_way_the_rfc_says() {
    // A famous gotcha: an EXCLUDED instance still CONSUMES the COUNT budget, so
    // this yields four dates, not five.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;COUNT=5").unwrap()],
        exdates: vec![EventTime::Zoned {
            local: dt(2026, 8, 3, 9, 0),
            zone: TzRef::new("UTC"),
        }],
        ..Default::default()
    };
    let got: Vec<NaiveDate> =
        expand_local(&rec, dt(2026, 8, 1, 9, 0), d(2026, 8, 1), d(2026, 8, 31))
            .into_iter()
            .map(|t| t.date())
            .collect();
    assert_eq!(
        got,
        vec![d(2026, 8, 1), d(2026, 8, 2), d(2026, 8, 4), d(2026, 8, 5)],
        "the 3rd is excluded but still spent one of the five"
    );
}

#[test]
fn until_is_inclusive_and_stops_the_series() {
    let got = dates(
        "FREQ=DAILY;UNTIL=20260805T090000Z",
        dt(2026, 8, 1, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    assert_eq!(got.last(), Some(&d(2026, 8, 5)));
    assert_eq!(got.len(), 5);
}

#[test]
fn an_endless_rule_is_bounded_by_the_query_window() {
    // No COUNT, no UNTIL: expansion must still terminate, and cheaply.
    let got = dates(
        "FREQ=DAILY",
        dt(2020, 1, 1, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    assert_eq!(got.len(), 31, "one per day of the window, and no more");
    assert_eq!(got[0], d(2026, 8, 1));
    assert_eq!(got[30], d(2026, 8, 31));
}

#[test]
fn occurrences_before_dtstart_are_never_emitted() {
    let got = dates(
        "FREQ=DAILY",
        dt(2026, 8, 15, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    assert_eq!(got.first(), Some(&d(2026, 8, 15)));
    assert_eq!(got.len(), 17);
}

// --- the ordinary shapes ----------------------------------------------------

#[test]
fn weekly_with_byday_expands_within_each_week() {
    let got = dates(
        "FREQ=WEEKLY;BYDAY=MO,WE,FR",
        dt(2026, 8, 3, 9, 0), // a Monday
        d(2026, 8, 1),
        d(2026, 8, 14),
    );
    assert_eq!(
        got,
        vec![
            d(2026, 8, 3),
            d(2026, 8, 5),
            d(2026, 8, 7),
            d(2026, 8, 10),
            d(2026, 8, 12),
            d(2026, 8, 14),
        ]
    );
}

#[test]
fn weekly_without_byday_repeats_on_dtstarts_weekday() {
    let got = dates(
        "FREQ=WEEKLY",
        dt(2026, 8, 5, 9, 0), // a Wednesday
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    assert!(got.iter().all(|d| d.weekday() == Weekday::Wed));
    assert_eq!(got.len(), 4);
}

#[test]
fn interval_skips_periods() {
    let biweekly = dates(
        "FREQ=WEEKLY;INTERVAL=2",
        dt(2026, 8, 3, 9, 0),
        d(2026, 8, 1),
        d(2026, 9, 30),
    );
    assert_eq!(
        biweekly,
        vec![
            d(2026, 8, 3),
            d(2026, 8, 17),
            d(2026, 8, 31),
            d(2026, 9, 14),
            d(2026, 9, 28)
        ]
    );
    let every_third_day = dates(
        "FREQ=DAILY;INTERVAL=3",
        dt(2026, 8, 1, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 10),
    );
    assert_eq!(
        every_third_day,
        vec![d(2026, 8, 1), d(2026, 8, 4), d(2026, 8, 7), d(2026, 8, 10)]
    );
}

#[test]
fn yearly_with_bymonth_and_bymonthday_is_an_anniversary() {
    let got = dates(
        "FREQ=YEARLY;BYMONTH=12;BYMONTHDAY=25",
        dt(2026, 12, 25, 0, 0),
        d(2026, 1, 1),
        d(2028, 12, 31),
    );
    assert_eq!(got, vec![d(2026, 12, 25), d(2027, 12, 25), d(2028, 12, 25)]);
}

#[test]
fn yearly_with_nth_byday_finds_the_us_thanksgiving_shape() {
    // Fourth Thursday of November.
    let got = dates(
        "FREQ=YEARLY;BYMONTH=11;BYDAY=4TH",
        dt(2026, 11, 26, 0, 0),
        d(2026, 1, 1),
        d(2028, 12, 31),
    );
    assert_eq!(got, vec![d(2026, 11, 26), d(2027, 11, 25), d(2028, 11, 23)]);
}

#[test]
fn byyearday_supports_negative_indices() {
    // -1 is the last day of the year, leap or not.
    let got = dates(
        "FREQ=YEARLY;BYYEARDAY=-1",
        dt(2026, 12, 31, 0, 0),
        d(2026, 1, 1),
        d(2028, 12, 31),
    );
    assert_eq!(got, vec![d(2026, 12, 31), d(2027, 12, 31), d(2028, 12, 31)]);
}

#[test]
fn byweekno_selects_iso_weeks() {
    let got = dates(
        "FREQ=YEARLY;BYWEEKNO=1;BYDAY=MO",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2027, 12, 31),
    );
    // The Monday of ISO week 1 in each year.
    assert!(got.iter().all(|d| d.weekday() == Weekday::Mon));
    assert!(got.contains(&NaiveDate::from_isoywd_opt(2027, 1, Weekday::Mon).unwrap()));
}

#[test]
fn byhour_and_byminute_expand_times_within_a_day() {
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;BYHOUR=9,17;BYMINUTE=0,30").unwrap()],
        ..Default::default()
    };
    let got = expand_local(&rec, dt(2026, 8, 1, 9, 0), d(2026, 8, 1), d(2026, 8, 1));
    assert_eq!(
        got,
        vec![
            dt(2026, 8, 1, 9, 0),
            dt(2026, 8, 1, 9, 30),
            dt(2026, 8, 1, 17, 0),
            dt(2026, 8, 1, 17, 30),
        ]
    );
}

#[test]
fn an_unspecified_time_is_inherited_from_dtstart() {
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        ..Default::default()
    };
    let got = expand_local(&rec, dt(2026, 8, 1, 14, 45), d(2026, 8, 1), d(2026, 8, 2));
    assert_eq!(got, vec![dt(2026, 8, 1, 14, 45), dt(2026, 8, 2, 14, 45)]);
}

#[test]
fn rdates_add_occurrences_the_rules_never_produce() {
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=WEEKLY;BYDAY=MO").unwrap()],
        rdates: vec![EventTime::Zoned {
            local: dt(2026, 8, 5, 9, 0), // a Wednesday
            zone: TzRef::new("UTC"),
        }],
        ..Default::default()
    };
    let got: Vec<NaiveDate> =
        expand_local(&rec, dt(2026, 8, 3, 9, 0), d(2026, 8, 1), d(2026, 8, 10))
            .into_iter()
            .map(|t| t.date())
            .collect();
    assert_eq!(got, vec![d(2026, 8, 3), d(2026, 8, 5), d(2026, 8, 10)]);
}

#[test]
fn a_non_recurring_event_is_its_own_single_occurrence() {
    let rec = Recurrence::default();
    assert!(rec.is_empty());
    let got = expand_local(&rec, dt(2026, 8, 21, 9, 0), d(2026, 8, 1), d(2026, 8, 31));
    assert_eq!(got, vec![dt(2026, 8, 21, 9, 0)]);
    // ...and nothing at all outside the window.
    assert!(expand_local(&rec, dt(2026, 8, 21, 9, 0), d(2026, 9, 1), d(2026, 9, 30)).is_empty());
}

// --- DST correctness (the reason wall time is stored, not instants) ----------

#[test]
fn a_weekly_meeting_keeps_its_wall_time_across_a_dst_boundary() {
    // 09:00 America/Chicago is 14:00Z in winter and 13:00Z in summer. Advancing
    // by a fixed 7×86400s would drift the local time by an hour instead.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=WEEKLY;BYDAY=SU").unwrap()],
        ..Default::default()
    };
    let start = EventTime::Zoned {
        local: dt(2026, 3, 1, 9, 0),
        zone: TzRef::new("America/Chicago"),
    };
    let occ = occurrences(
        &rec,
        &start,
        d(2026, 3, 1),
        d(2026, 3, 22),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    // Every occurrence is still 09:00 local...
    for o in &occ {
        match o {
            EventTime::Zoned { local, .. } => assert_eq!(local.hour(), 9),
            other => panic!("expected zoned, got {other:?}"),
        }
    }
    // ...but the instants straddle the March 8 transition.
    let utc: Vec<u32> = occ
        .iter()
        .map(|o| {
            o.instant_in(chrono_tz::Tz::UTC, GapPolicy::ShiftForward)
                .unwrap()
                .hour()
        })
        .collect();
    assert_eq!(utc, vec![15, 14, 14, 14], "CST 15:00Z then CDT 14:00Z");
}

#[test]
fn a_daily_meeting_inside_the_spring_forward_gap_shifts_forward() {
    // 02:30 does not exist on 2026-03-08 in New York.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        ..Default::default()
    };
    let start = EventTime::Zoned {
        local: dt(2026, 3, 7, 2, 30),
        zone: TzRef::new("America/New_York"),
    };
    let occ = occurrences(
        &rec,
        &start,
        d(2026, 3, 7),
        d(2026, 3, 9),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    assert_eq!(occ.len(), 3, "the skipped hour doesn't delete the day");
    let inst: Vec<_> = occ
        .iter()
        .map(|o| {
            o.instant_in(chrono_tz::Tz::UTC, GapPolicy::ShiftForward)
                .unwrap()
        })
        .collect();
    // 07:30Z on the 8th is 03:30 EDT — shifted past the gap, keeping :30.
    assert_eq!(inst[1].hour(), 7);
    assert_eq!(inst[1].minute(), 30);
}

#[test]
fn a_weekly_meeting_in_the_repeated_fall_back_hour_takes_the_earlier_instant() {
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=WEEKLY;BYDAY=SU").unwrap()],
        ..Default::default()
    };
    let start = EventTime::Zoned {
        local: dt(2026, 11, 1, 1, 30),
        zone: TzRef::new("America/New_York"),
    };
    let occ = occurrences(
        &rec,
        &start,
        d(2026, 11, 1),
        d(2026, 11, 1),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    let at = occ[0]
        .instant_in(chrono_tz::Tz::UTC, GapPolicy::ShiftForward)
        .unwrap();
    assert_eq!(at.hour(), 5, "05:30Z — the EDT reading, not the EST one");
}

#[test]
fn an_all_day_recurrence_stays_a_floating_date() {
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=YEARLY").unwrap()],
        ..Default::default()
    };
    let start = EventTime::Date {
        date: d(2026, 12, 25),
    };
    let occ = occurrences(
        &rec,
        &start,
        d(2026, 1, 1),
        d(2028, 12, 31),
        chrono_tz::Tz::Pacific__Auckland,
        GapPolicy::ShiftForward,
    );
    assert_eq!(occ.len(), 3);
    // Never promoted to a zoned time — Christmas is Dec 25 everywhere.
    for o in &occ {
        assert!(matches!(o, EventTime::Date { .. }), "got {o:?}");
    }
}

// --- serde, display, and the reduced-frequency shapes -----------------------

#[test]
fn a_rule_serializes_as_its_ical_string() {
    // The wire format is the spelling every calendar tool speaks, and one a
    // shell plugin can print by hand — not a JSON object of twelve arrays.
    let r = RRule::parse("FREQ=WEEKLY;BYDAY=MO,WE;INTERVAL=2;COUNT=10").unwrap();
    let json = serde_json::to_string(&r).unwrap();
    assert!(
        json.starts_with('"') && json.contains("FREQ=WEEKLY"),
        "{json}"
    );
    let back: RRule = serde_json::from_str(&json).unwrap();
    assert_eq!(back, r);
}

#[test]
fn deserializing_a_malformed_rule_is_an_error_not_a_panic() {
    assert!(serde_json::from_str::<RRule>("\"INTERVAL=2\"").is_err());
    assert!(serde_json::from_str::<RRule>("\"FREQ=DAILY\"").is_ok());
}

#[test]
fn a_whole_recurrence_round_trips_through_json() {
    let rec = Recurrence {
        rules: vec![
            RRule::parse("FREQ=MONTHLY;BYDAY=-1FR;UNTIL=20260831").unwrap(),
            RRule::parse("FREQ=WEEKLY;UNTIL=20260925T090000").unwrap(),
            RRule::parse("FREQ=DAILY;UNTIL=20261001T000000Z").unwrap(),
        ],
        rdates: vec![
            EventTime::Date {
                date: d(2026, 8, 1),
            },
            EventTime::Zoned {
                local: dt(2026, 8, 2, 9, 0),
                zone: TzRef::new("America/Chicago"),
            },
            utc(2026, 8, 3, 14, 0),
        ],
        exdates: vec![
            EventTime::Date {
                date: d(2026, 9, 24),
            },
            EventTime::Zoned {
                local: dt(2026, 9, 25, 9, 0),
                zone: TzRef::new("UTC"),
            },
            utc(2026, 9, 26, 14, 0),
        ],
    };
    let s = serde_json::to_string(&rec).unwrap();
    assert_eq!(serde_json::from_str::<Recurrence>(&s).unwrap(), rec);
    // An absent recurrence deserializes to the empty default.
    assert!(serde_json::from_str::<Recurrence>("{}").unwrap().is_empty());
}

#[test]
fn every_frequency_round_trips_its_name() {
    for name in [
        "SECONDLY", "MINUTELY", "HOURLY", "DAILY", "WEEKLY", "MONTHLY", "YEARLY",
    ] {
        let f = Freq::parse(name).unwrap();
        assert_eq!(f.as_str(), name);
        assert_eq!(
            RRule::parse(&format!("FREQ={name}")).unwrap().freq,
            f,
            "{name}"
        );
    }
    assert!(Freq::parse("fortnightly").is_none());
    // Lower case is accepted — real feeds are not always upper.
    assert_eq!(Freq::parse("weekly"), Some(Freq::Weekly));
}

#[test]
fn recurrence_errors_render_readably() {
    let e = RRule::parse("BYDAY=MO").unwrap_err();
    assert!(e.to_string().contains("FREQ"), "{e}");
    let v = RecurError::BadValue("xyz".into());
    assert!(v.to_string().contains("xyz"));
}

#[test]
fn sub_daily_frequencies_step_the_clock_not_the_calendar() {
    // `HOURLY;INTERVAL=24` means every 24 HOURS. Treating the interval as days
    // (which the calendar-stepping path would) turns it into every 24 days.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=HOURLY;INTERVAL=24").unwrap()],
        ..Default::default()
    };
    let times = expand_local(&rec, dt(2026, 8, 1, 9, 0), d(2026, 8, 1), d(2026, 8, 3));
    assert_eq!(
        times,
        vec![
            dt(2026, 8, 1, 9, 0),
            dt(2026, 8, 2, 9, 0),
            dt(2026, 8, 3, 9, 0)
        ]
    );

    // A three-hour cadence within one day.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=HOURLY;INTERVAL=3;COUNT=4").unwrap()],
        ..Default::default()
    };
    let times = expand_local(&rec, dt(2026, 8, 1, 9, 0), d(2026, 8, 1), d(2026, 8, 1));
    assert_eq!(
        times,
        vec![
            dt(2026, 8, 1, 9, 0),
            dt(2026, 8, 1, 12, 0),
            dt(2026, 8, 1, 15, 0),
            dt(2026, 8, 1, 18, 0)
        ]
    );
}

#[test]
fn a_by_part_finer_than_the_frequency_filters_rather_than_expanding() {
    // RFC 5545: below the frequency, BY* limits. An HOURLY rule with BYHOUR
    // keeps only those hours; it does not multiply them out.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=HOURLY;BYHOUR=9,17").unwrap()],
        ..Default::default()
    };
    let times = expand_local(&rec, dt(2026, 8, 1, 0, 0), d(2026, 8, 1), d(2026, 8, 2));
    assert_eq!(
        times,
        vec![
            dt(2026, 8, 1, 9, 0),
            dt(2026, 8, 1, 17, 0),
            dt(2026, 8, 2, 9, 0),
            dt(2026, 8, 2, 17, 0)
        ]
    );

    // BYDAY filters an hourly rule down to chosen weekdays.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=HOURLY;INTERVAL=24;BYDAY=MO").unwrap()],
        ..Default::default()
    };
    let times = expand_local(&rec, dt(2026, 8, 1, 9, 0), d(2026, 8, 1), d(2026, 8, 14));
    assert!(
        times.iter().all(|t| t.weekday() == Weekday::Mon),
        "{times:?}"
    );
    assert_eq!(times.len(), 2);
}

#[test]
fn a_secondly_rule_is_bounded_rather_than_running_away() {
    // A SECONDLY rule over months is millions of instants and no UI wants
    // them.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=SECONDLY").unwrap()],
        ..Default::default()
    };
    // Refused as a typed error — never a silently truncated list that would
    // read as the complete answer. The retained-instant ceiling bites first:
    // every expanded local is charged before the vector holding it grows.
    let mut budget = ExpansionBudget::default();
    assert_eq!(
        expand_local_bounded(
            &rec,
            dt(2026, 8, 1, 0, 0),
            d(2026, 8, 1),
            d(2026, 12, 31),
            &mut budget
        ),
        Err(ExpansionError::Budget(
            ExpansionLimit::MaterializedOccurrences
        ))
    );
    // A day of it fits, and is complete.
    let mut budget = ExpansionBudget::default();
    let day = expand_local_bounded(
        &rec,
        dt(2026, 8, 1, 0, 0),
        d(2026, 8, 1),
        d(2026, 8, 1),
        &mut budget,
    )
    .unwrap();
    assert_eq!(day.len(), 86_400);
}

// --- BY* parts acting as FILTERS on the finer frequencies -------------------

#[test]
fn a_daily_rule_is_filtered_by_bymonth() {
    // On DAILY, BYMONTH filters rather than expands.
    let got = dates(
        "FREQ=DAILY;BYMONTH=8",
        dt(2026, 7, 29, 9, 0),
        d(2026, 7, 29),
        d(2026, 8, 3),
    );
    assert_eq!(
        got,
        vec![d(2026, 8, 1), d(2026, 8, 2), d(2026, 8, 3)],
        "July is filtered out"
    );
}

#[test]
fn a_daily_rule_is_filtered_by_bymonthday_including_from_the_end() {
    let got = dates(
        "FREQ=DAILY;BYMONTHDAY=1,-1",
        dt(2026, 8, 1, 9, 0),
        d(2026, 8, 1),
        d(2026, 9, 30),
    );
    assert_eq!(
        got,
        vec![d(2026, 8, 1), d(2026, 8, 31), d(2026, 9, 1), d(2026, 9, 30)],
        "-1 is the last day of each month"
    );
}

#[test]
fn a_daily_rule_is_filtered_by_byday() {
    let got = dates(
        "FREQ=DAILY;BYDAY=SA,SU",
        dt(2026, 8, 1, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 10),
    );
    assert!(
        got.iter()
            .all(|x| matches!(x.weekday(), Weekday::Sat | Weekday::Sun)),
        "{got:?}"
    );
    assert_eq!(got.len(), 4);
}

#[test]
fn a_daily_rule_is_filtered_by_byyearday_including_negative() {
    let got = dates(
        "FREQ=DAILY;BYYEARDAY=-1",
        dt(2026, 12, 1, 9, 0),
        d(2026, 12, 1),
        d(2026, 12, 31),
    );
    assert_eq!(got, vec![d(2026, 12, 31)]);
    // A leap year's last day is ordinal 366, so a hard-coded 365 would miss it.
    let leap = dates(
        "FREQ=DAILY;BYYEARDAY=-1",
        dt(2028, 12, 1, 9, 0),
        d(2028, 12, 1),
        d(2028, 12, 31),
    );
    assert_eq!(leap, vec![d(2028, 12, 31)]);
}

#[test]
fn a_daily_rule_is_filtered_by_byweekno() {
    let got = dates(
        "FREQ=DAILY;BYWEEKNO=32",
        dt(2026, 8, 1, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    assert!(!got.is_empty());
    assert!(got.iter().all(|x| x.iso_week().week() == 32), "{got:?}");
    // A negative week counts back from the year's last ISO week.
    let last = dates(
        "FREQ=DAILY;BYWEEKNO=-1",
        dt(2026, 12, 1, 9, 0),
        d(2026, 12, 1),
        d(2026, 12, 31),
    );
    assert!(!last.is_empty());
}

#[test]
fn wkst_shifts_which_days_a_weekly_rule_groups_together() {
    // WKST only matters once INTERVAL > 1, but the week's start still decides
    // which seven days a period covers.
    let mon = dates(
        "FREQ=WEEKLY;BYDAY=SU;WKST=MO",
        dt(2026, 8, 3, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    let sun = dates(
        "FREQ=WEEKLY;BYDAY=SU;WKST=SU",
        dt(2026, 8, 3, 9, 0),
        d(2026, 8, 1),
        d(2026, 8, 31),
    );
    assert!(mon.iter().all(|x| x.weekday() == Weekday::Sun));
    assert!(sun.iter().all(|x| x.weekday() == Weekday::Sun));
}

#[test]
fn monthly_byday_and_bymonthday_together_intersect() {
    // RFC 5545: with both present the rule is their intersection — "a Friday
    // that is also the 13th".
    let got = dates(
        "FREQ=MONTHLY;BYDAY=FR;BYMONTHDAY=13",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2026, 12, 31),
    );
    assert!(!got.is_empty());
    for x in &got {
        assert_eq!(x.weekday(), Weekday::Fri);
        assert_eq!(x.day(), 13);
    }
}

#[test]
fn a_monthly_rule_can_be_restricted_by_bymonth() {
    let got = dates(
        "FREQ=MONTHLY;BYMONTHDAY=1;BYMONTH=3,6,9,12",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2026, 12, 31),
    );
    assert_eq!(
        got,
        vec![d(2026, 3, 1), d(2026, 6, 1), d(2026, 9, 1), d(2026, 12, 1)]
    );
}

#[test]
fn a_yearly_rule_with_plain_byday_takes_every_such_weekday_in_the_month() {
    let got = dates(
        "FREQ=YEARLY;BYMONTH=2;BYDAY=MO",
        dt(2026, 2, 2, 9, 0),
        d(2026, 1, 1),
        d(2026, 12, 31),
    );
    assert!(got.len() >= 4, "every Monday in February: {got:?}");
    assert!(
        got.iter()
            .all(|x| x.month() == 2 && x.weekday() == Weekday::Mon)
    );
}

#[test]
fn a_yearly_rule_can_intersect_byday_with_bymonthday() {
    let got = dates(
        "FREQ=YEARLY;BYMONTH=8;BYDAY=FR;BYMONTHDAY=21",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2027, 12, 31),
    );
    assert_eq!(got, vec![d(2026, 8, 21)], "2027-08-21 is a Saturday");
}

#[test]
fn an_out_of_range_by_value_is_rejected_at_parse() {
    // RFC 5545 domains are enforced up front; nothing is clamped or dropped.
    for rule in [
        "FREQ=MONTHLY;BYMONTHDAY=40",
        "FREQ=MONTHLY;BYMONTHDAY=-40",
        "FREQ=MONTHLY;BYMONTHDAY=0",
        "FREQ=YEARLY;BYYEARDAY=400",
        "FREQ=YEARLY;BYYEARDAY=0",
        "FREQ=YEARLY;BYWEEKNO=60",
        "FREQ=YEARLY;BYWEEKNO=0",
        "FREQ=MONTHLY;BYDAY=54MO",
        "FREQ=MONTHLY;BYDAY=-54MO",
        "FREQ=MONTHLY;BYDAY=0MO",
        "FREQ=DAILY;BYHOUR=24",
        "FREQ=DAILY;BYHOUR=-1",
        "FREQ=DAILY;BYMINUTE=60",
        "FREQ=DAILY;BYSECOND=61",
        "FREQ=YEARLY;BYMONTH=0",
        "FREQ=YEARLY;BYMONTH=13",
        "FREQ=MONTHLY;BYDAY=MO;BYSETPOS=0",
        "FREQ=MONTHLY;BYDAY=MO;BYSETPOS=367",
        "FREQ=MONTHLY;BYDAY=MO;BYSETPOS=-2147483648",
        "FREQ=MONTHLY;BYSETPOS=-9223372036854775808",
        "FREQ=MONTHLY;BYMONTHDAY=-9223372036854775808",
        "FREQ=YEARLY;BYYEARDAY=-9223372036854775808",
        "FREQ=YEARLY;BYWEEKNO=-9223372036854775808",
    ] {
        assert!(
            matches!(RRule::parse(rule), Err(RecurError::OutOfRange(_))),
            "{rule} should be out of range"
        );
    }
    // The inclusive edges are accepted.
    RRule::parse("FREQ=YEARLY;BYSECOND=60;BYMINUTE=59;BYHOUR=23;BYMONTH=12").unwrap();
    RRule::parse("FREQ=YEARLY;BYMONTHDAY=-31,31;BYYEARDAY=-366,366;BYWEEKNO=-53,53").unwrap();
    RRule::parse("FREQ=YEARLY;BYDAY=-53MO,53FR;BYSETPOS=-366,366").unwrap();
}

#[test]
fn an_oversized_by_list_is_rejected() {
    let ok = vec!["1"; MAX_BY_LIST].join(",");
    RRule::parse(&format!("FREQ=DAILY;BYHOUR={ok}")).unwrap();
    for part in ["BYHOUR", "BYMONTHDAY", "BYDAY"] {
        let v = if part == "BYDAY" { "MO" } else { "1" };
        let big = vec![v; MAX_BY_LIST + 1].join(",");
        assert!(
            matches!(
                RRule::parse(&format!("FREQ=DAILY;{part}={big}")),
                Err(RecurError::OutOfRange(_))
            ),
            "{part}"
        );
    }
}

#[test]
fn a_bysetpos_outside_the_candidate_list_selects_nothing() {
    let got = dates(
        "FREQ=MONTHLY;BYDAY=MO;BYSETPOS=9",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2026, 3, 31),
    );
    assert!(got.is_empty());
}

#[test]
fn malformed_byday_count_until_and_wkst_reject_the_rule() {
    // A junk token must not silently drop out and change the rule's meaning.
    for rule in [
        "FREQ=WEEKLY;BYDAY=MO,XX,FR",
        "FREQ=WEEKLY;BYDAY=Q",
        "FREQ=DAILY;COUNT=many",
        "FREQ=DAILY;COUNT=-1",
        "FREQ=DAILY;UNTIL=soon",
        "FREQ=WEEKLY;WKST=XX",
    ] {
        assert!(
            matches!(RRule::parse(rule), Err(RecurError::BadValue(_))),
            "{rule}"
        );
    }
    let r = RRule::parse("FREQ=WEEKLY;BYDAY=MO,FR").unwrap();
    assert_eq!(r.by_day.len(), 2);
    assert!(
        RecurError::OutOfRange("BYHOUR=24".into())
            .to_string()
            .contains("BYHOUR=24")
    );
}

#[test]
fn non_ascii_byday_is_rejected_without_panicking() {
    assert!(matches!(
        RRule::parse("FREQ=WEEKLY;BYDAY=é1"),
        Err(RecurError::BadValue(_))
    ));
}

#[test]
fn a_non_numeric_by_value_is_an_error() {
    assert!(matches!(
        RRule::parse("FREQ=MONTHLY;BYMONTHDAY=first"),
        Err(RecurError::BadValue(_))
    ));
}

#[test]
fn a_part_without_an_equals_sign_is_ignored() {
    let r = RRule::parse("FREQ=DAILY;JUNK;INTERVAL=2").unwrap();
    assert_eq!(r.interval, 2);
}

#[test]
fn bare_known_rule_parts_are_rejected_but_empty_and_unknown_parts_are_ignored() {
    assert_eq!(
        RRule::parse("FREQ=DAILY;COUNT"),
        Err(RecurError::BadValue("COUNT".to_string()))
    );
    assert_eq!(
        RRule::parse("FREQ=WEEKLY;UNTIL"),
        Err(RecurError::BadValue("UNTIL".to_string()))
    );
    assert_eq!(
        RRule::parse("FREQ=DAILY;;X-NAME=1").unwrap().freq,
        Freq::Daily
    );
}

#[test]
fn duplicate_known_rule_parts_are_rejected() {
    assert_eq!(
        RRule::parse("FREQ=DAILY;COUNT=2;COUNT=500"),
        Err(RecurError::BadValue("duplicate COUNT".to_string()))
    );
    assert_eq!(
        RRule::parse("FREQ=DAILY;FREQ=WEEKLY"),
        Err(RecurError::BadValue("duplicate FREQ".to_string()))
    );
}

#[test]
fn count_and_until_cannot_appear_together() {
    assert_eq!(
        RRule::parse("FREQ=DAILY;COUNT=3;UNTIL=20261231T000000Z"),
        Err(RecurError::BadValue("COUNT with UNTIL".to_string()))
    );
}

#[test]
fn serde_rejects_duplicate_rule_parts() {
    assert!(serde_json::from_str::<RRule>("\"FREQ=DAILY;COUNT=2;COUNT=500\"").is_err());
}

#[test]
fn to_rrule_emits_only_the_parts_that_were_set() {
    // A minimal rule must not round-trip into a wall of defaults.
    assert_eq!(RRule::parse("FREQ=DAILY").unwrap().to_rrule(), "FREQ=DAILY");
    let full = RRule::parse(
        "FREQ=YEARLY;INTERVAL=2;BYSECOND=0;BYMINUTE=30;BYHOUR=9;BYDAY=1MO;         BYMONTHDAY=-1;BYYEARDAY=100;BYWEEKNO=-2;BYMONTH=6;BYSETPOS=1;WKST=SU",
    )
    .unwrap();
    let s = full.to_rrule();
    for part in [
        "BYSECOND=0",
        "BYMINUTE=30",
        "BYHOUR=9",
        "BYDAY=1MO",
        "BYMONTHDAY=-1",
        "BYYEARDAY=100",
        "BYWEEKNO=-2",
        "BYMONTH=6",
        "BYSETPOS=1",
        "WKST=SU",
    ] {
        assert!(s.contains(part), "{part} missing from {s}");
    }
    assert_eq!(RRule::parse(&s).unwrap(), full);
    // UNTIL survives the round trip in its Z-suffixed form.
    let until = RRule::parse("FREQ=DAILY;UNTIL=20260805T090000Z").unwrap();
    assert!(until.to_rrule().contains("UNTIL=20260805T090000Z"));
}

#[test]
fn every_weekday_abbreviation_parses_and_renders() {
    for (tok, wd) in [
        ("MO", Weekday::Mon),
        ("TU", Weekday::Tue),
        ("WE", Weekday::Wed),
        ("TH", Weekday::Thu),
        ("FR", Weekday::Fri),
        ("SA", Weekday::Sat),
        ("SU", Weekday::Sun),
    ] {
        let r = RRule::parse(&format!("FREQ=WEEKLY;BYDAY={tok}")).unwrap();
        assert_eq!(r.by_day[0].weekday, wd, "{tok}");
        assert!(r.to_rrule().contains(&format!("BYDAY={tok}")), "{tok}");
    }
}

#[test]
fn the_gap_policy_can_drop_an_occurrence_that_does_not_exist() {
    // Under Skip, an occurrence in the spring-forward gap genuinely does not
    // happen and is omitted rather than nudged.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        ..Default::default()
    };
    let start = EventTime::Zoned {
        local: dt(2026, 3, 7, 2, 30),
        zone: TzRef::new("America/New_York"),
    };
    let shifted = occurrences(
        &rec,
        &start,
        d(2026, 3, 7),
        d(2026, 3, 9),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    let skipped = occurrences(
        &rec,
        &start,
        d(2026, 3, 7),
        d(2026, 3, 9),
        chrono_tz::Tz::UTC,
        GapPolicy::Skip,
    );
    assert_eq!(shifted.len(), 3);
    assert_eq!(skipped.len(), 2, "the 8th has no 02:30");
}

#[test]
fn an_instant_seeded_recurrence_expands_in_utc() {
    // Absolute DTSTART recurrence candidates stay UTC-shaped and UTC-timed.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;COUNT=3").unwrap()],
        ..Default::default()
    };
    let start = EventTime::Instant {
        at: chrono::DateTime::from_naive_utc_and_offset(dt(2026, 8, 21, 9, 0), chrono::Utc),
    };
    let occ = occurrences(
        &rec,
        &start,
        d(2026, 8, 1),
        d(2026, 8, 31),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    assert_eq!(occ.len(), 3);
    assert!(matches!(occ[0], EventTime::Instant { .. }));
}

#[test]
fn a_huge_count_stops_at_the_end_of_the_window() {
    // COUNT must not keep the walk alive past the requested window.
    let got = dates(
        "FREQ=DAILY;COUNT=4294967295",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2026, 1, 7),
    );
    assert_eq!(got.len(), 7);
}

// --- typed RDATE / EXDATE / UNTIL (THE-478) ----------------------------------

fn chicago_start() -> EventTime {
    EventTime::Zoned {
        local: dt(2026, 8, 3, 9, 0),
        zone: TzRef::new("America/Chicago"),
    }
}

fn utc(y: i32, m: u32, day: u32, h: u32, mi: u32) -> EventTime {
    EventTime::Instant {
        at: dt(y, m, day, h, mi).and_utc(),
    }
}

/// Expand a daily Chicago series (plus the given extras) and return the local
/// wall times of every occurrence in `[from, to]`.
fn chicago_locals(
    rule: &str,
    rdates: Vec<EventTime>,
    exdates: Vec<EventTime>,
    to: NaiveDate,
) -> Vec<NaiveDateTime> {
    let rec = Recurrence {
        rules: vec![RRule::parse(rule).unwrap()],
        rdates,
        exdates,
    };
    occurrences(
        &rec,
        &chicago_start(),
        d(2026, 8, 3),
        to,
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    )
    .into_iter()
    .map(|t| match t {
        EventTime::Zoned { local, .. } => local,
        other => panic!("expected zoned, got {other:?}"),
    })
    .collect()
}

#[test]
fn a_utc_exdate_matches_the_zoned_recurrence_id() {
    // 09:00 CDT on Aug 5 is 14:00Z. Read as a UTC wall time it would be 14:00
    // "local" and exclude nothing.
    let got = chicago_locals(
        "FREQ=DAILY",
        vec![],
        vec![utc(2026, 8, 5, 14, 0)],
        d(2026, 8, 7),
    );
    let days: Vec<u32> = got.iter().map(|t| t.day()).collect();
    assert_eq!(days, vec![3, 4, 6, 7]);
    // A different instant on that day is not the recurrence id.
    let kept = chicago_locals(
        "FREQ=DAILY",
        vec![],
        vec![utc(2026, 8, 5, 15, 0)],
        d(2026, 8, 7),
    );
    assert_eq!(kept.len(), 5);
}

#[test]
fn a_utc_exdate_tracks_the_offset_across_a_dst_transition() {
    // Chicago springs forward on 2026-03-08: 09:00 is 15:00Z before, 14:00Z after.
    let start = EventTime::Zoned {
        local: dt(2026, 3, 6, 9, 0),
        zone: TzRef::new("America/Chicago"),
    };
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        exdates: vec![utc(2026, 3, 7, 15, 0), utc(2026, 3, 9, 14, 0)],
        ..Default::default()
    };
    let got = occurrences(
        &rec,
        &start,
        d(2026, 3, 6),
        d(2026, 3, 10),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    let days: Vec<u32> = got
        .iter()
        .map(|t| match t {
            EventTime::Zoned { local, .. } => local.day(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(days, vec![6, 8, 10]);
}

#[test]
fn a_utc_exdate_matches_the_realized_instant_of_a_gap_occurrence() {
    // Nominal 02:30 is shifted to 03:30 CDT, which is 08:30Z.
    let start = EventTime::Zoned {
        local: dt(2026, 3, 7, 2, 30),
        zone: TzRef::new("America/Chicago"),
    };
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        exdates: vec![utc(2026, 3, 8, 8, 30)],
        ..Default::default()
    };
    let got = occurrences(
        &rec,
        &start,
        d(2026, 3, 7),
        d(2026, 3, 9),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    let days: Vec<u32> = got
        .iter()
        .map(|t| match t {
            EventTime::Zoned { local, .. } => local.day(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(days, vec![7, 9]);
}

#[test]
fn a_utc_until_compares_a_gap_occurrence_by_its_realized_instant() {
    // The nominal 02:30 occurrence resolves to 08:30Z. UNTIL=08:15Z is
    // later than the nominal wall time converted naïvely, but earlier as an
    // instant, so the March 8 occurrence must be excluded.
    let start = EventTime::Zoned {
        local: dt(2026, 3, 7, 2, 30),
        zone: TzRef::new("America/Chicago"),
    };
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;UNTIL=20260308T081500Z").unwrap()],
        ..Default::default()
    };
    let got = occurrences(
        &rec,
        &start,
        d(2026, 3, 7),
        d(2026, 3, 9),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    let days: Vec<u32> = got
        .iter()
        .map(|t| match t {
            EventTime::Zoned { local, .. } => local.day(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(days, vec![7]);
}

#[test]
fn a_subdaily_utc_until_keeps_a_later_candidate_after_a_dst_gap() {
    // 01:45 CST is 07:45Z, 02:30 shifts to 08:30Z, then 03:15 CDT is 08:15Z.
    // The shifted gap candidate is past UNTIL, but the following candidate is
    // still inside it and must not be lost to an early break.
    let start = EventTime::Zoned {
        local: dt(2026, 3, 8, 1, 45),
        zone: TzRef::new("America/Chicago"),
    };
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=MINUTELY;INTERVAL=45;UNTIL=20260308T082000Z").unwrap()],
        ..Default::default()
    };
    let got = occurrences(
        &rec,
        &start,
        d(2026, 3, 8),
        d(2026, 3, 8),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    let locals: Vec<NaiveDateTime> = got
        .iter()
        .map(|t| match t {
            EventTime::Zoned { local, .. } => *local,
            other => panic!("expected zoned, got {other:?}"),
        })
        .collect();
    assert_eq!(locals, vec![dt(2026, 3, 8, 1, 45), dt(2026, 3, 8, 3, 15)]);
}

#[test]
fn skipped_gap_candidates_are_not_compared_or_excluded_as_utc() {
    let gap_candidate = dt(2026, 3, 8, 2, 30);
    let rule = RRule::parse("FREQ=MINUTELY;UNTIL=20260308T020000Z").unwrap();
    assert!(!past_until(
        &rule,
        gap_candidate,
        Some(chrono_tz::Tz::America__Chicago),
        GapPolicy::Skip,
    ));

    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=MINUTELY").unwrap()],
        exdates: vec![utc(2026, 3, 8, 2, 30)],
        ..Default::default()
    };
    let got = expand_local_with_gap(
        &rec,
        gap_candidate,
        Some(chrono_tz::Tz::America__Chicago),
        d(2026, 3, 8),
        d(2026, 3, 8),
        &mut ExpansionBudget::default(),
        GapPolicy::Skip,
    )
    .unwrap();
    assert!(got.contains(&gap_candidate));
}

#[test]
fn a_utc_until_keeps_a_fold_occurrence_before_its_instant_bound() {
    // At the fall-back transition 01:30 CDT is 06:30Z. The UTC bound at
    // 07:00Z is later, despite converting to the nominal local time 01:00 CST.
    let start = EventTime::Zoned {
        local: dt(2026, 11, 1, 0, 30),
        zone: TzRef::new("America/Chicago"),
    };
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=HOURLY;UNTIL=20261101T070000Z").unwrap()],
        ..Default::default()
    };
    let got = occurrences(
        &rec,
        &start,
        d(2026, 11, 1),
        d(2026, 11, 1),
        chrono_tz::Tz::UTC,
        GapPolicy::ShiftForward,
    );
    assert!(
        got.iter().any(
            |t| matches!(t, EventTime::Zoned { local, .. } if *local == dt(2026, 11, 1, 1, 30))
        )
    );
}

#[test]
fn a_date_exdate_removes_every_occurrence_on_that_day() {
    let got = chicago_locals(
        "FREQ=DAILY;BYHOUR=9,17",
        vec![],
        vec![EventTime::Date {
            date: d(2026, 8, 4),
        }],
        d(2026, 8, 5),
    );
    let days: Vec<u32> = got.iter().map(|t| t.day()).collect();
    assert_eq!(days, vec![3, 3, 5, 5]);
}

#[test]
fn an_instant_rdate_lands_at_its_local_time_in_the_series_zone() {
    // 02:00Z on Aug 10 is 21:00 CDT on Aug 9.
    let got = chicago_locals(
        "FREQ=WEEKLY",
        vec![utc(2026, 8, 10, 2, 0)],
        vec![],
        d(2026, 8, 12),
    );
    assert!(got.contains(&dt(2026, 8, 9, 21, 0)), "{got:?}");
    assert!(!got.contains(&dt(2026, 8, 10, 2, 0)), "{got:?}");
}

#[test]
fn a_zoned_rdate_in_another_zone_converts_through_its_instant() {
    let london = EventTime::Zoned {
        local: dt(2026, 8, 7, 18, 0),
        zone: TzRef::new("Europe/London"),
    };
    // Same zone as the series, and an unknown zone, are read as wall times.
    let same = EventTime::Zoned {
        local: dt(2026, 8, 8, 7, 0),
        zone: TzRef::new("America/Chicago"),
    };
    let unknown = EventTime::Zoned {
        local: dt(2026, 8, 9, 6, 0),
        zone: TzRef::new("Nowhere/Land"),
    };
    let got = chicago_locals(
        "FREQ=WEEKLY",
        vec![london, same, unknown],
        vec![],
        d(2026, 8, 12),
    );
    // 18:00 BST is 17:00Z is 12:00 CDT.
    assert!(got.contains(&dt(2026, 8, 7, 12, 0)), "{got:?}");
    assert!(got.contains(&dt(2026, 8, 8, 7, 0)), "{got:?}");
    assert!(got.contains(&dt(2026, 8, 9, 6, 0)), "{got:?}");
}

#[test]
fn a_date_until_is_inclusive_of_its_whole_day() {
    // UNTIL=DATE covers every time that day: the 09:00 occurrence is kept.
    let got = chicago_locals("FREQ=DAILY;UNTIL=20260805", vec![], vec![], d(2026, 8, 9));
    let days: Vec<u32> = got.iter().map(|t| t.day()).collect();
    assert_eq!(days, vec![3, 4, 5]);
}

#[test]
fn a_utc_until_is_compared_as_an_instant_in_the_series_zone() {
    // 13:00Z on Aug 5 is 08:00 CDT: the 09:00 occurrence that day is after it.
    let before = chicago_locals(
        "FREQ=DAILY;UNTIL=20260805T130000Z",
        vec![],
        vec![],
        d(2026, 8, 9),
    );
    assert_eq!(before.last().map(|t| t.day()), Some(4));
    // 14:00Z is exactly 09:00 CDT, which is inclusive.
    let at = chicago_locals(
        "FREQ=DAILY;UNTIL=20260805T140000Z",
        vec![],
        vec![],
        d(2026, 8, 9),
    );
    assert_eq!(at.last().map(|t| t.day()), Some(5));
    // A floating UNTIL is already a wall time in the series zone.
    let floating = chicago_locals(
        "FREQ=DAILY;UNTIL=20260805T090000",
        vec![],
        vec![],
        d(2026, 8, 9),
    );
    assert_eq!(floating.last().map(|t| t.day()), Some(5));
}

#[test]
fn until_keeps_its_value_type_through_a_round_trip() {
    for (src, form) in [
        ("FREQ=DAILY;UNTIL=20260805", UntilForm::Date),
        ("FREQ=DAILY;UNTIL=20260805T090000", UntilForm::Floating),
        ("FREQ=DAILY;UNTIL=20260805T090000Z", UntilForm::Utc),
    ] {
        let r = RRule::parse(src).unwrap();
        assert_eq!(r.until_form, form, "{src}");
        assert_eq!(r.to_rrule(), src);
        assert_eq!(RRule::parse(&r.to_rrule()).unwrap(), r);
    }
}

#[test]
fn floating_recurrence_values_are_accepted_and_unknown_zones_are_diagnosed() {
    let start = EventTime::Zoned {
        local: dt(2030, 1, 1, 10, 0),
        zone: TzRef::new("Europe/Berlin"),
    };
    for recurrence in [
        Recurrence {
            exdates: vec![EventTime::Zoned {
                local: dt(2030, 1, 2, 10, 0),
                zone: TzRef::new(""),
            }],
            ..Default::default()
        },
        Recurrence {
            rdates: vec![EventTime::Zoned {
                local: dt(2030, 1, 2, 10, 0),
                zone: TzRef::new("  "),
            }],
            ..Default::default()
        },
    ] {
        assert_eq!(diagnose_values(&recurrence, &start), Ok(()));
        assert_eq!(check_values(&recurrence, &start), Ok(()));
    }
    let instant_start = utc(2030, 1, 1, 10, 0);
    let floating_rdate = Recurrence {
        rdates: vec![EventTime::Zoned {
            local: dt(2030, 1, 2, 10, 0),
            zone: TzRef::new(""),
        }],
        ..Default::default()
    };
    assert_eq!(diagnose_values(&floating_rdate, &instant_start), Ok(()));

    let all_day_start = EventTime::Date {
        date: d(2030, 1, 1),
    };
    let timed_exdate = Recurrence {
        exdates: vec![EventTime::Zoned {
            local: dt(2030, 1, 2, 10, 0),
            zone: TzRef::new(""),
        }],
        ..Default::default()
    };
    let error = diagnose_values(&timed_exdate, &all_day_start).unwrap_err();
    assert_eq!(error.property, RecurProperty::Exdate);
    assert_eq!(error.defect, RecurValueDefect::ShapeMismatch);

    let bad = Recurrence {
        rdates: vec![
            EventTime::Zoned {
                local: dt(2030, 1, 2, 10, 0),
                zone: TzRef::new("Europe/Berlin"),
            },
            EventTime::Zoned {
                local: dt(2030, 1, 3, 10, 0),
                zone: TzRef::new("Secret/Unknown"),
            },
        ],
        ..Default::default()
    };
    let error = diagnose_values(&bad, &start).unwrap_err();
    assert_eq!(error.property, RecurProperty::Rdate);
    assert_eq!(error.index, 1);
    assert_eq!(error.defect, RecurValueDefect::UnknownZone);
    let display = error.to_string();
    assert!(display.contains("RDATE"));
    assert!(display.contains('2'));
    assert!(!display.contains("Secret"));
    assert!(!display.contains("2030"));
}

#[test]
fn recurrence_value_diagnostics_report_shape_mismatch_and_exdate_index() {
    let timed_start = EventTime::Instant {
        at: chrono::DateTime::from_naive_utc_and_offset(dt(2030, 1, 1, 10, 0), Utc),
    };
    let rdates = Recurrence {
        rdates: vec![
            utc(2030, 1, 2, 10, 0),
            EventTime::Date {
                date: d(2030, 1, 3),
            },
        ],
        ..Default::default()
    };
    let error = diagnose_values(&rdates, &timed_start).unwrap_err();
    assert_eq!(error.property, RecurProperty::Rdate);
    assert_eq!(error.index, 1);
    assert_eq!(error.defect, RecurValueDefect::ShapeMismatch);

    let exdates = Recurrence {
        exdates: vec![
            EventTime::Zoned {
                local: dt(2030, 1, 2, 10, 0),
                zone: TzRef::new("UTC"),
            },
            EventTime::Zoned {
                local: dt(2030, 1, 3, 10, 0),
                zone: TzRef::new("Secret/Unknown"),
            },
        ],
        ..Default::default()
    };
    let error = diagnose_values(&exdates, &timed_start).unwrap_err();
    assert_eq!(error.property, RecurProperty::Exdate);
    assert_eq!(error.index, 1);
    assert_eq!(error.defect, RecurValueDefect::UnknownZone);
    assert!(!error.to_string().contains("Secret"));
}

#[test]
fn recurrence_ics_properties_round_trip_timed_and_date_sets() {
    fn parse_recurrence(start: &str, end: &str, recurrence: &Recurrence) -> Recurrence {
        let properties = recurrence.to_ics_properties().unwrap().join("\n");
        let document = format!(
            "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:roundtrip\n{start}\n{end}\n{properties}\nEND:VEVENT\nEND:VCALENDAR"
        );
        crate::calendar::parse_ics(&document, "UTC")[0]
            .recurrence
            .clone()
            .unwrap()
    }

    let timed = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;UNTIL=20300105T100000").unwrap()],
        rdates: vec![
            EventTime::Zoned {
                local: dt(2030, 1, 6, 10, 0),
                zone: TzRef::new("America/New_York"),
            },
            EventTime::Zoned {
                local: dt(2030, 1, 7, 10, 0),
                zone: TzRef::new("Europe/London"),
            },
            utc(2030, 1, 8, 15, 0),
            EventTime::Zoned {
                local: dt(2030, 1, 9, 10, 0),
                zone: TzRef::new(""),
            },
        ],
        exdates: vec![
            EventTime::Zoned {
                local: dt(2030, 1, 2, 10, 0),
                zone: TzRef::new("America/New_York"),
            },
            EventTime::Zoned {
                local: dt(2030, 1, 3, 10, 0),
                zone: TzRef::new("Europe/London"),
            },
            utc(2030, 1, 4, 15, 0),
            EventTime::Zoned {
                local: dt(2030, 1, 5, 10, 0),
                zone: TzRef::new(""),
            },
        ],
    };
    let actual = parse_recurrence(
        "DTSTART;TZID=America/New_York:20300101T100000",
        "DTEND;TZID=America/New_York:20300101T110000",
        &timed,
    );
    assert_eq!(actual, timed);

    let all_day = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;COUNT=3").unwrap()],
        rdates: vec![EventTime::Date {
            date: d(2030, 1, 5),
        }],
        exdates: vec![EventTime::Date {
            date: d(2030, 1, 2),
        }],
    };
    let actual = parse_recurrence(
        "DTSTART;VALUE=DATE:20300101",
        "DTEND;VALUE=DATE:20300102",
        &all_day,
    );
    assert_eq!(actual, all_day);
}

#[test]
fn floating_ics_values_keep_timed_events_and_expand_as_series_local() {
    let berlin = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:berlin\nDTSTART;TZID=Europe/Berlin:20300101T100000\nDTEND;TZID=Europe/Berlin:20300101T110000\nRRULE:FREQ=DAILY;COUNT=3\nEXDATE:20300102T100000\nEND:VEVENT\nEND:VCALENDAR";
    let events = crate::calendar::parse_ics(berlin, "UTC");
    assert_eq!(events.len(), 1);
    let dates = events[0]
        .occurrences(d(2030, 1, 1), d(2030, 1, 5), chrono_tz::UTC)
        .into_iter()
        .filter_map(|occurrence| match occurrence.start {
            EventTime::Zoned { local, .. } => Some(local.date()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(dates, vec![d(2030, 1, 1), d(2030, 1, 3)]);

    let floating = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nUID:floating\nDTSTART:20300101T100000\nDTEND:20300101T110000\nRRULE:FREQ=DAILY;COUNT=1\nRDATE:20300103T100000\nEND:VEVENT\nEND:VCALENDAR";
    let events = crate::calendar::parse_ics(floating, "UTC");
    assert_eq!(events.len(), 1);
    let dates = events[0]
        .occurrences(d(2030, 1, 1), d(2030, 1, 5), chrono_tz::UTC)
        .into_iter()
        .filter_map(|occurrence| match occurrence.start {
            EventTime::Zoned { local, .. } => Some(local.date()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(dates, vec![d(2030, 1, 1), d(2030, 1, 3)]);
}

#[test]
fn recurrence_writer_quotes_parameters_and_refuses_lossy_values() {
    let recurrence = Recurrence {
        rdates: vec![EventTime::Zoned {
            local: dt(2030, 1, 1, 10, 0),
            zone: TzRef::new("Area;Zone,Part:Name"),
        }],
        ..Default::default()
    };
    assert_eq!(
        recurrence.to_ics_properties().unwrap(),
        vec!["RDATE;TZID=\"Area;Zone,Part:Name\":20300101T100000"]
    );

    for zone in ["Area\"Zone", "Area\nZone"] {
        let recurrence = Recurrence {
            rdates: vec![EventTime::Zoned {
                local: dt(2030, 1, 1, 10, 0),
                zone: TzRef::new(zone),
            }],
            ..Default::default()
        };
        assert_eq!(recurrence.to_ics_properties(), None);
    }
    let recurrence = Recurrence {
        exdates: vec![EventTime::Zoned {
            local: dt(2030, 1, 1, 10, 0).with_nanosecond(1).unwrap(),
            zone: TzRef::new("UTC"),
        }],
        ..Default::default()
    };
    assert_eq!(recurrence.to_ics_properties(), None);
}

#[test]
fn instant_and_date_series_use_utc_unknown_zones_fail_and_floating_uses_home() {
    let home = chrono_tz::America::Chicago;
    assert_eq!(
        series_zone(&utc(2026, 8, 3, 14, 0), home),
        Some(chrono_tz::UTC)
    );
    assert_eq!(
        series_zone(
            &EventTime::Date {
                date: d(2026, 8, 3)
            },
            home
        ),
        Some(chrono_tz::UTC)
    );
    let unknown = EventTime::Zoned {
        local: dt(2026, 8, 3, 9, 0),
        zone: TzRef::new("Nowhere/Land"),
    };
    assert_eq!(series_zone(&unknown, home), None);
    let floating = EventTime::Zoned {
        local: dt(2026, 8, 3, 9, 0),
        zone: TzRef::default(),
    };
    assert_eq!(series_zone(&floating, home), Some(home));
    // A timed EXDATE does not match the DATE recurrence-id's UTC midnight.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY").unwrap()],
        exdates: vec![utc(2026, 8, 5, 3, 0)], // 22:00 CDT on Aug 4
        ..Default::default()
    };
    let got = occurrences(
        &rec,
        &EventTime::Date {
            date: d(2026, 8, 3),
        },
        d(2026, 8, 3),
        d(2026, 8, 5),
        home,
        GapPolicy::ShiftForward,
    );
    assert_eq!(
        got.len(),
        3,
        "an instant EXDATE does not match a date id: {got:?}"
    );
}

#[test]
fn a_date_series_utc_until_is_independent_of_the_home_zone() {
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=DAILY;UNTIL=20261015T030000Z").unwrap()],
        ..Default::default()
    };
    let start = EventTime::Date {
        date: d(2026, 10, 12),
    };
    let homes = [
        chrono_tz::Asia::Tokyo,
        chrono_tz::America::Los_Angeles,
        chrono_tz::UTC,
    ];
    let by_home: Vec<_> = homes
        .into_iter()
        .map(|home| {
            occurrences(
                &rec,
                &start,
                d(2026, 10, 10),
                d(2026, 10, 20),
                home,
                GapPolicy::ShiftForward,
            )
        })
        .collect();
    assert_eq!(by_home[0], by_home[1]);
    assert_eq!(by_home[0], by_home[2]);
    assert!(by_home[0].contains(&EventTime::Date {
        date: d(2026, 10, 15),
    }));
    assert_eq!(
        by_home[0],
        (12..=15)
            .map(|day| EventTime::Date {
                date: d(2026, 10, day),
            })
            .collect::<Vec<_>>()
    );
}
