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
    // Arg construction is independent of whether the configured secret resolves.
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
    // Secrets are delivered through the child environment, never argv.
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
            "--max-conn",
            "10",
        ]
    );
    assert!(!plan.args.iter().any(|arg| arg.contains("literal-secret")));
    assert!(
        plan.env
            .iter()
            .any(|(key, value)| { key == "BORE_SECRET" && value == "literal-secret" })
    );
}

#[test]
fn bore_secret_never_reaches_argv() {
    let secret = "literal-test-secret";
    let spec = spec_with(
        BoreConfig {
            secret: secret.into(),
            ..BoreConfig::default()
        },
        3000,
    );
    let plan = process_plan(&spec);

    assert!(
        !plan
            .args
            .iter()
            .any(|arg| arg == secret || arg.contains(secret))
    );
    assert!(!plan.args.iter().any(|arg| arg == "--secret"));
    assert!(
        plan.env
            .iter()
            .any(|(key, value)| key == "BORE_SECRET" && value == secret)
    );
}

#[test]
fn direct_client_env_drops_ambient_credentials() {
    let ambient = [
        ("GITHUB_TOKEN", "github-secret"),
        ("LINEAR_API_KEY", "linear-secret"),
        ("IROH_SECRET", "iroh-secret"),
        ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/secret"),
        ("HOME", "/home/private"),
        ("CONTAINER_HOST", "unix:///secret"),
        ("PATH", "/usr/bin:/bin"),
        ("LANG", "en_US.UTF-8"),
    ]
    .into_iter()
    .map(|(key, value)| (key.into(), value.into()));
    let filtered = direct_client_env(ambient);

    assert_eq!(
        filtered,
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("LANG".into(), "en_US.UTF-8".into()),
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
        plan.match_url(line)
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
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
        rule.apply("listening at relay:8000.")
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
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

#[test]
fn public_endpoint_parsing_canonicalizes_and_rejects_ambiguous_input() {
    assert_eq!(
        PublicEndpoint::parse_url("http://127.0.0.1:8080")
            .unwrap()
            .canonical(),
        "http://127.0.0.1:8080"
    );
    assert_eq!(
        PublicEndpoint::parse_url("https://EXAMPLE.test:443/path")
            .unwrap()
            .canonical(),
        "https://example.test/path"
    );
    assert_eq!(
        PublicEndpoint::parse_url("http://[2001:0db8::1]:8080")
            .unwrap()
            .canonical(),
        "http://[2001:db8::1]:8080"
    );
    assert!(PublicEndpoint::parse_url("https://xn--bcher-kva.example").is_some());
    for numeric in [
        "http://127.1",
        "http://2130706433",
        "http://0x7f.1",
        "http://1.2.3.4.5",
    ] {
        assert!(
            PublicEndpoint::parse_url(numeric).is_none(),
            "accepted {numeric}"
        );
    }
    assert!(PublicEndpoint::parse_url("http://[::ffff:127.0.0.1]:8080").is_some());
    assert!(PublicEndpoint::parse_url("http://[fe80::1%25eth0]").is_none());
    assert!(PublicEndpoint::parse_fixed("127.1:80").is_none());
    assert!(PublicEndpoint::parse_fixed("::1:80").is_none());
    assert!(PublicEndpoint::parse_fixed("ab--cd.example:80").is_none());
    assert!(PublicEndpoint::parse_fixed("[::ffff:127.0.0.1]:8080").is_some());
    assert!(PublicEndpoint::parse_fixed("xn--bcher-kva.example:443").is_some());
    for bad in [
        "https://user@example.test",
        "https://example.test/#fragment",
        "https://example.test:0",
        "https://example.test:99999",
        "https://example.test/path with space",
        "https://example.test/%zz",
        "https://example.test\u{1b}]0;evil",
        "https://example.test:80;evil",
        "https://-bad.example",
        "https://例え.テスト",
        "https://ab--cd.example",
    ] {
        assert!(PublicEndpoint::parse_url(bad).is_none(), "accepted {bad:?}");
    }
    assert!(
        PublicEndpoint::parse_ticket(&"x".repeat(PublicEndpoint::MAX_TICKET_BYTES + 1)).is_none()
    );
    assert!(PublicEndpoint::parse_ticket("ticket\u{1b}[31m").is_none());
    let ticket = PublicEndpoint::parse_ticket("safe-ticket").expect("valid opaque ticket");
    assert!(ticket.as_url().is_none());
    assert_eq!(ticket.kind(), PublicEndpointKind::Ticket);
    assert!(!format!("{ticket:?}").contains("safe-ticket"));
}

#[test]
fn announcements_reject_repeated_markers_and_ticket_delimiters() {
    let rule = UrlRule::AfterMarker {
        marker: "public: ".into(),
        scheme: "https".into(),
    };
    assert!(rule.apply("public: one public: two.example").is_none());
    let ticket = UrlRule::AfterMarkerRaw {
        marker: "connect-tcp".into(),
    };
    assert!(ticket.apply("connect-tcp good-ticket;rm").is_none());
    assert_eq!(
        ticket.apply("connect-tcp safe-ticket").unwrap().canonical(),
        "safe-ticket"
    );
}

#[test]
fn startup_rejects_two_provider_endpoint_announcements() {
    let (tx, rx) = std::sync::mpsc::sync_channel(2);
    let first = PublicEndpoint::parse_url("https://first.test").expect("valid URL");
    tx.send(first.clone()).expect("first announcement");
    tx.send(PublicEndpoint::parse_url("https://second.test").expect("valid URL"))
        .expect("second announcement");
    assert!(reject_duplicate_announcement(&rx, &first).is_err());
}

#[test]
fn startup_accepts_byte_identical_endpoint_announcements() {
    let (tx, rx) = std::sync::mpsc::sync_channel(2);
    let first = PublicEndpoint::parse_url("https://first.test").expect("valid URL");
    tx.send(first.clone()).expect("repeat announcement");
    tx.send(first.clone())
        .expect("byte-identical repeated announcement");
    drop(tx);
    assert!(
        reject_duplicate_announcement(&rx, &first).is_ok(),
        "a byte-identical repeated announcement must not fail startup"
    );
}

#[test]
fn share_state_identity_is_bounded_canonical_and_manifest_owned() {
    let temp = tempfile::tempdir().expect("temporary root");
    let base = temp.path().join("state");
    let repo = temp.path().join("repo");
    for path in [
        repo.join("a/b"),
        repo.join("a-b"),
        repo.join("a_b"),
        repo.join("renamed"),
    ] {
        std::fs::create_dir_all(path).expect("worktree fixture");
    }
    let nested = repo.join("a/b");
    let a = share_state_dir_at(&base, &nested, "bore", 3000, 1).expect("first identity");
    let b = share_state_dir_at(&base, &repo.join("a-b"), "bore", 3000, 1).expect("second identity");
    let c = share_state_dir_at(&base, &repo.join("a_b"), "bore", 3000, 1).expect("third identity");
    assert_ne!(a, b);
    assert_ne!(a, c);
    assert_ne!(b, c);
    assert_ne!(
        a,
        share_state_dir_at(&base, &nested, "frp", 3000, 1).expect("provider identity")
    );
    assert_ne!(
        a,
        share_state_dir_at(&base, &nested, "bore", 3000, 2).expect("generation identity")
    );
    assert_eq!(
        a,
        share_state_dir_at(&base, &nested.join("."), "bore", 3000, 1).expect("canonical alias")
    );
    assert!(a.file_name().expect("component").to_string_lossy().len() < 100);

    let moved = repo.join("renamed");
    let before_rename = share_state_dir_at(&base, &moved, "bore", 3001, 1).expect("before rename");
    let renamed = repo.join("renamed-again");
    std::fs::rename(&moved, &renamed).expect("rename worktree fixture");
    let after_rename = share_state_dir_at(&base, &renamed, "bore", 3001, 1).expect("after rename");
    assert_ne!(before_rename, after_rename);

    let missing_worktree = repo.join("remote-or-deleted-worktree");
    assert!(
        share_state_dir_at(&base, &missing_worktree, "bore", 3002, 1).is_ok(),
        "missing worktree paths retain a usable identity"
    );
    let relative = std::path::PathBuf::from(format!("missing-share-wt-{}", std::process::id()));
    let absolute = std::env::current_dir()
        .expect("current directory")
        .join(&relative);
    assert_eq!(
        share_state_dir_at(&base, &relative, "bore", 3003, 1).expect("relative identity"),
        share_state_dir_at(&base, &absolute, "bore", 3003, 1).expect("absolute identity")
    );

    std::fs::write(a.join("owner.manifest"), b"foreign owner").expect("tamper manifest");
    assert!(share_state_dir_at(&base, &nested, "bore", 3000, 1).is_err());
}

#[cfg(unix)]
#[test]
fn share_state_identity_resolves_symlink_aliases() {
    let temp = tempfile::tempdir().expect("temporary root");
    let base = temp.path().join("state");
    let worktree = temp.path().join("worktree");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&worktree).expect("worktree fixture");
    std::os::unix::fs::symlink(&worktree, &alias).expect("symlink alias");
    assert_eq!(
        share_state_dir_at(&base, &worktree, "bore", 3000, 1).expect("real identity"),
        share_state_dir_at(&base, &alias, "bore", 3000, 1).expect("alias identity")
    );
}

#[cfg(unix)]
#[test]
fn share_state_identity_preserves_non_utf8_path_bytes() {
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir().expect("temporary root");
    let base = temp.path().join("state");
    let non_utf8 = temp.path().join(std::ffi::OsStr::from_bytes(b"wt-\xff"));
    let replacement = temp.path().join("wt--");
    std::fs::create_dir(&non_utf8).expect("non-UTF-8 worktree");
    std::fs::create_dir(&replacement).expect("replacement worktree");
    let actual =
        share_state_dir_at(&base, &non_utf8, "bore", 3000, 1).expect("non-UTF-8 share identity");
    let other = share_state_dir_at(&base, &replacement, "bore", 3000, 1)
        .expect("replacement share identity");
    assert!(actual.is_dir());
    assert_ne!(actual, other);
}

#[test]
fn share_state_identity_distinguishes_unicode_normalization_when_supported() {
    let temp = tempfile::tempdir().expect("temporary root");
    let base = temp.path().join("state");
    let nfc = temp.path().join("caf\u{e9}");
    let nfd = temp.path().join("cafe\u{301}");
    std::fs::create_dir(&nfc).expect("NFC worktree");
    match std::fs::create_dir(&nfd) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return,
        Err(error) => panic!("create NFD worktree: {error}"),
    }
    assert_ne!(
        share_state_dir_at(&base, &nfc, "bore", 3000, 1).expect("NFC identity"),
        share_state_dir_at(&base, &nfd, "bore", 3000, 1).expect("NFD identity")
    );
}

