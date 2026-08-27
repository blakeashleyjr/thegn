use super::*;

/// `[weather]` alone, parsed the way a real config file reaches it (through the
/// container's `#[serde(default)]`).
fn parse(body: &str) -> WeatherConfig {
    toml::from_str(body).expect("[weather] body must deserialize")
}

#[test]
fn defaults_are_inert() {
    let c = WeatherConfig::default();
    // The consent step: off until the user says otherwise.
    assert!(!c.enabled);
    // No slot ⇒ no ticker wake ⇒ a user who never enables weather pays nothing.
    assert_eq!(c.poll_secs(), None);
    assert!(!c.is_active());
    // ...and a default table is not itself a misconfiguration.
    assert!(validate_weather(&c).is_empty());
}

#[test]
fn a_missing_provider_key_defaults_to_wttr_in_but_a_reserved_one_disables() {
    // Key absent ⇒ the container's `#[serde(default)]` fills it from
    // `WeatherConfig::default()` ⇒ the sane keyless provider.
    let c = parse("enabled = true");
    assert_eq!(c.provider, WeatherProviderKind::WttrIn);
    assert!(c.is_active());

    // Key present but reserved ⇒ the enum's infallible `Deserialize` warns and
    // yields the ENUM default (`none`) ⇒ inert. Emphatically NOT a silent
    // fallback to wttr.in, which would be egress the user never chose.
    for body in [
        "enabled = true\nprovider = \"open_meteo\"",
        "enabled = true\nprovider = \"openweathermap\"",
        // ...including through an alias of a reserved kind.
        "enabled = true\nprovider = \"owm\"",
    ] {
        let c = parse(body);
        assert_eq!(c.provider, WeatherProviderKind::None, "{body}");
        assert_eq!(c.poll_secs(), None, "{body}");
    }
}

#[test]
fn an_unknown_provider_or_unit_warns_and_falls_back() {
    // Warn-and-default, never a hard load failure: one typo must not cost the
    // whole config file. (`thegn config validate` is where it is an error.)
    let c = parse("enabled = true\nprovider = \"nope\"\nunits = \"kelvin\"");
    assert_eq!(c.provider, WeatherProviderKind::None);
    assert_eq!(c.units, WeatherUnits::Auto);
    assert!(!c.is_active());
}

#[test]
fn refresh_is_floored_in_the_accessor() {
    // Floored in the accessor so every caller inherits it — a stray 0 can never
    // spin a poll loop against a free community service.
    for raw in [0, 1, 599] {
        let c = WeatherConfig {
            refresh_interval_secs: raw,
            ..Default::default()
        };
        assert_eq!(c.refresh_secs(), MIN_REFRESH_SECS, "{raw}s must be floored");
    }
    // Above the floor, the configured value is honored verbatim.
    let c = WeatherConfig {
        refresh_interval_secs: 1800,
        ..Default::default()
    };
    assert_eq!(c.refresh_secs(), 1800);
    assert_eq!(WeatherConfig::default().refresh_secs(), 1800);
}

#[test]
fn poll_secs_is_none_unless_everything_is_on() {
    let on = WeatherConfig {
        enabled: true,
        ..Default::default()
    };
    assert_eq!(on.poll_secs(), Some(1800));
    assert!(on.is_active());

    // Disabled.
    assert_eq!(
        WeatherConfig {
            enabled: false,
            ..on.clone()
        }
        .poll_secs(),
        None
    );
    // Explicitly no provider.
    assert_eq!(
        WeatherConfig {
            provider: WeatherProviderKind::None,
            ..on.clone()
        }
        .poll_secs(),
        None
    );
    // Reserved kinds. Config can't actually reach these (they deserialize to
    // `None` — see the type doc), but a programmatically-built config can.
    for k in [
        WeatherProviderKind::OpenMeteo,
        WeatherProviderKind::OpenWeatherMap,
    ] {
        let c = WeatherConfig {
            provider: k,
            ..on.clone()
        };
        assert_eq!(c.poll_secs(), None, "{k:?}");
        assert!(!c.is_active(), "{k:?}");
    }
}

