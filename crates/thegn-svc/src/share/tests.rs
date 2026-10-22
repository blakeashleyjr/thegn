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
            .any(|(key, value)| { key == "BORE_SECRET" && value.expose() == "literal-secret" })
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
            .any(|(key, value)| key == "BORE_SECRET" && value.expose() == secret)
    );
}

#[test]
fn secret_string_debug_never_shows_the_value() {
    let secret = "canary-325";
    let wrapped = SecretString::from(secret);
    assert!(!format!("{wrapped:?}").contains(secret));

    let spec = spec_with(
        BoreConfig {
            secret: secret.into(),
            ..BoreConfig::default()
        },
        3000,
    );
    let plan = process_plan(&spec);
    assert!(!format!("{plan:?}").contains(secret));
}

#[test]
fn share_plan_env_values_are_secret_strings() {
    let frp = FrpConfig {
        server_addr: "frps.example.com".into(),
        subdomain_host: "share.example.com".into(),
        token: "frp-canary-325".into(),
        ..FrpConfig::default()
    };
    let plan = process_plan(&frp_spec(frp, "wt", 3000));
    let _: &Vec<(String, SecretString)> = &plan.env;
    let _: &SecretString = &plan.files[0].contents;
}

#[test]
fn frp_token_reaches_only_the_materialized_file() {
    let token = "frp-canary-325";
    let frp = FrpConfig {
        server_addr: "frps.example.com".into(),
        subdomain_host: "share.example.com".into(),
        proxy_type: FrpProxyType::Https,
        token: token.into(),
        ..FrpConfig::default()
    };
    let plan = process_plan(&frp_spec(frp, "wt", 3000));

    assert!(plan.files[0].contents.expose().contains(token));
    assert!(!plan.args.iter().any(|arg| arg.contains(token)));
    assert!(
        plan.env
            .iter()
            .all(|(key, value)| { !key.contains(token) && !value.expose().contains(token) })
    );
    assert!(!format!("{plan:?}").contains(token));
}

#[test]
fn iroh_plan_carries_no_secret_channel() {
    let cfg = ShareConfig {
        provider: ShareProviderKind::Iroh,
        iroh: thegn_core::config::IrohShareConfig {
            extra_args: vec!["--relay".into(), "https://relay.example".into()],
        },
        ..ShareConfig::default()
    };
    let spec = build_share_spec(&cfg, "wt", 3000, None).expect("enabled");
    let plan = process_plan(&spec);

    assert!(plan.env.is_empty());
    assert!(plan.files.is_empty());
    assert_eq!(
        plan.args,
        vec![
            "listen-tcp",
            "--host",
            "127.0.0.1:3000",
            "--relay",
            "https://relay.example",
        ]
    );
}

