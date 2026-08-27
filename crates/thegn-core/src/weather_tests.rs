use super::*;
use crate::termcaps::{ASCII, UNICODE};

/// A captured-shape `j1` body: numbers as JSON strings, both unit families
/// present, `hourly` as the 3-hourly array wttr.in actually returns.
const J1: &str = r#"{
  "current_condition": [{
    "temp_C": "18", "temp_F": "64",
    "FeelsLikeC": "17", "FeelsLikeF": "63",
    "humidity": "52", "weatherCode": "116",
    "weatherDesc": [{"value": "  Partly cloudy  "}],
    "windspeedKmph": "11", "windspeedMiles": "7"
  }],
  "nearest_area": [{
    "areaName": [{"value": "Berlin"}],
    "country": [{"value": "Germany"}]
  }],
  "weather": [
    {"date": "2026-08-26", "maxtempC": "22", "maxtempF": "71",
     "mintempC": "13", "mintempF": "55",
     "hourly": [{"weatherCode": "113"}, {"weatherCode": "113"},
                {"weatherCode": "119"}, {"weatherCode": "119"},
                {"weatherCode": "296"}, {"weatherCode": "296"},
                {"weatherCode": "122"}, {"weatherCode": "122"}]},
    {"date": "2026-08-27", "maxtempC": "24", "maxtempF": "75",
     "mintempC": "14", "mintempF": "57",
     "hourly": [{"weatherCode": "113"}]}
  ]
}"#;

#[test]
fn wwo_codes_map_to_the_right_class() {
    for (code, want) in [
        (113u16, Sky::Clear),
        (116, Sky::Partly),
        (119, Sky::Cloudy),
        (122, Sky::Cloudy),
        (143, Sky::Fog),
        (248, Sky::Fog),
        (260, Sky::Fog),
        (176, Sky::Rain),
        (296, Sky::Rain),
        (377, Sky::Rain),
        (179, Sky::Snow),
        (326, Sky::Snow),
        (371, Sky::Snow),
        (200, Sky::Storm),
        (395, Sky::Storm),
        // Not a code wttr.in emits, and the classes it does not cover.
        (0, Sky::Unknown),
        (999, Sky::Unknown),
    ] {
        assert_eq!(sky_from_wwo_code(code), want, "code {code}");
    }
    // `Wind` has no WWO code at all — it exists for the reserved providers
    // (Open-Meteo does report it) and must still resolve to a glyph.
    assert!(
        (0u16..=1000).all(|c| sky_from_wwo_code(c) != Sky::Wind),
        "no WWO code should map to Wind"
    );
}

#[test]
fn j1_decodes_metric_and_imperial_from_one_payload() {
    let m = decode_wttr_j1(J1, Units::Metric, 1_700_000_000).expect("metric decode");
    let i = decode_wttr_j1(J1, Units::Imperial, 1_700_000_000).expect("imperial decode");

    assert_eq!(m.provider, "wttr_in");
    assert_eq!(m.place, "Berlin");
    assert_eq!(m.description, "Partly cloudy", "weatherDesc is trimmed");
    assert_eq!(m.sky, Sky::Partly);
    assert_eq!(m.humidity_pct, 52);
    assert_eq!(m.units, Units::Metric);
    assert_eq!(m.fetched_at, 1_700_000_000);

    // The payload's OWN fields are selected — no arithmetic conversion. A
    // converted 18°C would be 64.4°F, not the 64 the provider reports.
    assert_eq!(m.temp, 18.0);
    assert_eq!(i.temp, 64.0);
    assert_eq!(m.feels_like, 17.0);
    assert_eq!(i.feels_like, 63.0);
    assert_eq!(m.wind, 11.0);
    assert_eq!(i.wind, 7.0);
    assert_eq!(m.hi, 22.0);
    assert_eq!(i.hi, 71.0);
    assert_eq!(m.lo, 13.0);
    assert_eq!(i.lo, 55.0);
    assert_eq!(i.units, Units::Imperial);

    assert_eq!(m.forecast.len(), 2);
    assert_eq!(
        m.forecast[0].date,
        chrono::NaiveDate::from_ymd_opt(2026, 8, 26).unwrap()
    );
    assert_eq!(m.forecast[1].hi, 24.0);
    assert_eq!(i.forecast[1].hi, 75.0);

    // Round-trips through the `ui_state` cache value unchanged.
    let json = serde_json::to_string(&m).unwrap();
    assert_eq!(serde_json::from_str::<WeatherSnapshot>(&json).unwrap(), m);
}