#[test]
fn foreign_manifest_collision_blocks_reuse_and_cleanup() {
    let temp = tempfile::tempdir().expect("temporary root");
    let base = temp.path().join("state");
    let worktree = temp.path().join("worktree");
    std::fs::create_dir(&worktree).expect("worktree fixture");
    let state =
        share_state_dir_at(&base, &worktree, "bore", 3000, 1).expect("owned state directory");
    let manifest = state.join("owner.manifest");
    let expected = std::fs::read(&manifest).expect("original manifest");
    std::fs::write(&manifest, b"different canonical identity").expect("simulated hash collision");
    assert!(share_state_dir_at(&base, &worktree, "bore", 3000, 1).is_err());
    cleanup_state_files(&state, &[], Some(&expected));
    assert!(
        state.is_dir(),
        "cleanup must retain a foreign-owned directory"
    );
    assert_eq!(
        std::fs::read(manifest).expect("foreign manifest remains"),
        b"different canonical identity"
    );
}

#[test]
fn failed_start_removes_materialized_provider_credentials() {
    let state = tempfile::tempdir().expect("temporary state");
    let plan = SharePlan {
        program: "thegn-share-test-command-that-does-not-exist".into(),
        args: Vec::new(),
        env: Vec::new(),
        files: vec![SharePlanFile {
            dest: "frpc.toml".into(),
            contents: "token = \"secret\"".into(),
        }],
        url_rule: UrlRule::Fixed("https://share.example".into()),
    };
    let error = start(&plan, state.path(), Duration::from_millis(1)).expect_err("missing command");
    assert!(error.to_string().contains("failed to spawn"));
    assert!(!state.path().join("frpc.toml").exists());
}

