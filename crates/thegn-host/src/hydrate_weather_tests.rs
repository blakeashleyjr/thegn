//! Pure decisions only: no network, no runtime, no DB. Everything the poll
//! actually decides lives in [`should_fetch`] and in the snapshot's serialized
//! shape, so both are testable without isolating `XDG_STATE_HOME` — which
//! matters here, because this shell often runs *inside* a live thegn.

use super::*;
use thegn_core::config_weather::{WeatherConfig, WeatherProviderKind};
use thegn_core::weather::{ForecastDay, Sky, Units};

/// A config with weather actually turned on (the shipped default is off).
fn enabled() -> WeatherConfig {
    WeatherConfig {
        enabled: true,
        ..Default::default()
    }
}

fn snapshot() -> WeatherSnapshot {
    WeatherSnapshot {
        provider: "wttr_in".into(),
        place: "Reykjavík".into(),
        sky: Sky::Partly,
        description: "Partly cloudy".into(),
        temp: 7.5,
        feels_like: 3.0,
        hi: 9.0,
        lo: 2.0,
        humidity_pct: 81,
        wind: 24.0,
        units: Units::Metric,
        fetched_at: 1_760_000_000,
        forecast: vec![ForecastDay {
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 2).expect("valid date"),
            hi: 8.0,
            lo: 1.0,
            sky: Sky::Rain,
        }],
    }
}

#[test]
fn an_inactive_config_never_spawns() {
    let now = 1_800_000_000;
    // The shipped default: `enabled = false`. Nothing — not a cold cache, not
    // being online — makes it fetch.
    let off = WeatherConfig::default();
    assert!(!should_fetch(&off, None, now, false));
    assert!(!should_fetch(&off, Some(0), now, false));

    // `provider = "none"` and a reserved kind are the same inert state, reached
    // through the config's own `is_active()` rather than re-derived here.
    for provider in [
        WeatherProviderKind::None,
        WeatherProviderKind::OpenMeteo,
        WeatherProviderKind::OpenWeatherMap,
    ] {
        let cfg = WeatherConfig {
            provider,
            ..enabled()
        };
        assert!(
            !should_fetch(&cfg, None, now, false),
            "{provider} must not fetch"
        );
    }
}

#[test]
fn a_fresh_cache_suppresses_the_fetch() {
    let cfg = enabled();
    let interval = cfg.refresh_secs() as i64;
    let now = 1_800_000_000;

    // Inside the interval: the cached reading was already delivered, so a
    // restart (or a poll that lands early) costs zero requests.
    assert!(!should_fetch(&cfg, Some(now - interval + 1), now, false));
    assert!(!should_fetch(&cfg, Some(now), now, false));
    // Exactly at the interval is due — the comparison is strict.
    assert!(should_fetch(&cfg, Some(now - interval), now, false));
    // Well outside it, and the cold-start case.
    assert!(should_fetch(&cfg, Some(now - interval * 4), now, false));
    assert!(should_fetch(&cfg, None, now, false));

    // A stray `refresh_interval_secs = 0` is floored by `refresh_secs()`, so it
    // cannot turn the gate into "always fetch".
    let spinny = WeatherConfig {
        refresh_interval_secs: 0,
        ..enabled()
    };
    assert!(!should_fetch(&spinny, Some(now - 599), now, false));
    assert!(should_fetch(&spinny, Some(now - 600), now, false));
}

#[test]
fn offline_suppresses_the_fetch_but_not_the_delivery() {
    let cfg = enabled();
    let now = 1_800_000_000;
    // No cache and offline: still no attempt. `poll` has already delivered
    // whatever was cached (nothing, here) *before* asking this — so an offline
    // machine with a warm cache is fully served by the delivery, and one with a
    // cold cache simply shows no widget rather than burning a doomed request.
    assert!(!should_fetch(&cfg, None, now, true));
    assert!(!should_fetch(&cfg, Some(now - 86_400), now, true));
    // The very same inputs online are a fetch — offline is the only difference.
    assert!(should_fetch(&cfg, None, now, false));
    assert!(should_fetch(&cfg, Some(now - 86_400), now, false));
}

#[test]
fn the_cache_key_round_trips() {
    let snap = snapshot();
    // The `ui_state` value shape is exactly `serde_json::to_string`.
    let json = serde_json::to_string(&snap).expect("snapshot serializes");
    let back: WeatherSnapshot = serde_json::from_str(&json).expect("snapshot deserializes");
    assert_eq!(back, snap);

    // The key partitions the cache by everything that changes the reading, so a
    // units flip or a move can never serve the previous answer.
    let key = thegn_core::weather::cache_key("wttr_in", "Reykjavík", Units::Metric);
    assert_eq!(
        key,
        thegn_core::weather::cache_key("wttr_in", "Reykjavík", Units::Metric)
    );
    assert_ne!(
        key,
        thegn_core::weather::cache_key("wttr_in", "Reykjavík", Units::Imperial)
    );
    assert_ne!(
        key,
        thegn_core::weather::cache_key("wttr_in", "Oslo", Units::Metric)
    );
    assert_eq!(CACHE_SCOPE, "weather");
}
