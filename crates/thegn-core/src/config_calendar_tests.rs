use super::*;

fn account(name: &str, provider: CalendarProviderKind) -> CalendarAccount {
    CalendarAccount {
        name: name.into(),
        provider,
        ..Default::default()
    }
}

#[test]
fn defaults_are_inert_but_useful() {
    let c = CalendarConfig::default();
    // No accounts configured ⇒ no ticker slot ⇒ a user without a calendar pays
    // nothing for the feature existing.
    assert!(c.poll_secs().is_none());
    assert!(c.active_accounts().is_empty());
    assert!(c.active_clocks().is_empty());
    assert!(c.home_zone().is_none(), "empty means the system zone");
    // The grid and clocks need no provider, so the display side is on.
    assert!(c.enabled);
    assert!(c.show_six_weeks, "a fixed-height popup doesn't jitter");
    assert!(c.show_agenda);
    assert_eq!(c.week_start, WeekStart::Auto);
    assert_eq!(c.time_format, TimeFormat::Auto);
    assert!(validate_calendar(&c).is_empty());
}

#[test]
fn a_misconfigured_refresh_interval_can_never_spin() {
    // THE guard: 0 and 1 must both resolve to the floor, in the accessor, so
    // every caller inherits it rather than each remembering to clamp.
    let cfg = CalendarConfig::default();
    // A small-but-explicit interval is floored. (0 is not in this list: it
    // means "inherit the table default", covered below.)
    for raw in [1, 5, 59] {
        let a = CalendarAccount {
            refresh_interval_secs: raw,
            ..account("x", CalendarProviderKind::Ics)
        };
        assert_eq!(
            a.refresh_secs(&cfg),
            MIN_REFRESH_SECS,
            "{raw}s must be floored"
        );
    }
    // Above the floor, the account's own value is honored.
    let a = CalendarAccount {
        refresh_interval_secs: 3600,
        ..account("x", CalendarProviderKind::Ics)
    };
    assert_eq!(a.refresh_secs(&cfg), 3600);
    // 0 inherits the table default...
    let a = account("x", CalendarProviderKind::Ics);
    assert_eq!(a.refresh_secs(&cfg), 900);
    // ...which is itself floored.
    let low = CalendarConfig {
        refresh_interval_secs: 0,
        ..CalendarConfig::default()
    };
    assert_eq!(a.refresh_secs(&low), MIN_REFRESH_SECS);
}

#[test]
fn poll_secs_takes_the_shortest_configured_interval() {
    let cfg = CalendarConfig {
        accounts: vec![
            CalendarAccount {
                refresh_interval_secs: 3600,
                ..account("slow", CalendarProviderKind::Ics)
            },
            CalendarAccount {
                refresh_interval_secs: 120,
                ..account("fast", CalendarProviderKind::IcsUrl)
            },
        ],
        ..CalendarConfig::default()
    };
    // One pass serves every account, so it must tick as often as the keenest.
    assert_eq!(cfg.poll_secs(), Some(120));
}

#[test]
fn disabled_accounts_and_the_master_switch_both_silence_the_ticker() {
    let mut cfg = CalendarConfig {
        accounts: vec![account("work", CalendarProviderKind::Ics)],
        ..CalendarConfig::default()
    };
    assert_eq!(cfg.active_accounts().len(), 1);
    assert!(cfg.poll_secs().is_some());

    cfg.accounts[0].enabled = false;
    assert!(cfg.active_accounts().is_empty());
    assert!(cfg.poll_secs().is_none());

    cfg.accounts[0].enabled = true;
    cfg.enabled = false;
    assert!(cfg.active_accounts().is_empty(), "master switch wins");

    // A "none" provider is configuration in progress, not a source.
    cfg.enabled = true;
    cfg.accounts[0].provider = CalendarProviderKind::None;
    assert!(cfg.active_accounts().is_empty());
}