#[test]
fn start_then_stop_removes_share_state_directory() {
    let temp = tempfile::tempdir().expect("temporary root");
    let base = temp.path().join("state");
    let worktree = temp.path().join("repo");
    std::fs::create_dir_all(&worktree).expect("worktree fixture");
    let statedir =
        share_state_dir_at(&base, &worktree, "bore", 3000, 42).expect("share state directory");
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), "sleep 10".into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::Fixed("https://share.example".into()),
    };
    let running = start(&plan, &statedir, Duration::from_secs(1)).expect("start share");
    running.stop();
    assert!(
        !statedir.exists(),
        "stop must remove the owned state directory"
    );
}

#[test]
fn concurrent_share_state_creation_agrees_on_one_manifest() {
    let temp = tempfile::tempdir().expect("temporary root");
    let base = temp.path().join("state");
    let worktree = temp.path().join("worktree");
    std::fs::create_dir_all(&worktree).expect("worktree fixture");
    std::thread::scope(|scope| {
        let a = scope.spawn(|| share_state_dir_at(&base, &worktree, "bore", 3000, 7));
        let b = scope.spawn(|| share_state_dir_at(&base, &worktree, "bore", 3000, 7));
        assert_eq!(
            a.join().expect("first worker").expect("first create"),
            b.join().expect("second worker").expect("second create")
        );
    });
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
        plan.url_rule
            .fixed()
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
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
    assert_eq!(
        plan.url_rule
            .fixed()
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
        Some("https://demo.ex.com:8443")
    );
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
    assert_eq!(
        plan.url_rule
            .fixed()
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
        Some("frps.example.com:6000")
    );
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
        assert_eq!(
            plan.url_rule
                .fixed()
                .map(|endpoint| endpoint.canonical())
                .as_deref(),
            Some(expected_url)
        );
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
    assert_eq!(
        plan.url_rule
            .fixed()
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
        Some("frps.example.com:6001")
    );
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
    assert_eq!(
        plan.url_rule
            .fixed()
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
        Some("[2001:db8::7]:6000")
    );
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
        plan.match_url(line)
            .map(|endpoint| endpoint.canonical())
            .as_deref(),
        Some("blobAbCdEf123ticket")
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
    assert_eq!(running.public_url.canonical(), "http://fixed.example");

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

#[cfg(unix)]
#[test]
fn spawned_client_sees_only_allowlisted_env() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temporary client directory");
    let script_path = temp.path().join("fake-client");
    let dump_path = temp.path().join("environment.txt");
    let script =
        format!("#!/bin/sh\nenv > \"$1\"\nprintf 'listening at fake.example:4123\\n'\nsleep 2\n");
    std::fs::write(&script_path, script).expect("write fake client");
    let mut permissions = std::fs::metadata(&script_path)
        .expect("fake client metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&script_path, permissions).expect("make fake client executable");

    let plan = SharePlan {
        program: script_path.to_string_lossy().into_owned(),
        args: vec![dump_path.to_string_lossy().into_owned()],
        env: vec![("RUST_LOG".into(), "info".into())],
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
    };
    let ambient = [
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("LANG".into(), "C".into()),
        ("HOME".into(), "/private/home".into()),
        ("THEGN_TEST_CANARY_SECRET".into(), "canary-value".into()),
        ("GITHUB_TOKEN".into(), "github-value".into()),
    ];
    let running = start_with_ambient(&plan, temp.path(), Duration::from_secs(1), ambient)
        .expect("start fake client");
    let dumped_env = std::fs::read_to_string(&dump_path).expect("read child environment");

    assert!(dumped_env.lines().any(|line| line == "PATH=/usr/bin:/bin"));
    assert!(dumped_env.lines().any(|line| line == "RUST_LOG=info"));
    assert!(!dumped_env.contains("THEGN_TEST_CANARY_SECRET"));
    assert!(!dumped_env.contains("canary-value"));
    assert!(!dumped_env.contains("HOME="));
    assert!(!dumped_env.contains("GITHUB_TOKEN="));
    running.stop();
}

#[cfg(unix)]
#[test]
fn running_share_reports_endpoint_mutation_after_startup_window() {
    let state = tempfile::tempdir().expect("private temporary state");
    let script = "echo 'public: first.example:4123'; sleep 0.3; echo 'public: first.example:4123'; echo 'public: first.example:4123'; echo 'public: first.example:4123'; echo 'public: second.example:4123'; sleep 2";
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    };
    let running = start(&plan, state.path(), Duration::from_secs(2)).expect("start share");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !running.endpoint_conflict() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        running.endpoint_conflict(),
        "later changed endpoint must be sticky"
    );
    running.stop();
}