#[test]
fn a_cached_row_without_a_forecast_still_loads() {
    // `#[serde(default)]` on `forecast`: an older cached snapshot predates the
    // field and must not fail to deserialize.
    let old = r#"{"provider":"wttr_in","place":"Berlin","sky":"partly",
        "description":"Partly cloudy","temp":18.0,"feels_like":17.0,
        "hi":22.0,"lo":13.0,"humidity_pct":52,"wind":11.0,
        "units":"metric","fetched_at":1700000000}"#;
    let snap: WeatherSnapshot = serde_json::from_str(old).expect("legacy row decodes");
    assert!(snap.forecast.is_empty());
    assert_eq!(snap.sky, Sky::Partly);
    assert_eq!(snap.units, Units::Metric);
}

#[test]
fn j1_numbers_are_strings_and_missing_fields_are_tolerated() {
    // No `humidity`, no `FeelsLikeC`, no `weather` block at all — and a
    // `temp_C` that arrived as a real number rather than a string.
    let body = r#"{"current_condition":[{"temp_C":18,"weatherCode":"113",
        "windspeedKmph":"not a number"}]}"#;
    let s = decode_wttr_j1(body, Units::Metric, 42).expect("tolerant decode");
    assert_eq!(s.temp, 18.0, "a real JSON number decodes too");
    assert_eq!(s.feels_like, 0.0);
    assert_eq!(s.humidity_pct, 0);
    assert_eq!(s.wind, 0.0, "unparseable numbers default, never fail");
    assert_eq!(s.hi, 0.0);
    assert_eq!(s.lo, 0.0);
    assert_eq!(s.sky, Sky::Clear);
    assert_eq!(s.place, "", "absent nearest_area is empty, not an error");
    assert_eq!(s.description, "");
    assert!(s.forecast.is_empty());

    // Out-of-range humidity is clamped rather than wrapping through `as u8`.
    let hot = r#"{"current_condition":[{"humidity":"250"}]}"#;
    assert_eq!(
        decode_wttr_j1(hot, Units::Metric, 0).unwrap().humidity_pct,
        100
    );

    // A forecast day with no parseable date is dropped, not defaulted.
    let undated = r#"{"current_condition":[{"temp_C":"1"}],
        "weather":[{"maxtempC":"5"},{"date":"nonsense","maxtempC":"6"}]}"#;
    assert!(
        decode_wttr_j1(undated, Units::Metric, 0)
            .unwrap()
            .forecast
            .is_empty()
    );
}

#[test]
fn j1_rejects_garbage_and_an_empty_current_condition() {
    // The body may embed the user's location — the one piece of user data this
    // feature handles — so no error message may quote it.
    let secret = "Rue de la Paix, Paris";
    for body in [
        format!("not json at all: {secret}"),
        format!(r#"{{"current_condition":[],"note":"{secret}"}}"#),
        format!(r#"{{"note":"{secret}"}}"#),
    ] {
        let err = decode_wttr_j1(&body, Units::Metric, 0).expect_err("must be fatal");
        assert!(
            !err.to_string().contains(secret),
            "error leaked the body: {err}"
        );
        assert!(err.to_string().starts_with("weather: "), "{err}");
    }
    // `DecodeError` is a real error type (the seam wraps it).
    let e = decode_wttr_j1("{", Units::Metric, 0).unwrap_err();
    assert_eq!(format!("{e}"), e.0);
    let _: &dyn std::error::Error = &e;
}

#[test]
fn provider_text_is_stripped_of_control_chars_and_bounded() {
    // The body is remote data from a keyless third-party service, and the two
    // strings it contributes are drawn by a terminal compositor. `\r` and `\n`
    // are NOT inert there: termwiz acts on them inside a `Change::Text`, so the
    // tail of the string paints outside the popup's clip rect (a `\r` in
    // `place` put it at column 0 of the underlying chrome) and, from the last
    // row, scrolls the whole composed frame. ESC and BEL are additionally
    // nerfed to a space by termwiz's cell constructor, but only after `\r`/`\n`
    // have already been acted on — so the filter is what closes this.
    let hostile = format!(
        r#"{{"current_condition":[{{"temp_C":"1",
             "weatherDesc":[{{"value":"Sunny\u001b[31m\u0007\nPWNED{}"}}]}}],
           "nearest_area":[{{"areaName":[{{"value":"  Berlin\r\nZAP  "}}]}}]}}"#,
        "x".repeat(4096)
    );
    let s = decode_wttr_j1(&hostile, Units::Metric, 0).expect("hostile but valid JSON");
    for text in [&s.description, &s.place] {
        assert!(
            !text.chars().any(char::is_control),
            "control character survived the decode: {text:?}"
        );
        assert!(
            text.chars().count() <= 64,
            "unbounded provider text ({} chars): {text:?}",
            text.chars().count()
        );
        assert_eq!(text.trim(), text, "text must land trimmed: {text:?}");
    }
    // Dropped, not replaced: the visible words run together rather than gaining
    // phantom whitespace (the rule `cache_key` already follows).
    assert!(s.description.starts_with("Sunny[31mPWNED"), "{s:?}");
    assert_eq!(s.place, "BerlinZAP");

    // A well-formed payload is untouched — including the trimming this shape
    // has always done.
    let ok = decode_wttr_j1(J1, Units::Metric, 0).unwrap();
    assert_eq!(ok.description, "Partly cloudy");
    assert_eq!(ok.place, "Berlin");
}

