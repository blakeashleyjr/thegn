use super::*;
use superzej_core::config::{BoreConfig, ShareConfig, ShareProviderKind};
use superzej_core::share::build_share_spec;

fn spec_with(bore: BoreConfig, port: u16) -> ShareSpec {
    let cfg = ShareConfig {
        provider: ShareProviderKind::Bore,
        bore,
        ..ShareConfig::default()
    };
    build_share_spec(&cfg, port).expect("enabled")
}

#[test]
fn kind_is_bore() {
    let spec = spec_with(BoreConfig::default(), 3000);
    assert_eq!(for_provider(&spec).kind(), "bore");
}

#[test]
fn bore_args_minimal_use_public_relay() {
    // Ensure no inherited secret leaks into the minimal-config assertion.
    unsafe { std::env::remove_var("BORE_SECRET") };
    let spec = spec_with(BoreConfig::default(), 3000);
    let plan = for_provider(&spec).plan().expect("plan");
    assert_eq!(plan.program, "bore");
    assert_eq!(
        plan.args,
        vec![
            "local",
            "3000",
            "--to",
            "bore.pub",
            "--local-host",
            "127.0.0.1",
        ]
    );
    // No secret flag when BORE_SECRET is unset.
    assert!(!plan.args.iter().any(|a| a == "--secret"));
}

#[test]
fn bore_args_full() {
    let bore = BoreConfig {
        to: "relay.example.com".into(),
        secret: "literal-secret".into(),
        remote_port: 9000,
        local_host: "0.0.0.0".into(),
        extra_args: vec!["--max-conn".into(), "10".into()],
    };
    let spec = spec_with(bore, 8080);
    let plan = for_provider(&spec).plan().expect("plan");
    assert_eq!(
        plan.args,
        vec![
            "local",
            "8080",
            "--to",
            "relay.example.com",
            "--port",
            "9000",
            "--local-host",
            "0.0.0.0",
            "--secret",
            "literal-secret",
            "--max-conn",
            "10",
        ]
    );
}

#[test]
fn remote_port_zero_is_omitted() {
    let bore = BoreConfig {
        remote_port: 0,
        ..BoreConfig::default()
    };
    let spec = spec_with(bore, 3000);
    let plan = for_provider(&spec).plan().expect("plan");
    assert!(!plan.args.iter().any(|a| a == "--port"));
}

#[test]
fn url_rule_extracts_bore_listening_line() {
    let spec = spec_with(BoreConfig::default(), 3000);
    let plan = for_provider(&spec).plan().expect("plan");
    let line = "2026-06-26T00:00:00Z  INFO bore_cli::client: listening at bore.pub:41234";
    assert_eq!(
        plan.match_url(line).as_deref(),
        Some("http://bore.pub:41234")
    );
}

#[test]
fn url_rule_trims_trailing_punctuation() {
    let rule = UrlRule::AfterMarker {
        marker: "listening at ".into(),
        scheme: "http".into(),
    };
    assert_eq!(
        rule.apply("listening at relay:8000.").as_deref(),
        Some("http://relay:8000")
    );
}

#[test]
fn url_rule_ignores_non_matching_lines() {
    let rule = UrlRule::AfterMarker {
        marker: "listening at ".into(),
        scheme: "http".into(),
    };
    assert!(rule.apply("connected to server").is_none());
    // marker present but no host:port shape after it
    assert!(rule.apply("listening at soon").is_none());
    // non-numeric port
    assert!(rule.apply("listening at host:abc").is_none());
}
