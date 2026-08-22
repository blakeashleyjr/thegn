use super::*;
use chrono::NaiveDate;

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
fn a_zero_interval_cannot_produce_an_infinite_loop() {
    // INTERVAL=0 is invalid; treating it literally would never advance.
    let r = RRule::parse("FREQ=DAILY;INTERVAL=0").unwrap();
    assert_eq!(r.interval, 1);
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
        rules: vec![RRule::parse("FREQ=MONTHLY;BYDAY=-1FR").unwrap()],
        rdates: vec![EventTime::Date {
            date: d(2026, 8, 1),
        }],
        exdates: vec![EventTime::Zoned {
            local: dt(2026, 9, 25, 9, 0),
            zone: TzRef::new("UTC"),
        }],
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
    // The one place the expander refuses instead of obeying: a SECONDLY rule
    // over a month is millions of instants and no UI wants them.
    let rec = Recurrence {
        rules: vec![RRule::parse("FREQ=SECONDLY").unwrap()],
        ..Default::default()
    };
    let times = expand_local(&rec, dt(2026, 8, 1, 0, 0), d(2026, 8, 1), d(2026, 12, 31));
    assert!(!times.is_empty());
    assert!(times.len() <= 200_000, "bounded: {}", times.len());
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
fn an_out_of_range_by_value_produces_nothing_rather_than_panicking() {
    // Feeds do contain nonsense; it must degrade to "no occurrence".
    for rule in [
        "FREQ=MONTHLY;BYMONTHDAY=40",
        "FREQ=MONTHLY;BYMONTHDAY=-40",
        "FREQ=YEARLY;BYYEARDAY=400",
        "FREQ=YEARLY;BYWEEKNO=60",
        "FREQ=MONTHLY;BYDAY=9MO",
        "FREQ=MONTHLY;BYDAY=-9MO",
    ] {
        let got = dates(rule, dt(2026, 1, 1, 9, 0), d(2026, 1, 1), d(2026, 12, 31));
        assert!(got.is_empty(), "{rule} produced {got:?}");
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
    // 0 is not a valid position either.
    let zero = dates(
        "FREQ=MONTHLY;BYDAY=MO;BYSETPOS=0",
        dt(2026, 1, 1, 9, 0),
        d(2026, 1, 1),
        d(2026, 3, 31),
    );
    assert!(zero.is_empty());
}

#[test]
fn a_malformed_byday_token_is_skipped() {
    // `parse_by_day` returns None for junk; the rule keeps its other parts.
    let r = RRule::parse("FREQ=WEEKLY;BYDAY=MO,XX,FR,Q").unwrap();
    assert_eq!(r.by_day.len(), 2);
    assert_eq!(r.by_day[0].weekday, Weekday::Mon);
    assert_eq!(r.by_day[1].weekday, Weekday::Fri);
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
fn an_instant_seeded_recurrence_expands_in_the_home_zone() {
    // A provider that hands back absolute timestamps still recurs sensibly.
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
    assert!(matches!(occ[0], EventTime::Zoned { .. }));
}