#[test]
fn forecast_takes_the_midday_hourly_slot() {
    // Eight 3-hourly slots; index 4 is 12:00 and is the one that reads as "the
    // weather that day" — not 00:00 and not the last entry.
    let m = decode_wttr_j1(J1, Units::Metric, 0).unwrap();
    assert_eq!(m.forecast[0].sky, Sky::Rain, "hourly[4] = 296 wins");
    // A short `hourly` falls back to the first entry.
    assert_eq!(m.forecast[1].sky, Sky::Clear);

    // No `hourly` at all ⇒ Unknown, and an empty one likewise.
    let bare = r#"{"current_condition":[{"temp_C":"1"}],
        "weather":[{"date":"2026-01-01"},{"date":"2026-01-02","hourly":[]}]}"#;
    let s = decode_wttr_j1(bare, Units::Metric, 0).unwrap();
    assert_eq!(s.forecast[0].sky, Sky::Unknown);
    assert_eq!(s.forecast[1].sky, Sky::Unknown);
}

#[test]
fn forecast_is_capped_at_five_days() {
    let days: Vec<String> = (1..=9)
        .map(|d| format!(r#"{{"date":"2026-03-0{d}","maxtempC":"{d}"}}"#))
        .collect();
    let body = format!(
        r#"{{"current_condition":[{{"temp_C":"1"}}],"weather":[{}]}}"#,
        days.join(",")
    );
    let s = decode_wttr_j1(&body, Units::Metric, 0).unwrap();
    assert_eq!(s.forecast.len(), 5, "capped regardless of payload length");
}

#[test]
fn freshness_boundaries() {
    // Exactly at `stale_after` is stale; one second short is fresh.
    assert_eq!(freshness(0, 599, 600, 3600), Freshness::Fresh);
    assert_eq!(freshness(0, 600, 600, 3600), Freshness::Stale);
    // Exactly at `hard_expiry` is expired, and expiry wins over staleness.
    assert_eq!(freshness(0, 3599, 600, 3600), Freshness::Stale);
    assert_eq!(freshness(0, 3600, 600, 3600), Freshness::Expired);
    assert_eq!(freshness(0, 99_999, 600, 3600), Freshness::Expired);
    // `hard_expiry == 0` disables expiry entirely — it never wins.
    assert_eq!(freshness(0, 99_999, 600, 0), Freshness::Stale);
    // A `fetched_at` in the future (clock skew, a resumed laptop) is Fresh, and
    // must not underflow into a huge age.
    assert_eq!(freshness(2_000, 1_000, 600, 3600), Freshness::Fresh);
    assert_eq!(freshness(i64::MIN, i64::MAX, 600, 3600), Freshness::Expired);
    assert_eq!(freshness(100, 100, 600, 3600), Freshness::Fresh);
}

#[test]
fn units_resolve_from_locale() {
    // An explicit preference always beats the locale.
    assert_eq!(
        resolve_units(Some(Units::Metric), Some("en_US.UTF-8")),
        Units::Metric
    );
    assert_eq!(
        resolve_units(Some(Units::Imperial), Some("de_DE.UTF-8")),
        Units::Imperial
    );
    // Auto: the three Fahrenheit regions, and everything else.
    for loc in ["en_US.UTF-8", "en-us", "en_LR", "my_MM.UTF-8"] {
        assert_eq!(resolve_units(None, Some(loc)), Units::Imperial, "{loc}");
    }
    for loc in ["de_DE.UTF-8", "en_GB", "fr_CA@euro", "C", "POSIX", "", "en"] {
        assert_eq!(resolve_units(None, Some(loc)), Units::Metric, "{loc}");
    }
    assert_eq!(resolve_units(None, None), Units::Metric);
    assert_eq!(Units::Metric.as_str(), "metric");
    assert_eq!(Units::Imperial.as_str(), "imperial");
}

#[test]
fn cache_key_is_stable_and_case_insensitive() {
    let a = cache_key("wttr_in", "Berlin", Units::Metric);
    assert_eq!(a, "wttr_in|berlin|metric");
    assert_eq!(cache_key("wttr_in", " berlin ", Units::Metric), a);
    assert_eq!(cache_key("wttr_in", "BERLIN", Units::Metric), a);
    // Units and provider both partition the cache.
    assert_ne!(cache_key("wttr_in", "Berlin", Units::Imperial), a);
    assert_ne!(cache_key("open_meteo", "Berlin", Units::Metric), a);
    // An empty location is a valid configuration (the provider geolocates).
    assert_eq!(
        cache_key("wttr_in", "   ", Units::Metric),
        "wttr_in||metric"
    );
    // It is a DB key: no newline may survive a hand-edited config value.
    let messy = cache_key("wttr_in", " New\nYork\t ", Units::Metric);
    assert!(!messy.contains('\n') && !messy.contains('\t'), "{messy}");
    assert_eq!(messy, "wttr_in|newyork|metric");
}

#[test]
fn formatters_read_naturally() {
    assert_eq!(fmt_temp(18.0, Units::Metric), "18\u{00b0}C");
    assert_eq!(fmt_temp(17.6, Units::Metric), "18\u{00b0}C");
    assert_eq!(fmt_temp(64.4, Units::Imperial), "64\u{00b0}F");
    assert_eq!(fmt_temp(-0.4, Units::Metric), "0\u{00b0}C");
    assert_eq!(fmt_temp(-7.5, Units::Metric), "-8\u{00b0}C");

    assert_eq!(fmt_wind(11.6, Units::Metric), "12 km/h");
    assert_eq!(fmt_wind(7.0, Units::Imperial), "7 mph");
    assert_eq!(fmt_wind(0.0, Units::Metric), "0 km/h");

    // Age boundaries: the minute, the hour, and the day.
    assert_eq!(fmt_age(0, 0), "just now");
    assert_eq!(fmt_age(0, 59), "just now");
    assert_eq!(fmt_age(0, 60), "1m ago");
    assert_eq!(fmt_age(0, 300), "5m ago");
    assert_eq!(fmt_age(0, 3_599), "59m ago");
    assert_eq!(fmt_age(0, 3_600), "1h ago");
    assert_eq!(fmt_age(0, 10_800), "3h ago");
    assert_eq!(fmt_age(0, 86_399), "23h ago");
    assert_eq!(fmt_age(0, 86_400), "1d ago");
    assert_eq!(fmt_age(0, 172_800), "2d ago");
    // Never negative.
    assert_eq!(fmt_age(1_000, 0), "just now");
    assert_eq!(fmt_age(i64::MAX, i64::MIN), "just now");
}

#[test]
fn every_sky_class_has_a_glyph_in_both_sets() {
    for &sky in Sky::ALL {
        let uni = sky_glyph(sky, &UNICODE);
        let asc = sky_glyph(sky, &ASCII);
        if sky == Sky::Unknown {
            // Unknown draws nothing — the caller renders temperature alone.
            assert!(uni.is_empty() && asc.is_empty());
            continue;
        }
        assert!(!uni.is_empty(), "{sky:?} has no Unicode glyph");
        assert!(!asc.is_empty(), "{sky:?} has no ASCII fallback");
        assert!(asc.is_ascii(), "{sky:?} fallback is not ASCII: {asc:?}");
    }
    // The class list is exhaustive: every variant is in `ALL`.
    assert_eq!(Sky::ALL.len(), 9);
    assert_eq!(Sky::default(), Sky::Unknown);
}
