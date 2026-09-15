use super::*;
use thegn_core::config::{BoreConfig, FrpConfig, FrpProxyType, ShareConfig, ShareProviderKind};
use thegn_core::share::build_share_spec;

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
    assert!(toml.contains("[auth]"));
    assert!(toml.contains("token = \"literal-token\""));
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
fn frp_http_https_and_udp_keep_their_explicit_wire_shapes() {
    for (proxy_type, expected_type, expected_url) in [
        (
            FrpProxyType::Http,
            "http",
            "http://wt-3000.share.example.com",
        ),
        (
            FrpProxyType::Https,
            "https",
            "https://wt-3000.share.example.com",
        ),
    ] {
        let frp = FrpConfig {
            server_addr: "frps.example.com".into(),
            subdomain_host: "share.example.com".into(),
            proxy_type,
            vhost_https_port: 0,
            token: String::new(),
            ..FrpConfig::default()
        };
        let plan = process_plan(&frp_spec(frp, "wt", 3000));
        assert_eq!(plan.url_rule.fixed(), Some(expected_url));
        assert!(
            plan.files[0]
                .contents
                .contains(&format!("type = \"{expected_type}\""))
        );
    }

    let frp = FrpConfig {
        server_addr: "frps.example.com".into(),
        proxy_type: FrpProxyType::Udp,
        remote_port: 6001,
        token: String::new(),
        ..FrpConfig::default()
    };
    let plan = process_plan(&frp_spec(frp, "wt", 3000));
    assert_eq!(plan.url_rule.fixed(), Some("frps.example.com:6001"));
    assert!(plan.files[0].contents.contains("type = \"udp\""));
    assert!(plan.files[0].contents.contains("remotePort = 6001"));
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

#[test]
fn frp_rejects_invalid_fields_before_secret_resolution() {
    let token = "unused-token".to_string();
    let cases = [
        (
            "server_addr",
            FrpConfig {
                server_addr: "frps\nattacker".into(),
                subdomain_host: "ex.com".into(),
                token: token.clone(),
                ..FrpConfig::default()
            },
            "wt",
        ),
        (
            "subdomain",
            FrpConfig {
                server_addr: "frps".into(),
                subdomain_host: "ex.com".into(),
                subdomain: "bad\nname".into(),
                token: token.clone(),
                ..FrpConfig::default()
            },
            "wt",
        ),
        (
            "subdomain_host",
            FrpConfig {
                server_addr: "frps".into(),
                subdomain_host: "bad..example".into(),
                token: token.clone(),
                ..FrpConfig::default()
            },
            "wt",
        ),
        (
            "worktree label",
            FrpConfig {
                server_addr: "frps".into(),
                subdomain_host: "ex.com".into(),
                token,
                ..FrpConfig::default()
            },
            "bad_label",
        ),
    ];
    for (field, frp, label) in cases {
        let spec = frp_spec(frp.clone(), label, 3000);
        let resolutions = std::cell::Cell::new(0);
        let error = super::plan_frp_resolving_token(&spec, &frp, || {
            resolutions.set(resolutions.get() + 1);
            None
        })
        .expect_err("invalid provider input must be refused");
        assert_eq!(resolutions.get(), 0, "invalid {field} resolved a secret");
        assert!(
            error.to_string().contains(field),
            "expected {field} validation error, got {error}"
        );
    }
}

#[test]
fn frp_rejects_zero_local_port_before_serialization() {
    let frp = FrpConfig {
        server_addr: "frps".into(),
        subdomain_host: "ex.com".into(),
        token: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp.clone(), "wt", 0);
    let resolutions = std::cell::Cell::new(0);
    let error = super::plan_frp_resolving_token(&spec, &frp, || {
        resolutions.set(resolutions.get() + 1);
        None
    })
    .expect_err("zero local port must be refused");
    assert_eq!(resolutions.get(), 0, "zero local port resolved a secret");
    assert!(error.to_string().contains("local_port"));
}

#[test]
fn frp_rejects_unsupported_extra_lines() {
    let frp = FrpConfig {
        server_addr: "frps".into(),
        subdomain_host: "ex.com".into(),
        extra: vec!["transport.useEncryption = true".into()],
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 3000);
    let error = for_provider(&spec)
        .launch()
        .expect_err("extra must be refused");
    assert!(
        error
            .to_string()
            .contains("raw proxy-field injection is unsupported")
    );
}

#[test]
fn frp_rejects_zero_remote_port_for_fixed_tcp_address() {
    let frp = FrpConfig {
        server_addr: "frps".into(),
        proxy_type: FrpProxyType::Tcp,
        token: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 5432);
    let error = for_provider(&spec)
        .launch()
        .expect_err("remote port is required");
    assert!(error.to_string().contains("remote_port"));
}

#[test]
fn frp_ipv6_is_unbracketed_in_toml_and_bracketed_in_url() {
    let frp = FrpConfig {
        server_addr: "[2001:db8::7]".into(),
        proxy_type: FrpProxyType::Tcp,
        remote_port: 6000,
        token: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 5432);
    let plan = process_plan(&spec);
    assert_eq!(plan.url_rule.fixed(), Some("[2001:db8::7]:6000"));
    assert!(
        plan.files[0]
            .contents
            .contains("serverAddr = \"2001:db8::7\"")
    );
    assert!(
        !plan.files[0]
            .contents
            .contains("serverAddr = \"[2001:db8::7]\"")
    );
}

#[test]
fn frp_escapes_utf8_token_and_redacts_plan_debug() {
    let token = "tok\"\\\n\t\u{0001}–🔐";
    let frp = FrpConfig {
        server_addr: "frps".into(),
        subdomain_host: "ex.com".into(),
        token: token.into(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 3000);
    let plan = process_plan(&spec);
    let document = &plan.files[0].contents;
    assert!(document.contains("[auth]"));
    let parsed: super::FrpDocument = toml::from_str(document).expect("generated frpc TOML");
    assert_eq!(
        parsed.auth.as_ref().map(|auth| auth.token.as_str()),
        Some(token)
    );
    let debug = format!("{plan:?}");
    assert!(!debug.contains(token));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn frp_typed_document_rejects_unknown_fields() {
    let document = r#"
serverAddr = "frps"
serverPort = 7000

[[proxies]]
name = "tg-wt-3000"
type = "https"
localIP = "127.0.0.1"
localPort = 3000
subdomain = "wt-3000"
unexpected = true
"#;
    assert!(toml::from_str::<super::FrpDocument>(document).is_err());
}

#[test]
fn frp_typed_document_rejects_duplicate_keys_and_injected_tables() {
    let duplicate = r#"
serverAddr = "frps"
serverAddr = "attacker"
serverPort = 7000
proxies = []
"#;
    assert!(toml::from_str::<super::FrpDocument>(duplicate).is_err());

    let injected_table = r#"
serverAddr = "frps"
serverPort = 7000
proxies = []

[security]
auth = "disabled"
"#;
    assert!(toml::from_str::<super::FrpDocument>(injected_table).is_err());
}

#[test]
fn frp_rejects_invalid_or_overlong_dns_components() {
    let frp = FrpConfig {
        server_addr: "frps".into(),
        subdomain_host: "ex.com".into(),
        subdomain: "UPPER".into(),
        token: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, "wt", 3000);
    assert!(for_provider(&spec).launch().is_err());

    let long_label = "a".repeat(63);
    let frp = FrpConfig {
        server_addr: "frps".into(),
        subdomain_host: "ex.com".into(),
        token: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, &long_label, 3000);
    assert!(for_provider(&spec).launch().is_err());
}

#[test]
fn frp_preserves_a_63_byte_label_for_tcp_proxy_name() {
    let label = "a".repeat(63);
    let frp = FrpConfig {
        server_addr: "frps".into(),
        proxy_type: FrpProxyType::Tcp,
        remote_port: 6000,
        token: String::new(),
        ..FrpConfig::default()
    };
    let spec = frp_spec(frp, &label, 5432);
    let plan = process_plan(&spec);
    let expected = format!("name = \"tg-{label}-5432\"");
    assert!(plan.files[0].contents.contains(&expected));
}

// ── tailscale ────────────────────────────────────────────────────────────────

fn serve_plan(ts: thegn_core::config::TailscaleShareConfig, port: u16) -> ServePlan {
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
    use thegn_core::config::TailscaleShareConfig;
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
        thegn_core::config::ShareVisibility::Private
    );
}

// ── start(): the child must survive after start() returns ─────────────────────

/// Regression: the per-stream reader threads must keep draining the child's
/// stdout/stderr for the child's whole lifetime. They used to `break` the moment
/// `tx.send` failed — which is the instant `start()` returns (the local `rx` is
/// dropped) — closing the pipe read ends. A tunnel client that then logs a line
/// (frpc's reconnect/heartbeat) takes SIGPIPE against the closed fd and dies,
/// flipping a healthy share to Down. Here a `sh` child (SIGPIPE is SIG_DFL in a
/// spawned child, as for a Go client) keeps writing well past the grace window;
/// with the leak it dies on its next write, with the fix it stays alive.
#[cfg(unix)]
#[test]
fn start_keeps_draining_so_child_survives_post_return() {
    // Prints a couple of lines during the grace window, then keeps writing every
    // 50ms for ~3s — long after start() returns and `rx` is dropped.
    let script = "i=0; while [ $i -lt 60 ]; do echo line $i; i=$((i+1)); sleep 0.05; done";
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![],
        files: vec![],
        url_rule: UrlRule::Fixed("http://fixed.example".into()),
    };
    let tmp = std::env::temp_dir().join(format!("thegn-share-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&tmp); // best-effort: test setup; later asserts fail loudly if missing

    let mut running = start(&plan, &tmp, Duration::from_secs(5)).expect("start");
    assert_eq!(running.public_url, "http://fixed.example");

    // Give the child time to emit several post-return lines: under the old
    // break-on-send-fail behavior it would SIGPIPE and exit here.
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        running.child.try_wait().expect("try_wait").is_none(),
        "child must still be running — a closed stdout would have SIGPIPE-killed it"
    );

    running.stop();
    let _ = std::fs::remove_dir_all(&tmp); // best-effort: test tmp cleanup
}

#[test]
fn tailscale_funnel_custom_port() {
    use thegn_core::config::TailscaleShareConfig;
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