#[test]
fn bore_secret_is_only_in_the_child_env() {
    let secret = "canary-325";
    let spec = spec_with(
        BoreConfig {
            secret: secret.into(),
            ..BoreConfig::default()
        },
        3000,
    );
    let plan = process_plan(&spec);

    assert_eq!(
        plan.env
            .iter()
            .filter(|(key, value)| key == "BORE_SECRET" && value.expose() == secret)
            .count(),
        1
    );
    assert!(plan.args.iter().all(|arg| !arg.contains(secret)));
    assert!(
        plan.files
            .iter()
            .all(|file| !file.contents.expose().contains(secret))
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
fn direct_client_env_matches_windows_key_casing() {
    let ambient = [
        ("Path".into(), "C:\\Windows\\System32".into()),
        ("windir".into(), "C:\\Windows".into()),
    ];

    assert_eq!(
        direct_client_env(ambient),
        [
            ("Path".into(), "C:\\Windows\\System32".into()),
            ("windir".into(), "C:\\Windows".into()),
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

    let toml = plan.files[0].contents.expose();
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
    let toml = plan.files[0].contents.expose();
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
    let toml = plan.files[0].contents.expose();
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
                .expose()
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
    assert!(plan.files[0].contents.expose().contains("type = \"udp\""));
    assert!(
        plan.files[0]
            .contents
            .expose()
            .contains("remotePort = 6001")
    );
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
            .expose()
            .contains("serverAddr = \"2001:db8::7\"")
    );
    assert!(
        !plan.files[0]
            .contents
            .expose()
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
    let document = plan.files[0].contents.expose();
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
    assert!(plan.files[0].contents.expose().contains(&expected));
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

#[test]
fn serve_down_ok_when_status_lacks_the_port() {
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);
    let runtimes = [OciRuntime::podman()];
    let mut calls = Vec::new();
    let result = serve_down_with(&runtimes, &plan, |runtime, argv| {
        calls.push((runtime.clone(), argv.to_vec()));
        if argv == plan.down_argv.as_slice() {
            Ok((true, String::new()))
        } else {
            Ok((true, r#"{"TCP":{},"Web":{},"AllowFunnel":{}}"#.into()))
        }
    });
    assert!(result.is_ok());
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].1, vec!["tailscale", "serve", "status", "--json"]);
}

#[test]
fn serve_down_errs_when_port_still_served() {
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);
    let runtimes = [OciRuntime::podman()];
    let result = serve_down_with(&runtimes, &plan, |_, argv| {
        if argv == plan.down_argv.as_slice() {
            Ok((true, String::new()))
        } else {
            Ok((true, r#"{"TCP":{"443":{"HTTPS":true}}}"#.into()))
        }
    });
    let error = result.expect_err("a still-served port is unverified");
    assert!(error.to_string().contains("teardown unverified"));
}

#[test]
fn serve_down_errs_when_down_fails_on_every_runtime() {
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);
    let runtimes = [OciRuntime::podman(), OciRuntime::docker()];
    let mut attempts = 0;
    let result = serve_down_with(&runtimes, &plan, |_, argv| {
        attempts += 1;
        if argv == plan.down_argv.as_slice() && attempts == 1 {
            Err(anyhow::anyhow!("runtime unavailable"))
        } else if argv == plan.down_argv.as_slice() {
            Ok((false, String::new()))
        } else {
            Err(anyhow::anyhow!("status unavailable"))
        }
    });
    let error = result.expect_err("all down attempts failed");
    assert_eq!(attempts, 4);
    assert!(error.to_string().contains("teardown unverified"));
}

#[test]
fn serve_down_errs_on_unparseable_status() {
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);
    let runtimes = [OciRuntime::podman()];
    let result = serve_down_with(&runtimes, &plan, |_, argv| {
        if argv == plan.down_argv.as_slice() {
            Ok((true, String::new()))
        } else {
            Ok((true, "{not json".into()))
        }
    });
    let error = result.expect_err("malformed status cannot verify teardown");
    assert!(error.to_string().contains("teardown unverified"));
}

#[test]
fn serve_down_errs_when_status_cannot_be_read() {
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);
    let runtimes = [OciRuntime::podman()];
    let result = serve_down_with(&runtimes, &plan, |_, argv| {
        if argv == plan.down_argv.as_slice() {
            Ok((true, String::new()))
        } else {
            Err(anyhow::anyhow!("runtime unavailable"))
        }
    });
    let error = result.expect_err("unreadable status cannot verify teardown");
    assert!(error.to_string().contains("teardown unverified"));
}

#[test]
fn serve_down_ok_when_down_fails_but_status_lacks_the_port() {
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);
    let runtimes = [OciRuntime::podman()];
    let result = serve_down_with(&runtimes, &plan, |_, argv| {
        if argv == plan.down_argv.as_slice() {
            Ok((false, String::new()))
        } else {
            Ok((true, "{}".into()))
        }
    });
    assert!(result.is_ok());
}

#[test]
fn serve_up_uses_only_the_recorded_runtime_and_id() {
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);
    let recorded = crate::vpn::RecordedSidecar {
        runtime: OciRuntime::docker(),
        id: "a".repeat(64),
    };
    let mut calls = Vec::new();
    let endpoint = serve_up_with(&recorded, &plan, |runtime, target, argv| {
        calls.push((runtime.clone(), target.to_string(), argv.to_vec()));
        if argv == plan.up_argv.as_slice() {
            Ok((true, String::new()))
        } else {
            Ok((
                true,
                r#"{"Self":{"DNSName":"node.example.ts.net."}}"#.into(),
            ))
        }
    })
    .unwrap();

    assert_eq!(endpoint.as_url(), Some("https://node.example.ts.net"));
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(runtime, target, _)| {
        runtime == &OciRuntime::docker() && target == &recorded.id && target != "c-tgvpn"
    }));
}

#[test]
fn serve_up_refuses_without_a_record() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);

    let result = serve_up("c-missing", &plan);
    assert!(matches!(result, Err(ref error) if error.to_string().contains("no recorded runtime")));
}

#[test]
fn serve_down_without_a_record_is_unverified() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let plan = serve_plan(thegn_core::config::TailscaleShareConfig::default(), 3000);

    let result = serve_down("c-missing", &plan);
    assert!(matches!(result, Err(ref error) if error.to_string().contains("teardown unverified")));
}