#[cfg(unix)]
#[test]
fn running_share_accepts_post_startup_identical_announcements() {
    let state = tempfile::tempdir().expect("private temporary state");
    let script = "echo 'public: first.example:4123'; sleep 0.3; echo 'public: first.example:4123'; echo 'public: first.example:4123'; echo 'public: first.example:4123'; sleep 2";
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    };
    let running = start(&plan, state.path(), Duration::from_secs(2)).expect("start share");
    std::thread::sleep(Duration::from_millis(700));
    assert!(
        !running.endpoint_conflict(),
        "identical endpoint repeats are harmless"
    );
    running.stop();
}

#[cfg(unix)]
#[test]
fn endpoint_conflict_hook_fires_once_for_post_startup_changes() {
    let state = tempfile::tempdir().expect("private temporary state");
    let script = "echo 'public: first.example:4123'; sleep 0.3; echo 'public: first.example:4123'; echo 'public: first.example:4123'; echo 'public: second.example:4123'; sleep 0.3; echo 'public: third.example:4123'; sleep 1";
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    };
    let running = start(&plan, state.path(), Duration::from_secs(2)).expect("start share");
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_calls = calls.clone();
    running.on_endpoint_conflict(Box::new(move || {
        hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while calls.load(std::sync::atomic::Ordering::SeqCst) == 0
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    running.stop();
}

#[cfg(unix)]
#[test]
fn endpoint_conflict_hook_fires_when_installed_after_conflict() {
    let state = tempfile::tempdir().expect("private temporary state");
    let script =
        "echo 'public: first.example:4123'; sleep 0.3; echo 'public: second.example:4123'; sleep 1";
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    };
    let running = start(&plan, state.path(), Duration::from_secs(2)).expect("start share");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !running.endpoint_conflict() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        running.endpoint_conflict(),
        "fixture must publish the conflict first"
    );
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_calls = calls.clone();
    running.on_endpoint_conflict(Box::new(move || {
        hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    running.stop();
}

#[cfg(unix)]
#[test]
fn endpoint_conflict_hook_does_not_fire_for_identical_repeats() {
    let state = tempfile::tempdir().expect("private temporary state");
    let script = "echo 'public: first.example:4123'; sleep 0.3; echo 'public: first.example:4123'; echo 'public: first.example:4123'; sleep 0.6";
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    };
    let running = start(&plan, state.path(), Duration::from_secs(2)).expect("start share");
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hook_calls = calls.clone();
    running.on_endpoint_conflict(Box::new(move || {
        hook_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }));
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    running.stop();
}

#[test]
fn bounded_reader_retries_interrupted_and_emits_eof_url() {
    struct InterruptOnce {
        interrupted: bool,
        inner: std::io::Cursor<Vec<u8>>,
    }

    impl std::io::Read for InterruptOnce {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            std::io::Read::read(&mut self.inner, buffer)
        }
    }

    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    let (url_tx, url_rx) = std::sync::mpsc::sync_channel(1);
    super::drain_reader(
        InterruptOnce {
            interrupted: false,
            inner: std::io::Cursor::new(b"listening at interrupted.example:4123".to_vec()),
        },
        tx,
        url_tx,
        UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
        super::EndpointTracker::default(),
    );
    assert_eq!(
        url_rx
            .recv()
            .expect("URL after interrupted read")
            .canonical(),
        "http://interrupted.example:4123"
    );
    assert_eq!(
        rx.recv().expect("diagnostic after interrupted read"),
        "listening at interrupted.example:4123"
    );
}

#[test]
fn bounded_reader_discards_oversized_line_but_keeps_following_url() {
    let (tx, rx) = std::sync::mpsc::sync_channel(4);
    let (url_tx, url_rx) = std::sync::mpsc::sync_channel(1);
    let mut output = vec![b'x'; super::MAX_PROVIDER_LINE + 1];
    output.extend_from_slice(b"\nlistening at bounded.example:4123\n");
    super::drain_reader(
        std::io::Cursor::new(output),
        tx,
        url_tx,
        UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
        super::EndpointTracker::default(),
    );
    assert_eq!(
        url_rx.recv().expect("URL after oversized line").canonical(),
        "http://bounded.example:4123"
    );
    assert_eq!(
        rx.recv().expect("bounded diagnostic line"),
        "listening at bounded.example:4123"
    );
}

#[test]
fn bounded_reader_keeps_url_when_diagnostics_receiver_is_gone() {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    drop(rx);
    let (url_tx, url_rx) = std::sync::mpsc::sync_channel(1);
    super::drain_reader(
        std::io::Cursor::new(b"listening at disconnected.example:4123\n".to_vec()),
        tx,
        url_tx,
        UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
        super::EndpointTracker::default(),
    );
    assert_eq!(
        url_rx
            .recv()
            .expect("priority URL after diagnostic disconnect")
            .canonical(),
        "http://disconnected.example:4123"
    );
}

#[test]
fn share_debug_redacts_args_and_public_url() {
    let plan = SharePlan {
        program: "fake-provider".into(),
        args: vec!["--token=ARGV_SENTINEL_325".into()],
        env: vec![("AUTH_TOKEN".into(), "ENV_SENTINEL_325".into())],
        files: vec![],
        url_rule: UrlRule::Fixed("https://secret.example/TICKET_SENTINEL_325".into()),
    };
    let debug = format!("{plan:?}");
    assert!(!debug.contains("ARGV_SENTINEL_325"));
    assert!(!debug.contains("ENV_SENTINEL_325"));
    assert!(!debug.contains("TICKET_SENTINEL_325"));
    assert!(debug.contains("<redacted>"));
}

#[cfg(unix)]
#[test]
fn start_returns_url_from_unterminated_provider_line() {
    let state = tempfile::tempdir().expect("temporary state");
    let plan = SharePlan {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf 'listening at eof.example:4123'".into()],
        env: vec![],
        files: vec![],
        url_rule: UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
    };
    let running = start(&plan, state.path(), Duration::from_secs(1)).expect("EOF URL");
    assert!(!format!("{running:?}").contains("eof.example"));
    assert_eq!(running.public_url.canonical(), "http://eof.example:4123");
    running.stop();
}

#[cfg(unix)]
#[test]
fn start_redacts_provider_diagnostics_on_early_exit() {
    let state = tempfile::tempdir().expect("temporary state");
    let plan = SharePlan {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf 'OUTPUT_SECRET_SENTINEL_325\\n' >&2; exit 7".into(),
        ],
        env: vec![],
        files: vec![],
        url_rule: UrlRule::Fixed("http://fixed.example".into()),
    };
    let error = start(&plan, state.path(), Duration::from_millis(500))
        .expect_err("provider exits before fixed URL grace");
    assert!(error.to_string().contains("exited early"));
    assert!(!error.to_string().contains("OUTPUT_SECRET_SENTINEL_325"));
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

#[test]
fn bounded_reader_prioritizes_late_url_over_full_diagnostic_queue() {
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    for i in 0..64 {
        tx.try_send(format!("noise-{i}"))
            .expect("fill bounded queue");
    }
    let (url_tx, url_rx) = std::sync::mpsc::sync_channel(1);
    let mut output = (0..64).map(|i| format!("noise-{i}\n")).collect::<String>();
    output.push_str("listening at late.example:4123\n");
    super::drain_reader(
        std::io::Cursor::new(output.into_bytes()),
        tx,
        url_tx,
        UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
        super::EndpointTracker::default(),
    );
    assert_eq!(
        url_rx.recv().expect("late URL signal").canonical(),
        "http://late.example:4123"
    );
    drop(rx);
}

#[test]
fn bounded_lossy_output_stays_within_byte_limit() {
    let bytes = vec![0xff; super::MAX_PROVIDER_LINE];
    let text = super::bounded_lossy(&bytes);
    assert!(text.len() <= super::MAX_PROVIDER_LINE);
    assert_eq!(text, "�".repeat(super::MAX_PROVIDER_LINE / 3));
}

// ---- THE-328: credential materialization (synthetic canary contents only) ----

fn file_plan(files: Vec<(&str, &str)>, program: &str, args: Vec<String>) -> SharePlan {
    SharePlan {
        program: program.into(),
        args,
        env: vec![],
        files: files
            .into_iter()
            .map(|(d, c)| SharePlanFile {
                dest: d.into(),
                contents: c.into(),
            })
            .collect(),
        url_rule: UrlRule::Fixed("http://fixed.example".into()),
    }
}

#[test]
fn materialize_rejects_invalid_dest_and_creates_nothing() {
    let long = "a".repeat(MAX_DEST_BYTES + 1);
    let cases = [
        "",
        ".",
        "..",
        "/abs",
        "a/b",
        "../x",
        "a\\b",
        "x\0y",
        "x\ny",
        long.as_str(),
        "a:b",
        "frpc.toml:stream",
        "name.",
        "name ",
        "CON",
        "con",
        "NUL.txt",
        "aux.toml",
        "Prn",
        "COM1",
        "lpt9.log",
    ];
    for dest in cases {
        let tmp = tempfile::tempdir().unwrap();
        let sd = tmp.path().join("state");
        let plan = file_plan(
            vec![("ok.toml", "CANARY"), (dest, "CANARY")],
            "true",
            vec![],
        );
        assert!(materialize_files(&plan, &sd).is_err(), "{dest:?}");
        assert!(!sd.exists(), "nothing created for {dest:?}");
    }
    validate_dest("frpc.toml").unwrap();
}

#[cfg(unix)]
#[test]
fn materialize_symlink_at_dest_is_refused_and_provider_not_spawned() {
    let tmp = tempfile::tempdir().unwrap();
    let sd = tmp.path().join("state");
    std::fs::create_dir(&sd).unwrap();
    let canary = tmp.path().join("canary");
    std::fs::write(&canary, "ORIGINAL").unwrap();
    std::os::unix::fs::symlink(&canary, sd.join("frpc.toml")).unwrap();
    let marker = tmp.path().join("spawned");
    let plan = file_plan(
        vec![("frpc.toml", "NEW-CANARY")],
        "sh",
        vec!["-c".into(), format!("touch {}", marker.display())],
    );
    assert!(start(&plan, &sd, Duration::from_secs(2)).is_err());
    assert_eq!(std::fs::read_to_string(&canary).unwrap(), "ORIGINAL");
    assert!(!marker.exists(), "provider must not be spawned");
}

#[cfg(unix)]
#[test]
fn materialize_writes_content_mode_0600_and_replaces_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let sd = tmp.path().join("state");
    let plan = file_plan(vec![("frpc.toml", "CANARY-1")], "true", vec![]);
    materialize_files(&plan, &sd).unwrap();
    let p = sd.join("frpc.toml");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "CANARY-1");
    assert_eq!(thegn_core::fsperm::mode_bits(&p).unwrap(), Some(0o600));
    assert_eq!(thegn_core::fsperm::mode_bits(&sd).unwrap(), Some(0o700));
    let old_inode = std::fs::metadata(&p).unwrap();

    // Replacement is rename-based: a new inode, new content, no temp left.
    let plan = file_plan(vec![("frpc.toml", "CANARY-2")], "true", vec![]);
    materialize_files(&plan, &sd).unwrap();
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "CANARY-2");
    use std::os::unix::fs::MetadataExt;
    assert_ne!(old_inode.ino(), std::fs::metadata(&p).unwrap().ino());
    let names: Vec<_> = std::fs::read_dir(&sd)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1, "no leftover temp: {names:?}");
}

