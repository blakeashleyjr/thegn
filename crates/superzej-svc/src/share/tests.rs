use super::*;
use superzej_core::config::{BoreConfig, FrpConfig, FrpProxyType, ShareConfig, ShareProviderKind};
use superzej_core::share::build_share_spec;

fn spec_with(bore: BoreConfig, port: u16) -> ShareSpec {
    let cfg = ShareConfig {
        provider: ShareProviderKind::Bore,
        bore,
        ..ShareConfig::default()
    };
    build_share_spec(&cfg, "wt", port, None).expect("enabled")
}

fn frp_spec(frp: FrpConfig, label: &str, port: u16) -> ShareSpec {
    let cfg = ShareConfig {
        provider: ShareProviderKind::Frp,
        frp,
        ..ShareConfig::default()
    };
    build_share_spec(&cfg, label, port, None).expect("enabled")
}

/// The `Process` plan for a spec (panics if the provider is a sidecar-serve one).
fn process_plan(spec: &ShareSpec) -> SharePlan {
    match for_provider(spec).launch().expect("launch") {
        ShareLaunch::Process(p) => p,
        ShareLaunch::SidecarServe(_) => panic!("expected a Process launch"),
    }
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
    let plan = process_plan(&spec);
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
    let plan = process_plan(&spec);
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
    let plan = process_plan(&spec);
    assert!(!plan.args.iter().any(|a| a == "--port"));
}

#[test]
fn url_rule_extracts_bore_listening_line() {
    let spec = spec_with(BoreConfig::default(), 3000);
    let plan = process_plan(&spec);
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

// ── frp ─────────────────────────────────────────────────────────────────────

#[test]
fn frp_https_materializes_toml_and_derives_subdomain_url() {
    unsafe { std::env::remove_var("FRP_TOKEN") };
    let frp = FrpConfig {
        server_addr: "frps.example.com".into(),
        subdomain_host: "share.example.com".into(),
        token: String::new(), // no token → no auth lines
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "app-feat", 3000);
    let plan = process_plan(&spec);

    assert_eq!(plan.program, "frpc");
    assert_eq!(plan.args, vec!["-c", "{statedir}/frpc.toml"]);
    // URL derived from config (default subdomain = <label>-<port>).
    assert_eq!(
        plan.url_rule.fixed(),
        Some("https://app-feat-3000.share.example.com")
    );

    let toml = &plan.files[0].contents;
    assert_eq!(plan.files[0].dest, "frpc.toml");
    assert!(toml.contains("serverAddr = \"frps.example.com\""));
    assert!(toml.contains("type = \"https\""));
    assert!(toml.contains("localPort = 3000"));
    assert!(toml.contains("subdomain = \"app-feat-3000\""));
    assert!(!toml.contains("auth.token")); // unset token → omitted
}

#[test]
fn frp_token_and_explicit_subdomain_and_vhost_port() {
    let frp = FrpConfig {
        server_addr: "frps".into(),
        subdomain_host: "ex.com".into(),
        subdomain: "demo".into(),
        token: "literal-token".into(),
        vhost_https_port: 8443,
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 8080);
    let plan = process_plan(&spec);
    assert_eq!(plan.url_rule.fixed(), Some("https://demo.ex.com:8443"));
    let toml = &plan.files[0].contents;
    assert!(toml.contains("auth.token = \"literal-token\""));
    assert!(toml.contains("subdomain = \"demo\""));
}

#[test]
fn frp_tcp_derives_host_port_and_no_subdomain() {
    let frp = FrpConfig {
        server_addr: "frps.example.com".into(),
        proxy_type: FrpProxyType::Tcp,
        remote_port: 6000,
        token: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 5432);
    let plan = process_plan(&spec);
    assert_eq!(plan.url_rule.fixed(), Some("frps.example.com:6000"));
    let toml = &plan.files[0].contents;
    assert!(toml.contains("type = \"tcp\""));
    assert!(toml.contains("remotePort = 6000"));
    assert!(!toml.contains("subdomain"));
}

#[test]
fn frp_errors_without_server_addr() {
    let spec = frp_spec(FrpConfig::default(), "wt", 3000);
    assert!(for_provider(&spec).launch().is_err());
}

#[test]
fn frp_https_errors_without_subdomain_host() {
    let frp = FrpConfig {
        server_addr: "frps".into(),
        subdomain_host: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 3000);
    assert!(for_provider(&spec).launch().is_err());
}

// ── tailscale ────────────────────────────────────────────────────────────────

fn serve_plan(ts: superzej_core::config::TailscaleShareConfig, port: u16) -> ServePlan {
    let cfg = ShareConfig {
        provider: ShareProviderKind::Tailscale,
        tailscale: ts,
        ..ShareConfig::default()
    };
    let spec = build_share_spec(&cfg, "wt", port, None).expect("enabled");
    match for_provider(&spec).launch().expect("launch") {
        ShareLaunch::SidecarServe(s) => s,
        ShareLaunch::Process(_) => panic!("expected a SidecarServe launch"),
    }
}

#[test]
fn tailscale_serve_default_443() {
    use superzej_core::config::TailscaleShareConfig;
    let s = serve_plan(TailscaleShareConfig::default(), 3000);
    // serve (not funnel), default 443 → no --https flag, target = local port.
    assert_eq!(s.up_argv, vec!["tailscale", "serve", "--bg", "3000"]);
    assert_eq!(
        s.down_argv,
        vec!["tailscale", "serve", "--https=443", "off"]
    );
    assert_eq!(s.scheme, "https");
    assert_eq!(s.port, 443);
}

// ── iroh / dumbpipe ──────────────────────────────────────────────────────────

#[test]
fn iroh_listens_and_scrapes_ticket_into_connect_command() {
    let cfg = ShareConfig {
        provider: ShareProviderKind::Iroh,
        ..ShareConfig::default()
    };
    let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
    assert_eq!(for_provider(&spec).kind(), "iroh");
    let plan = process_plan(&spec);
    assert_eq!(plan.program, "dumbpipe");
    assert_eq!(plan.args, vec!["listen-tcp", "--host", "127.0.0.1:3000"]);
    // The ticket is opaque (not host:port); the address is the connect command.
    let line = "to connect, use: dumbpipe connect-tcp blobAbCdEf123ticket";
    assert_eq!(
        plan.match_url(line).as_deref(),
        Some("dumbpipe connect-tcp blobAbCdEf123ticket")
    );
    // Visibility is private (peer-to-peer).
    assert_eq!(
        spec.visibility,
        superzej_core::config::ShareVisibility::Private
    );
}

#[test]
fn tailscale_funnel_custom_port() {
    use superzej_core::config::TailscaleShareConfig;
    let s = serve_plan(
        TailscaleShareConfig {
            funnel: true,
            https_port: 8443,
        },
        3000,
    );
    assert_eq!(
        s.up_argv,
        vec!["tailscale", "funnel", "--https=8443", "--bg", "3000"]
    );
    assert_eq!(
        s.down_argv,
        vec!["tailscale", "funnel", "--https=8443", "off"]
    );
    assert_eq!(s.port, 8443);
}