#[test]
fn serve_status_parser_cases() {
    for (json, expected) in [
        ("", false),
        ("{}", false),
        (r#"{"TCP":{"443":{"HTTPS":true}}}"#, true),
        (r#"{"Web":{"host.ts.net:443":{}}}"#, true),
        (r#"{"AllowFunnel":{"host.ts.net:443":true}}"#, true),
        (
            r#"{"TCP":{"8443":{"HTTPS":true}},"Web":{"host.ts.net:8443":{}}}"#,
            false,
        ),
    ] {
        assert_eq!(
            serve_status_has_port(json, 443).unwrap(),
            expected,
            "{json}"
        );
    }
    assert!(serve_status_has_port("{broken", 443).is_err());
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

    let running = start(&plan, &tmp, Duration::from_secs(5)).expect("start");
    assert_eq!(running.public_url.canonical(), "http://fixed.example");

    // Give the child time to emit several post-return lines: under the old
    // break-on-send-fail behavior it would SIGPIPE and exit here.
    std::thread::sleep(Duration::from_millis(800));
    assert!(
        !running.has_exited(),
        "child must still be running — a closed stdout would have SIGPIPE-killed it"
    );

    running.stop();
    let _ = std::fs::remove_dir_all(&tmp); // best-effort: test tmp cleanup
}

#[cfg(unix)]
#[test]
fn start_cancellable_aborts_a_child_that_never_reports_a_url() {
    let state = tempfile::tempdir().expect("private temporary state");
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), "exec sleep 30".into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    };
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = cancel.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        signal.store(true, std::sync::atomic::Ordering::Release);
    });
    let started = std::time::Instant::now();
    let result = start_cancellable(&plan, state.path(), Duration::from_secs(10), &cancel);
    assert!(
        result.is_err_and(|error| error.downcast_ref::<ShareStartCancelled>().is_some()),
        "startup cancellation must retain its typed error"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    canceller.join().expect("canceller thread");
}

#[cfg(unix)]
#[test]
fn stop_handle_is_idempotent_and_never_signals_a_reaped_child() {
    let state = tempfile::tempdir().expect("private temporary state");
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), "exec sleep 30".into()],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::Fixed("http://fixed.example".into()),
    };
    let running = start(&plan, state.path(), Duration::from_secs(1)).expect("start share");
    let handle = running.stop_handle();
    assert_eq!(handle.stop(), StopOutcome::Terminated);
    running.wait();
    assert_eq!(handle.stop(), StopOutcome::AlreadyExited);
    assert_eq!(handle.stop(), StopOutcome::AlreadyExited);
}

#[cfg(unix)]
#[test]
fn spawned_client_sees_only_allowlisted_env() {
    let temp = tempfile::tempdir().expect("temporary client directory");
    let script_path = temp.path().join("fake-client");
    let dump_path = temp.path().join("environment.txt");
    let script = "#!/bin/sh\nenv > \"$1\"\nprintf 'listening at fake.example:4123\\n'\nsleep 2\n";
    std::fs::write(&script_path, script).expect("write fake client");

    let plan = SharePlan {
        program: "/bin/sh".into(),
        args: vec![
            script_path.to_string_lossy().into_owned(),
            dump_path.to_string_lossy().into_owned(),
        ],
        env: vec![
            ("RUST_LOG".into(), "info".into()),
            ("BORE_SECRET".into(), SecretString::from("child-canary-325")),
        ],
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "listening at ".into(),
            scheme: "http".into(),
        },
    };
    let path = std::env::var_os("PATH").expect("test runner PATH");
    let ambient = [
        ("PATH".into(), path.clone()),
        ("LANG".into(), "C".into()),
        ("HOME".into(), "/private/home".into()),
        ("THEGN_TEST_CANARY_SECRET".into(), "canary-value".into()),
        ("GITHUB_TOKEN".into(), "github-value".into()),
    ];
    let cancel = AtomicBool::new(false);
    let running = start_with_ambient(
        &plan,
        temp.path(),
        Duration::from_secs(1),
        ambient,
        &cancel,
        ProcessGrouping::Foreground,
    )
    .expect("start fake client");
    let dumped_env = std::fs::read_to_string(&dump_path).expect("read child environment");

    assert!(
        dumped_env
            .lines()
            .any(|line| line == format!("PATH={}", path.to_string_lossy()))
    );
    assert!(dumped_env.lines().any(|line| line == "RUST_LOG=info"));
    assert!(
        dumped_env
            .lines()
            .any(|line| line == "BORE_SECRET=child-canary-325")
    );
    assert!(!dumped_env.contains("THEGN_TEST_CANARY_SECRET"));
    assert!(!dumped_env.contains("canary-value"));
    assert!(!dumped_env.contains("HOME="));
    assert!(!dumped_env.contains("GITHUB_TOKEN="));
    let allowed_keys = DIRECT_CLIENT_ENV_KEYS
        .iter()
        .copied()
        .chain(["RUST_LOG", "BORE_SECRET", "PWD", "SHLVL", "_", "OLDPWD"])
        .collect::<Vec<_>>();
    for line in dumped_env.lines() {
        let (key, _) = line.split_once('=').expect("environment entry has key");
        assert!(
            allowed_keys.contains(&key),
            "unexpected child environment key: {key}"
        );
    }
    running.stop();
}

