//! Weather-seam unit tests. **Nothing here touches the network** — the seam's
//! one round trip is exercised by the pure URL builder and by the error
//! classification, exactly as the probe contract requires of a cheap check.

use super::wttr_in::url_for;
use super::*;
use thegn_core::seam::Kind;

/// An enabled `[weather]` with `location`.
fn cfg(location: &str) -> WeatherConfig {
    WeatherConfig {
        enabled: true,
        location: location.into(),
        ..Default::default()
    }
}

#[test]
fn provider_for_is_none_unless_configured() {
    // Off by default — the consent step. No provider, so no request can happen.
    assert!(provider_for(&WeatherConfig::default(), Units::Metric).is_none());

    // Enabled + the implemented kind ⇒ a backend that names itself.
    let p = provider_for(&cfg("Berlin"), Units::Metric).expect("wttr_in is implemented");
    assert_eq!(p.provider_id(), "wttr_in");

    // Enabled but deactivated.
    let mut none = cfg("Berlin");
    none.provider = WeatherProviderKind::None;
    assert!(provider_for(&none, Units::Metric).is_none());

    // Every reserved kind is inert even when enabled.
    for k in WeatherProviderKind::ALL.iter().filter(|k| k.is_reserved()) {
        let mut reserved = cfg("Berlin");
        reserved.provider = *k;
        assert!(
            provider_for(&reserved, Units::Metric).is_none(),
            "reserved kind {} built a provider",
            k.as_str()
        );
    }

    // The factory and the config gate agree on every kind, so nothing
    // re-derives "is weather active" at a call site.
    for k in WeatherProviderKind::ALL {
        let mut c = cfg("Berlin");
        c.provider = *k;
        assert_eq!(
            provider_for(&c, Units::Metric).is_some(),
            c.is_active(),
            "kind {} disagrees with is_active()",
            k.as_str()
        );
    }
}

#[test]
fn url_building_encodes_the_location() {
    // No location ⇒ the service infers a city from the request IP.
    assert_eq!(url_for("").unwrap(), "https://wttr.in/?format=j1");
    assert_eq!(url_for("   ").unwrap(), "https://wttr.in/?format=j1");

    // A space is encoded, never sent raw (a raw space would split the request
    // line).
    let ny = url_for("New York").unwrap();
    assert_eq!(ny, "https://wttr.in/New%20York?format=j1");
    assert!(!ny.contains(' '));

    // Non-ASCII is percent-encoded UTF-8.
    let sp = url_for("São Paulo").unwrap();
    assert_eq!(sp, "https://wttr.in/S%C3%A3o%20Paulo?format=j1");

    // A traversal attempt is data, not syntax: it must not climb off the base.
    let trav = url_for("../x").unwrap();
    assert!(
        trav.starts_with("https://wttr.in/") && !trav.contains("/../"),
        "{trav}"
    );
    // Likewise an embedded separator or query character.
    let slash = url_for("a/b?c=d").unwrap();
    assert_eq!(slash, "https://wttr.in/a%2Fb%3Fc=d?format=j1");
    assert_eq!(slash.matches("format=j1").count(), 1);
}

#[test]
fn errors_classify_correctly() {
    use ErrorClass::*;
    assert_eq!(WeatherError::NotConfigured.class(), NotConfigured);
    assert_eq!(WeatherError::Network("x".into()).class(), Transient);
    assert_eq!(WeatherError::Api("x".into()).class(), Other);
    assert_eq!(WeatherError::Parse("x".into()).class(), Other);
    assert_eq!(WeatherError::Unsupported("fetch").class(), Unsupported);
    assert_eq!(
        <WeatherError as SeamError>::unsupported("fetch").class(),
        Unsupported
    );

    // A transport blip is transient; an unreadable payload is NOT — it means
    // the provider changed its format, and calling that "offline" would flip
    // the whole app's connectivity state on a permanent condition.
    assert!(WeatherError::Network("timed out".into()).is_transient());
    assert!(!WeatherError::Parse("no current_condition".into()).is_transient());
    assert!(!WeatherError::Api("HTTP 500".into()).is_transient());

    // Only the "this layer can't" classes fall through a ladder.
    assert!(WeatherError::NotConfigured.falls_through());
    assert!(!WeatherError::Network("x".into()).falls_through());
}

#[test]
fn errors_never_carry_the_location() {
    const SECRET: &str = "Nuuk";
    // Every variant a fetch can produce, rendered.
    let rendered = [
        WeatherError::NotConfigured,
        WeatherError::Api("HTTP 503 Service Unavailable".into()),
        WeatherError::Api(
            "rate limited (HTTP 429): the service throttles anonymous callers — \
             wait for the next refresh"
                .into(),
        ),
        WeatherError::Api("weather response too large".into()),
        WeatherError::Network("operation timed out".into()),
        // The core decode's own message, which likewise never embeds the body.
        WeatherError::Parse(
            thegn_core::weather::decode_wttr_j1(
                &format!("not json, from {SECRET}"),
                Units::Metric,
                0,
            )
            .unwrap_err()
            .to_string(),
        ),
        WeatherError::Unsupported("fetch"),
    ];
    for e in &rendered {
        let msg = e.to_string();
        assert!(!msg.contains(SECRET), "error leaked the location: {msg}");
        // …and never the URL it was built into either.
        assert!(!msg.contains("wttr.in"), "error leaked the URL: {msg}");
    }
}