#[test]
fn only_url_backed_providers_are_gated_offline() {
    // Unlike issues/PRs, a calendar account can be a local file or a
    // subprocess — those must keep syncing with the network down.
    assert!(account("a", CalendarProviderKind::IcsUrl).is_network_backed());
    assert!(account("a", CalendarProviderKind::CalDav).is_network_backed());
    assert!(!account("a", CalendarProviderKind::Ics).is_network_backed());
    assert!(!account("a", CalendarProviderKind::Command).is_network_backed());
}

#[test]
fn an_unknown_clock_zone_is_dropped_not_fatal() {
    let cfg = CalendarConfig {
        clocks: vec![
            WorldClock {
                zone: "Asia/Tokyo".into(),
                ..Default::default()
            },
            WorldClock {
                zone: "Mars/Olympus_Mons".into(),
                ..Default::default()
            },
            WorldClock {
                zone: "america/new_york".into(),
                ..Default::default()
            },
        ],
        ..CalendarConfig::default()
    };
    let active = cfg.active_clocks();
    assert_eq!(
        active.len(),
        2,
        "the bogus zone is skipped, the rest survive"
    );
    assert_eq!(active[0].zone, chrono_tz::Tz::Asia__Tokyo);
    assert_eq!(
        active[1].zone,
        chrono_tz::Tz::America__New_York,
        "lookup is case-insensitive"
    );
}

#[test]
fn a_disabled_clock_is_skipped() {
    let cfg = CalendarConfig {
        clocks: vec![WorldClock {
            zone: "UTC".into(),
            enabled: false,
            ..Default::default()
        }],
        ..CalendarConfig::default()
    };
    assert!(cfg.active_clocks().is_empty());
}

#[test]
fn an_unknown_home_zone_falls_back_to_local() {
    let cfg = CalendarConfig {
        home_zone: "Mars/Olympus_Mons".into(),
        ..CalendarConfig::default()
    };
    assert!(
        cfg.home_zone().is_none(),
        "falls back rather than panicking"
    );
    let ok = CalendarConfig {
        home_zone: "Europe/Berlin".into(),
        ..CalendarConfig::default()
    };
    assert_eq!(ok.home_zone(), Some(chrono_tz::Tz::Europe__Berlin));
}

#[test]
fn a_miscased_zone_is_accepted_rather_than_reported() {
    // Case is not a mistake worth failing over — the lookup normalizes it.
    let cfg = CalendarConfig {
        clocks: vec![WorldClock {
            zone: "america/new_york".into(),
            ..Default::default()
        }],
        ..CalendarConfig::default()
    };
    assert!(validate_calendar(&cfg).is_empty());
}

#[test]
fn validation_reports_an_unknown_zone_with_a_did_you_mean() {
    // A transposition can't be recovered by substring matching, so this is the
    // case that proves the fuzzy fallback is wired up.
    let cfg = CalendarConfig {
        clocks: vec![WorldClock {
            zone: "America/New_Yrok".into(),
            ..Default::default()
        }],
        ..CalendarConfig::default()
    };
    let errs = validate_calendar(&cfg);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("calendar.clocks[0].zone"));
    assert!(
        errs[0].contains("America/New_York"),
        "the fix must be in the message, got: {}",
        errs[0]
    );
}