#[cfg(unix)]
fn group_stop_plan(script: String) -> SharePlan {
    SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    }
}

#[cfg(unix)]
#[test]
fn group_stop_kills_pipe_holding_grandchild() {
    let state = tempfile::tempdir().expect("private temporary state");
    let pid_path = state.path().join("grandchild.pid");
    let script = format!(
        "sleep 30 & echo $! > '{}'; echo 'public: grand.example:443'; wait",
        pid_path.display()
    );
    let plan = group_stop_plan(script);
    let running = start_cancellable(
        &plan,
        state.path(),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .expect("start grouped client");
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .expect("grandchild pid")
        .trim()
        .parse()
        .expect("pid number");
    assert_eq!(running.stop_handle().stop(), StopOutcome::Terminated);
    let started = Instant::now();
    let report = running.wait_verified();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(report.leaked_readers, 0);
    let mut gone = false;
    for _ in 0..100 {
        // SAFETY: signal zero checks process existence and does not deliver a signal.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        gone,
        "pipe-holding grandchild must be gone after group stop"
    );
}

#[cfg(unix)]
#[test]
fn group_stop_escalates_when_the_client_ignores_sigterm() {
    let state = tempfile::tempdir().expect("private temporary state");
    let plan = group_stop_plan(
        "trap '' TERM; echo 'public: stubborn.example:443'; while :; do sleep 1; done".into(),
    );
    let running = start_cancellable(
        &plan,
        state.path(),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .expect("start grouped client");
    let started = Instant::now();
    assert_eq!(
        running
            .stop_handle()
            .stop_with_grace(Duration::from_millis(200)),
        StopOutcome::Terminated
    );
    let report = running.wait_verified();
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(report.leaked_readers, 0);
}

#[cfg(unix)]
#[test]
fn group_stop_repeated_stop_arms_one_escalation() {
    let state = tempfile::tempdir().expect("private temporary state");
    let plan = group_stop_plan(
        "trap '' TERM; echo 'public: stubborn.example:443'; while :; do sleep 1; done".into(),
    );
    let running = start_cancellable(
        &plan,
        state.path(),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .expect("start grouped client");
    let handle = running.stop_handle();
    let counter = Arc::clone(
        &handle
            .child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .escalation_count,
    );
    assert_eq!(
        handle.stop_with_grace(Duration::from_secs(1)),
        StopOutcome::Terminated
    );
    assert_eq!(
        handle.stop_with_grace(Duration::from_secs(1)),
        StopOutcome::Terminated
    );
    assert_eq!(counter.load(Ordering::Relaxed), 1);
    running.wait_verified();
    assert_eq!(handle.stop(), StopOutcome::AlreadyExited);
}

#[cfg(unix)]
#[test]
fn group_stop_now_kills_term_ignoring_client_immediately() {
    let state = tempfile::tempdir().expect("private temporary state");
    let plan =
        group_stop_plan("trap '' TERM; echo 'public: stubborn.example:443'; exec sleep 30".into());
    let running = start_cancellable(
        &plan,
        state.path(),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .expect("start grouped client");
    let handle = running.stop_handle();
    let pid = handle
        .child
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pid;
    assert_eq!(handle.stop_now(), StopOutcome::Terminated);
    // SIGKILL is delivered asynchronously: poll briefly instead of racing the kernel.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !crate::plugin::proc::leader_exited_nowait(pid).expect("check stopped leader") {
        assert!(
            std::time::Instant::now() < deadline,
            "a TERM-ignoring leader survived stop_now"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let report = running.wait_verified();
    assert_eq!(report.leaked_readers, 0);
}

#[cfg(unix)]
#[test]
fn group_stop_foreground_start_keeps_the_callers_group() {
    let state = tempfile::tempdir().expect("private temporary state");
    let foreground_path = state.path().join("foreground-group");
    let script = format!(
        "if kill -0 -$$ 2>/dev/null; then echo own; else echo inherited; fi > '{}'; echo 'public: foreground.example:443'; exec sleep 30",
        foreground_path.display()
    );
    let plan = SharePlan {
        program: "sh".into(),
        args: vec!["-c".into(), script],
        env: Vec::new(),
        files: Vec::new(),
        url_rule: UrlRule::AfterMarker {
            marker: "public: ".into(),
            scheme: "https".into(),
        },
    };
    let running = start(&plan, state.path(), Duration::from_secs(3)).expect("foreground start");
    let group = std::fs::read_to_string(foreground_path).expect("foreground group check");
    assert_eq!(group.trim(), "inherited");
    running.stop();

    let grouped_path = state.path().join("grouped-group");
    let plan = group_stop_plan(format!(
        "if kill -0 -$$ 2>/dev/null; then echo own; else echo inherited; fi > '{}'; echo 'public: grouped.example:443'; exec sleep 30",
        grouped_path.display()
    ));
    let running = start_cancellable(
        &plan,
        state.path(),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .expect("grouped start");
    let group = std::fs::read_to_string(grouped_path).expect("grouped group check");
    assert_eq!(group.trim(), "own");
    running.stop();
}

#[cfg(unix)]
#[test]
fn group_stop_supervised_wait_returns_on_exit_without_poll() {
    let state = tempfile::tempdir().expect("private temporary state");
    let plan = group_stop_plan("echo 'public: quick.example:443'; sleep 0.3".into());
    let running = start_cancellable(
        &plan,
        state.path(),
        Duration::from_secs(3),
        &AtomicBool::new(false),
    )
    .expect("start grouped client");
    let started = Instant::now();
    let report = running.wait_verified();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(matches!(
        report.outcome,
        StopOutcome::AlreadyExited | StopOutcome::Terminated
    ));
    assert_eq!(report.leaked_readers, 0);
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

#[cfg(unix)]
#[test]
fn start_cancelled_before_spawn_starts_nothing_and_removes_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let statedir = tmp.path().join("state");
    let marker = tmp.path().join("spawned");
    let program = tmp.path().join("no-such-provider");
    let plan = file_plan(
        vec![("credential.txt", "SYNTHETIC-CREDENTIAL")],
        program.to_str().unwrap(),
        vec![],
    );
    let cancel = std::sync::atomic::AtomicBool::new(true);

    let result = start_cancellable(&plan, &statedir, Duration::from_secs(10), &cancel);

    assert!(result.is_err_and(|error| { error.downcast_ref::<ShareStartCancelled>().is_some() }));
    std::thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists(), "cancelled provider must never be spawned");
    assert!(
        !statedir.join("credential.txt").exists(),
        "cancelled start must not leave credentials"
    );
}

#[test]
fn share_provider_seam_is_sealed_and_claims_no_plugins() {
    let src = include_str!("mod.rs");
    assert!(!src.contains("plug in here later"));
    assert!(!src.contains("keeps room for"));
    assert!(src.contains("pub trait ShareProvider: sealed::Sealed"));

    let spec = spec_with(BoreConfig::default(), 3000);
    assert_eq!(for_provider(&spec).kind(), "bore");
}

#[test]
fn bore_extra_args_secret_flags_are_refused() {
    let cases = [
        (
            vec!["--secret".to_owned(), "canary-325x".to_owned()],
            "--secret",
        ),
        (vec!["--secret=canary-325x".to_owned()], "--secret"),
        (vec!["-s".to_owned(), "canary-325x".to_owned()], "-s"),
        (vec!["-scanary-325x".to_owned()], "-s"),
    ];

    for (extra_args, flag) in cases {
        let spec = spec_with(
            BoreConfig {
                extra_args,
                ..BoreConfig::default()
            },
            3000,
        );
        let error = match for_provider(&spec).launch() {
            Err(error) => error,
            Ok(_) => panic!("bore secret flags in extra_args must be refused"),
        };
        let message = format!("{error:#}");
        assert!(message.contains(flag), "refusal identifies {flag} flag");
        assert!(
            !message.contains("canary-325x"),
            "refusal never exposes the secret value"
        );
    }
}

#[test]
fn bore_extra_args_without_secret_flags_still_launch() {
    // Positive control: these non-secret arguments launch before and after the refusal guard.
    let spec = spec_with(
        BoreConfig {
            extra_args: vec!["--max-conn".into(), "10".into()],
            ..BoreConfig::default()
        },
        3000,
    );
    let plan = process_plan(&spec);

    let expected = ["--max-conn".to_owned(), "10".to_owned()];
    assert!(plan.args.ends_with(&expected));
}