#[test]
fn units_resolve_through_the_core_helper() {
    let auto = WeatherConfig::default();
    assert_eq!(auto.units_pref(), None);
    assert_eq!(auto.resolved_units(Some("en_US.UTF-8")), Units::Imperial);
    assert_eq!(auto.resolved_units(Some("de_DE.UTF-8")), Units::Metric);
    assert_eq!(auto.resolved_units(None), Units::Metric);

    // An explicit preference beats the locale in both directions.
    let metric = WeatherConfig {
        units: WeatherUnits::Metric,
        ..Default::default()
    };
    assert_eq!(metric.units_pref(), Some(Units::Metric));
    assert_eq!(metric.resolved_units(Some("en_US.UTF-8")), Units::Metric);
    let imperial = WeatherConfig {
        units: WeatherUnits::Imperial,
        ..Default::default()
    };
    assert_eq!(
        imperial.resolved_units(Some("de_DE.UTF-8")),
        Units::Imperial
    );
}

#[test]
fn timeout_is_clamped() {
    let t = |secs| {
        WeatherConfig {
            timeout_secs: secs,
            ..Default::default()
        }
        .timeout()
        .as_secs()
    };
    // 0 would mean "wait forever" to most HTTP clients.
    assert_eq!(t(0), 3);
    assert_eq!(t(1), 3);
    assert_eq!(t(10), 10);
    assert_eq!(t(60), 60);
    assert_eq!(t(3600), 60);
    assert_eq!(WeatherConfig::default().timeout(), Duration::from_secs(10));
}

#[test]
fn validate_flags_each_problem_once() {
    let has = |c: &WeatherConfig, needle: &str| {
        let msgs = validate_weather(c);
        assert_eq!(
            msgs.iter().filter(|m| m.contains(needle)).count(),
            1,
            "expected exactly one {needle:?} message, got {msgs:?}"
        );
    };

    // Below the floor: informational, because the accessor already floors it.
    has(
        &WeatherConfig {
            refresh_interval_secs: 60,
            ..Default::default()
        },
        "refresh_interval_secs",
    );
    // 0 reads as "unset" rather than as an attempt at a rate, so it is quiet.
    assert!(
        validate_weather(&WeatherConfig {
            refresh_interval_secs: 0,
            ..Default::default()
        })
        .is_empty()
    );

    has(
        &WeatherConfig {
            stale_after_secs: 0,
            ..Default::default()
        },
        "stale_after_secs",
    );
    // Hidden before it can ever render stale.
    has(
        &WeatherConfig {
            stale_after_secs: 10_800,
            hard_expiry_secs: 3600,
            ..Default::default()
        },
        "hard_expiry_secs",
    );
    // Equal is the same defect.
    has(
        &WeatherConfig {
            stale_after_secs: 3600,
            hard_expiry_secs: 3600,
            ..Default::default()
        },
        "hard_expiry_secs",
    );
    // 0 disables expiry and is fine at any stale threshold.
    assert!(
        validate_weather(&WeatherConfig {
            hard_expiry_secs: 0,
            ..Default::default()
        })
        .is_empty()
    );

    has(
        &WeatherConfig {
            forecast_days: 9,
            ..Default::default()
        },
        "forecast_days",
    );
    assert!(
        validate_weather(&WeatherConfig {
            forecast_days: 5,
            ..Default::default()
        })
        .is_empty()
    );

    // A location is a URL path segment: bounded, and single-line.
    has(
        &WeatherConfig {
            location: "x".repeat(129),
            ..Default::default()
        },
        "too long",
    );
    assert!(
        validate_weather(&WeatherConfig {
            location: "x".repeat(128),
            ..Default::default()
        })
        .is_empty()
    );
    has(
        &WeatherConfig {
            location: "Berlin\nHost: evil".into(),
            ..Default::default()
        },
        "newline",
    );

    // Credentials never live raw in config.toml...
    has(
        &WeatherConfig {
            api_key: "abcd1234".into(),
            ..Default::default()
        },
        "api_key",
    );
    // ...but a SecretRef is exactly right.
    for r in ["env:OWM_API_KEY", "file:~/.config/thegn/owm", ""] {
        assert!(
            validate_weather(&WeatherConfig {
                api_key: r.into(),
                ..Default::default()
            })
            .is_empty(),
            "{r:?} must be accepted"
        );
    }
}

#[test]
fn an_empty_config_gives_an_inert_weather_table() {
    // Guards the `Config` field's `Default` wiring: a user with no `[weather]`
    // table at all must land on the same inert defaults.
    let cfg: crate::config::Config = toml::from_str("").expect("empty config");
    assert_eq!(cfg.weather, WeatherConfig::default());
    assert!(!cfg.weather.enabled);
    assert_eq!(cfg.weather.poll_secs(), None);
    assert!(validate_weather(&cfg.weather).is_empty());
}