#[test]
fn validation_requires_the_field_each_provider_actually_needs() {
    let cfg = CalendarConfig {
        accounts: vec![
            account("a", CalendarProviderKind::Ics),
            account("b", CalendarProviderKind::IcsUrl),
            account("c", CalendarProviderKind::CalDav),
            account("d", CalendarProviderKind::Command),
        ],
        ..CalendarConfig::default()
    };
    let errs = validate_calendar(&cfg);
    assert_eq!(errs.len(), 4, "one per under-specified account: {errs:?}");
    assert!(errs[0].contains("path"));
    assert!(errs[1].contains("url"));
    assert!(errs[2].contains("url"));
    assert!(errs[3].contains("command"));

    // Fully specified accounts validate clean.
    let ok = CalendarConfig {
        accounts: vec![
            CalendarAccount {
                path: "/tmp/work.ics".into(),
                ..account("a", CalendarProviderKind::Ics)
            },
            CalendarAccount {
                url: "https://example.com/c.ics".into(),
                ..account("b", CalendarProviderKind::IcsUrl)
            },
            CalendarAccount {
                command: vec!["khal-thegn".into()],
                ..account("d", CalendarProviderKind::Command)
            },
        ],
        ..CalendarConfig::default()
    };
    assert!(
        validate_calendar(&ok).is_empty(),
        "{:?}",
        validate_calendar(&ok)
    );
}

#[test]
fn validation_rejects_duplicate_and_missing_account_names() {
    // Names are cache keys — two accounts sharing one would clobber each
    // other's rows on every sync.
    let cfg = CalendarConfig {
        accounts: vec![
            CalendarAccount {
                path: "/a.ics".into(),
                ..account("work", CalendarProviderKind::Ics)
            },
            CalendarAccount {
                path: "/b.ics".into(),
                ..account("work", CalendarProviderKind::Ics)
            },
            CalendarAccount {
                path: "/c.ics".into(),
                ..account("", CalendarProviderKind::Ics)
            },
        ],
        ..CalendarConfig::default()
    };
    let errs = validate_calendar(&cfg);
    assert!(errs.iter().any(|e| e.contains("duplicate")), "{errs:?}");
    assert!(
        errs.iter().any(|e| e.contains("name: required")),
        "{errs:?}"
    );
}

#[test]
fn validation_rejects_a_malformed_capability_grant() {
    let cfg = CalendarConfig {
        accounts: vec![CalendarAccount {
            command: vec!["p".into()],
            capabilities: vec!["run:khal".into(), "justnetwork".into()],
            ..account("p", CalendarProviderKind::Command)
        }],
        ..CalendarConfig::default()
    };
    let errs = validate_calendar(&cfg);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("kind:target"), "{}", errs[0]);
}

#[test]
fn validation_rejects_a_clock_format_that_would_panic_at_render_time() {
    let cfg = CalendarConfig {
        clocks: vec![WorldClock {
            zone: "UTC".into(),
            format: "%Q".into(),
            ..Default::default()
        }],
        ..CalendarConfig::default()
    };
    let errs = validate_calendar(&cfg);
    assert_eq!(errs.len(), 1);
    assert!(errs[0].contains("calendar.clocks[0].format"), "{}", errs[0]);
}

#[test]
fn provider_and_display_enums_round_trip_their_canonical_strings() {
    for (s, want) in [
        ("none", CalendarProviderKind::None),
        ("ics", CalendarProviderKind::Ics),
        ("ics_url", CalendarProviderKind::IcsUrl),
        ("webcal", CalendarProviderKind::IcsUrl),
        ("url", CalendarProviderKind::IcsUrl),
        ("caldav", CalendarProviderKind::CalDav),
        ("command", CalendarProviderKind::Command),
        ("exec", CalendarProviderKind::Command),
        ("subprocess", CalendarProviderKind::Command),
    ] {
        assert_eq!(
            CalendarProviderKind::from_str_validated(s).unwrap(),
            want,
            "{s}"
        );
    }
    // Canonical strings survive a serialize round trip.
    assert_eq!(CalendarProviderKind::IcsUrl.as_str(), "ics_url");
    assert!(CalendarProviderKind::from_str_validated("nope").is_err());

    for (s, want) in [
        ("auto", WeekStart::Auto),
        ("monday", WeekStart::Monday),
        ("mon", WeekStart::Monday),
        ("sunday", WeekStart::Sunday),
        ("sat", WeekStart::Saturday),
    ] {
        assert_eq!(WeekStart::from_str_validated(s).unwrap(), want, "{s}");
    }
    for (s, want) in [
        ("auto", TimeFormat::Auto),
        ("12", TimeFormat::H12),
        ("12h", TimeFormat::H12),
        ("24", TimeFormat::H24),
        ("h24", TimeFormat::H24),
    ] {
        assert_eq!(TimeFormat::from_str_validated(s).unwrap(), want, "{s}");
    }
}

