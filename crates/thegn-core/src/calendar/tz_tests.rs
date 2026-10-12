use super::{
    VtzRules, WINDOWS_TZ_ALIASES, nth_weekday, parse_vtz_offset, parse_vtz_yearly_rule,
    resolve_tzid, resolve_zone,
};

#[test]
fn windows_alias_targets_are_valid_iana_zones() {
    for (alias, target) in WINDOWS_TZ_ALIASES {
        assert!(
            resolve_zone(target).is_some(),
            "invalid alias target: {target}"
        );
        assert_eq!(
            resolve_tzid(alias).map(|zone| zone.name()),
            Some(*target),
            "alias does not resolve to its target: {alias}"
        );
    }
}

#[test]
fn resolves_windows_aliases_and_vendor_paths_but_not_unknown_names() {
    assert_eq!(
        resolve_tzid("Eastern Standard Time").map(|zone| zone.name()),
        Some("America/New_York")
    );
    assert_eq!(
        resolve_tzid("W. Europe Standard Time").map(|zone| zone.name()),
        Some("Europe/Berlin")
    );
    assert_eq!(
        resolve_tzid("/mozilla.org/20050126_1/America/New_York").map(|zone| zone.name()),
        Some("America/New_York")
    );
    assert_eq!(resolve_tzid("Nowhere/Land"), None);
    assert_eq!(resolve_tzid("Mars/Olympus_Mons"), None);
}

#[test]
fn vtimezone_offset_and_yearly_rule_parsers_are_strict_and_bounded() {
    assert_eq!(parse_vtz_offset("+0530"), Some(19_800));
    assert_eq!(parse_vtz_offset("-0330"), Some(-12_600));
    for invalid in ["+05:30", "+053000", "+2400", "+0060", "0530"] {
        assert_eq!(parse_vtz_offset(invalid), None, "accepted {invalid}");
    }
    assert_eq!(
        parse_vtz_yearly_rule("FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU"),
        Some((10, -1, chrono::Weekday::Sun, None))
    );
    assert_eq!(
        parse_vtz_yearly_rule("FREQ=YEARLY;BYMONTH=3;BYDAY=2SU;UNTIL=20301231T000000"),
        Some((
            3,
            2,
            chrono::Weekday::Sun,
            Some(
                chrono::NaiveDate::from_ymd_opt(2030, 12, 31)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
            )
        ))
    );
    for invalid in [
        "FREQ=MONTHLY;BYMONTH=3;BYDAY=-1SU",
        "FREQ=YEARLY;BYMONTH=3,4;BYDAY=-1SU",
        "FREQ=YEARLY;BYMONTH=3;BYDAY=SU",
        "FREQ=YEARLY;BYMONTH=3;BYDAY=6SU",
        "FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU;COUNT=3",
    ] {
        assert_eq!(parse_vtz_yearly_rule(invalid), None, "accepted {invalid}");
    }
}

#[test]
fn vtimezone_rules_evaluate_nth_weekday_onsets_and_prefer_the_default_zone() {
    let standard = VtzRules::observance(
        "19701025T030000",
        "+0200",
        "+0100",
        Some("FREQ=YEARLY;BYMONTH=10;BYDAY=-1SU"),
        false,
    )
    .unwrap();
    let daylight = VtzRules::observance(
        "19700329T020000",
        "+0100",
        "+0200",
        Some("FREQ=YEARLY;BYMONTH=3;BYDAY=-1SU"),
        false,
    )
    .unwrap();
    let rules = VtzRules::new(vec![daylight, standard]).unwrap();
    assert_eq!(
        rules.match_zone(2025, Some(chrono_tz::Europe__Berlin)),
        Some(chrono_tz::Europe__Berlin)
    );
    assert_eq!(
        rules.match_zone(2025, Some(chrono_tz::Europe__Paris)),
        Some(chrono_tz::Europe__Paris)
    );
    let fixed = VtzRules::new(vec![
        VtzRules::observance("20250101T000000", "+0000", "+0000", None, false).unwrap(),
    ])
    .unwrap();
    assert_eq!(fixed.match_zone(2025, None), Some(chrono_tz::Etc__GMT));
    assert_eq!(
        nth_weekday(2025, 10, -1, chrono::Weekday::Sun).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2025, 10, 26).unwrap()
    );
    assert_eq!(
        nth_weekday(2025, 3, 2, chrono::Weekday::Sun).unwrap(),
        chrono::NaiveDate::from_ymd_opt(2025, 3, 9).unwrap()
    );
    assert!(VtzRules::new(Vec::new()).is_none());
    assert!(
        VtzRules::new(
            (0..9)
                .map(
                    |_| VtzRules::observance("20250101T000000", "+0000", "+0000", None, false)
                        .unwrap()
                )
                .collect()
        )
        .is_none()
    );
    assert!(VtzRules::observance("20250101T000000", "+0000", "+0000", None, true).is_none());
}

#[test]
fn old_one_shot_onset_does_not_override_preferred_zone_history() {
    let rules = VtzRules::new(vec![
        VtzRules::observance("16010101T000000", "+0400", "+0400", None, false).unwrap(),
    ])
    .unwrap();
    assert_eq!(
        rules.match_zone(2025, Some(chrono_tz::Asia__Dubai)),
        Some(chrono_tz::Asia__Dubai)
    );
}