#[cfg(unix)]
#[test]
fn materialize_failure_aborts_before_spawn_and_leaves_no_temp() {
    let tmp = tempfile::tempdir().unwrap();
    let sd = tmp.path().join("state");
    std::fs::create_dir(&sd).unwrap();
    // A directory at the destination: precheck refuses (non-regular file).
    std::fs::create_dir(sd.join("frpc.toml")).unwrap();
    let marker = tmp.path().join("spawned");
    let plan = file_plan(
        vec![("a.toml", "CANARY"), ("frpc.toml", "CANARY")],
        "sh",
        vec!["-c".into(), format!("touch {}", marker.display())],
    );
    assert!(start(&plan, &sd, Duration::from_secs(2)).is_err());
    assert!(!marker.exists(), "provider must not be spawned");
    assert!(
        !sd.join("a.toml").exists(),
        "earlier file must not be written when a later destination is refused"
    );
    let leftovers: Vec<_> = std::fs::read_dir(&sd)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        leftovers.len(),
        1,
        "only the planted dir remains: {leftovers:?}"
    );
}

#[cfg(unix)]
#[test]
fn materialize_refuses_hardlinked_state_file_before_replacement() {
    let tmp = tempfile::tempdir().unwrap();
    let sd = tmp.path().join("state");
    std::fs::create_dir(&sd).unwrap();
    let credential = sd.join("frpc.toml");
    std::fs::write(&credential, "ORIGINAL").unwrap();
    let second_name = tmp.path().join("outside-link");
    std::fs::hard_link(&credential, &second_name).unwrap();
    let marker = tmp.path().join("spawned");
    let plan = file_plan(
        vec![("frpc.toml", "REPLACEMENT")],
        "sh",
        vec!["-c".into(), format!("touch {}", marker.display())],
    );

    assert!(start(&plan, &sd, Duration::from_secs(2)).is_err());
    assert_eq!(std::fs::read_to_string(&second_name).unwrap(), "ORIGINAL");
    assert!(!marker.exists(), "provider must not be spawned");
}

#[cfg(unix)]
#[test]
fn materialize_refuses_symlinked_state_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let sd = tmp.path().join("state");
    std::os::unix::fs::symlink(&real, &sd).unwrap();
    let plan = file_plan(vec![("frpc.toml", "CANARY")], "true", vec![]);
    assert!(materialize_files(&plan, &sd).is_err());
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
}
