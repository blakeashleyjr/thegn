//! The starter config must stay valid against the real loader.

fn starter_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/starter.toml")
}

fn starter_body() -> String {
    std::fs::read_to_string(starter_path()).expect("read config/starter.toml")
}

#[test]
fn starter_config_has_no_validation_diagnostics() {
    let diagnostics = thegn_core::config_validate::validate_diagnostics(&starter_body());
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn starter_config_parses_into_the_typed_config() {
    let body = starter_body();
    let normalized = thegn_core::config_compat::normalize(&body).expect("normalize starter config");
    let parsed = toml::from_str::<thegn_core::config::Config>(&normalized.body);
    assert!(parsed.is_ok(), "typed Config parse failed: {parsed:?}");
}

#[test]
fn starter_config_enables_no_experimental_keys() {
    let body = starter_body();
    let parsed = toml::from_str::<toml::Table>(&body).expect("parse starter config as TOML table");
    for key in ["host", "placement", "voice"] {
        assert!(
            !parsed.contains_key(key),
            "unexpected experimental key {key}"
        );
    }
    if let Some(sandbox) = parsed.get("sandbox").and_then(toml::Value::as_table) {
        assert!(
            !sandbox.contains_key("remote"),
            "unexpected experimental key sandbox.remote"
        );
    }
}

#[test]
fn starter_config_uses_the_shipped_sealed_sandbox_default() {
    let body = starter_body();
    let parsed = toml::from_str::<toml::Table>(&body).expect("parse starter config as TOML table");
    let sandbox = parsed
        .get("sandbox")
        .and_then(toml::Value::as_table)
        .expect("starter config has a sandbox section");
    assert!(
        !sandbox.contains_key("profile"),
        "starter config must inherit the shipped sandbox profile"
    );
    assert!(body.contains("Default: sealed ($HOME hidden, network off)."));
    assert!(body.contains("Set profile = \"standard\" to"));
    assert!(body.contains("opt into the previous behaviour."));
}

#[test]
fn starter_config_header_points_at_the_reference() {
    let body = starter_body();
    assert!(
        body.lines()
            .next()
            .is_some_and(|line| line.starts_with("# thegn starter config"))
    );
    assert!(body.contains("config/config.toml.example"));
    assert!(body.contains("thegn config validate"));
}

/// Positive control: this must reject an unknown key even though the starter itself is valid.
#[test]
fn unknown_key_in_the_starter_is_rejected() {
    let bad = "[theme]\nno_such_starter_key = 1\n";
    let diagnostics = thegn_core::config_validate::validate_diagnostics(bad);
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic.severity == thegn_core::config_validate::ValidationSeverity::Error
            && diagnostic.message.contains("unknown key")
    }));
}