#[test]
fn display_prefs_translate_to_the_pure_domain_types() {
    let mut c = CalendarConfig::default();
    assert_eq!(c.week_start_pref(), None, "auto defers to the locale");
    assert_eq!(c.twelve_hour_pref(), None);

    c.week_start = WeekStart::Sunday;
    c.time_format = TimeFormat::H12;
    assert_eq!(c.week_start_pref(), Some(chrono::Weekday::Sun));
    assert_eq!(c.twelve_hour_pref(), Some(true));

    c.week_start = WeekStart::Saturday;
    c.time_format = TimeFormat::H24;
    assert_eq!(c.week_start_pref(), Some(chrono::Weekday::Sat));
    assert_eq!(c.twelve_hour_pref(), Some(false));

    c.week_start = WeekStart::Monday;
    assert_eq!(c.week_start_pref(), Some(chrono::Weekday::Mon));
}

#[test]
fn account_identity_and_color_resolution() {
    let a = account("work", CalendarProviderKind::Ics);
    assert_eq!(a.source_id().as_str(), "ics:work");
    assert!(a.hue().is_none(), "no color configured");
    assert!(a.read_only, "sources are read-only unless opted out");

    let colored = CalendarAccount {
        color: "Amber".into(),
        ..a.clone()
    };
    assert_eq!(colored.hue(), Some(crate::theme::Hue::Amber));
    let bogus = CalendarAccount {
        color: "chartreuse".into(),
        ..a
    };
    assert!(bogus.hue().is_none(), "an unknown name is not a hue");
}

#[test]
fn default_reminders_come_from_the_table() {
    let c = CalendarConfig::default();
    assert_eq!(
        c.default_reminders(),
        vec![crate::calendar::Reminder { minutes_before: 10 }]
    );
    let none = CalendarConfig {
        reminder_default_mins: vec![],
        ..CalendarConfig::default()
    };
    assert!(none.default_reminders().is_empty());
}

#[test]
fn the_whole_family_round_trips_through_toml() {
    let cfg = CalendarConfig {
        week_start: WeekStart::Sunday,
        home_zone: "Europe/Berlin".into(),
        clocks: vec![WorldClock {
            label: "tokyo".into(),
            zone: "Asia/Tokyo".into(),
            ..Default::default()
        }],
        accounts: vec![CalendarAccount {
            path: "/tmp/work.ics".into(),
            ..account("work", CalendarProviderKind::Ics)
        }],
        ..CalendarConfig::default()
    };
    let s = toml::to_string(&cfg).unwrap();
    let back: CalendarConfig = toml::from_str(&s).unwrap();
    assert_eq!(back.week_start, WeekStart::Sunday);
    assert_eq!(back.home_zone, "Europe/Berlin");
    assert_eq!(back.clocks, cfg.clocks);
    assert_eq!(back.accounts, cfg.accounts);
}

#[test]
fn an_entry_that_omits_enabled_still_counts() {
    // serde container `default` fills missing fields from `Default`, so the
    // common minimal TOML entry must come out enabled.
    let c: WorldClock = toml::from_str(r#"zone = "Asia/Tokyo""#).unwrap();
    assert!(c.enabled);
    let a: CalendarAccount =
        toml::from_str("name = \"work\"\nprovider = \"ics\"\npath = \"/w.ics\"").unwrap();
    assert!(a.enabled);
    assert_eq!(a.provider, CalendarProviderKind::Ics);
    assert_eq!(a.timeout_secs, 20);
}
