use super::*;
use thegn_core::config::{
    CustomVpnConfig, NetbirdConfig, OpenvpnConfig, TailscaleConfig, VpnConfig, VpnProviderKind,
    WireguardConfig, ZerotierConfig,
};
use thegn_core::sandbox::build_vpn_spec;

/// Build a resolved VpnSpec the way `resolve_scoped` would, for a given config.
fn spec(cfg: VpnConfig) -> VpnSpec {
    build_vpn_spec(
        &cfg,
        "thegn-repo-feat",
        thegn_core::config::SandboxProfile::Hardened,
    )
    .expect("provider enabled")
}

fn ts_cfg() -> VpnConfig {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Tailscale,
        ..VpnConfig::default()
    };
    // A literal auth key so resolve_identity needs no env/file.
    c.tailscale = TailscaleConfig {
        auth_key: "tskey-abc123".into(),
        ..TailscaleConfig::default()
    };
    c
}

fn flag_val<'a>(flags: &'a [String], flag: &str) -> Option<&'a str> {
    flags
        .iter()
        .position(|f| f == flag)
        .and_then(|i| flags.get(i + 1))
        .map(String::as_str)
}

fn env_val<'a>(env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

#[test]
fn factory_kind_matches_provider() {
    assert_eq!(for_provider(&spec(ts_cfg())).kind(), "tailscale");
    let mut hs = ts_cfg();
    hs.provider = VpnProviderKind::Headscale;
    assert_eq!(for_provider(&spec(hs)).kind(), "headscale");
}

#[test]
fn tailscale_tun_plan_isolates_caps_to_sidecar() {
    let s = spec(ts_cfg()); // default mode = sidecar => TUN
    let p = for_provider(&s);
    let plan = p.sidecar_plan("thegn-repo-feat-tgvpn").unwrap();
    assert_eq!(plan.image, "docker.io/tailscale/tailscale:stable");
    // NET_ADMIN + tun device live on the sidecar.
    assert!(plan.run_flags.iter().any(|f| f == "NET_ADMIN"));
    assert_eq!(flag_val(&plan.run_flags, "--device"), Some("/dev/net/tun"));
    assert_eq!(
        flag_val(&plan.run_flags, "--hostname"),
        Some("thegn-repo-feat")
    );
    assert_eq!(env_val(&plan.env, "TS_AUTHKEY"), Some("tskey-abc123"));
    assert_eq!(env_val(&plan.env, "TS_USERSPACE"), Some("false"));
    assert!(plan.proxy.is_none());
    // Requirements: worktree clean, sidecar carries the burden.
    let req = p.requirements();
    assert!(!req.worktree_needs_net_admin && !req.worktree_needs_tun);
    assert!(req.sidecar_needs_net_admin && req.sidecar_needs_tun);
    // MagicDNS.
    assert_eq!(p.dns().nameserver.as_deref(), Some("100.100.100.100"));
}

#[test]
fn tailscale_userspace_plan_uses_proxy_no_caps() {
    let mut c = ts_cfg();
    c.mode = VpnMode::Proxy;
    let s = spec(c);
    let p = for_provider(&s);
    let plan = p.sidecar_plan("c-tgvpn").unwrap();
    // No NET_ADMIN/tun in userspace mode.
    assert!(!plan.run_flags.iter().any(|f| f == "NET_ADMIN"));
    assert_eq!(env_val(&plan.env, "TS_USERSPACE"), Some("true"));
    assert_eq!(env_val(&plan.env, "TS_SOCKS5_SERVER"), Some("0.0.0.0:1055"));
    let proxy = plan.proxy.expect("userspace exposes a proxy");
    assert_eq!(proxy.all_proxy, "socks5://127.0.0.1:1055");
    // Requirements clean on both sides for userspace.
    let req = p.requirements();
    assert!(!req.sidecar_needs_tun && !req.worktree_needs_tun);
}

#[test]
fn headscale_sets_login_server_and_tags_in_extra_args() {
    let mut c = ts_cfg();
    c.provider = VpnProviderKind::Headscale;
    c.tailscale.login_server = "https://hs.example.com".into();
    c.tailscale.tags = vec!["tag:dev".into(), "tag:ci".into()];
    c.tailscale.accept_routes = true;
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();
    let extra = env_val(&plan.env, "TS_EXTRA_ARGS").unwrap();
    assert!(extra.contains("--login-server=https://hs.example.com"));
    assert!(extra.contains("--advertise-tags=tag:dev,tag:ci"));
    assert!(extra.contains("--accept-routes"));
    assert!(extra.contains("--ephemeral")); // default ephemeral = true
}

#[test]
fn tailscale_missing_auth_key_errors() {
    let mut c = ts_cfg();
    c.tailscale.auth_key = "env:TG_TEST_DEFINITELY_UNSET_KEY".into();
    let s = spec(c);
    assert!(for_provider(&s).sidecar_plan("c-tgvpn").is_err());
}

#[test]
fn wireguard_plan_mounts_conf_and_adds_tun() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Wireguard,
        ..VpnConfig::default()
    };
    c.wireguard = WireguardConfig {
        config_path: "/etc/wireguard/dev.conf".into(),
        config: String::new(),
    };
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();
    assert!(plan.run_flags.iter().any(|f| f == "NET_ADMIN"));
    assert_eq!(
        plan.mounts,
        vec![(
            "/etc/wireguard/dev.conf".to_string(),
            "/etc/wireguard/wg0.conf".to_string()
        )]
    );
    assert!(plan.files.is_empty());
}

#[test]
fn wireguard_inline_config_materializes_a_file() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Wireguard,
        ..VpnConfig::default()
    };
    c.wireguard.config = "[Interface]\nPrivateKey=xxx\n".into();
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].dest, "/etc/wireguard/wg0.conf");
    assert!(plan.files[0].contents.contains("PrivateKey"));
}

#[test]
fn wireguard_without_config_errors() {
    let c = VpnConfig {
        provider: VpnProviderKind::Wireguard,
        ..VpnConfig::default()
    };
    assert!(for_provider(&spec(c)).sidecar_plan("c-tgvpn").is_err());
}

#[test]
fn openvpn_plan_mounts_config_and_creds() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Openvpn,
        ..VpnConfig::default()
    };
    c.openvpn = OpenvpnConfig {
        config_path: "/home/me/dev.ovpn".into(),
        auth_user_pass: "user\npass".into(),
        extra_args: vec!["--verb".into(), "3".into()],
    };
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();
    assert!(plan.command.iter().any(|a| a == "--config"));
    assert!(plan.command.iter().any(|a| a == "--auth-user-pass"));
    assert!(plan.command.iter().any(|a| a == "--verb"));
    // creds materialized, config bind-mounted.
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].contents, "user\npass");
    assert!(plan.mounts.iter().any(|(h, _)| h == "/home/me/dev.ovpn"));
}

#[test]
fn netbird_plan_passes_setup_key_and_mgmt_url() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Netbird,
        ..VpnConfig::default()
    };
    c.netbird = NetbirdConfig {
        setup_key: "nbkey-xyz".into(),
        management_url: "https://nb.example.com".into(),
        hostname: String::new(),
    };
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();
    assert_eq!(env_val(&plan.env, "NB_SETUP_KEY"), Some("nbkey-xyz"));
    assert_eq!(
        env_val(&plan.env, "NB_MANAGEMENT_URL"),
        Some("https://nb.example.com")
    );
}

/// Regression: NetBird userspace (proxy) mode must pin its netstack SOCKS5
/// listener to the SAME port the worktree's ALL_PROXY targets — otherwise
/// NetBird listens on its default 1080 and every proxied connection to 1055
/// is refused while readiness still reports success.
#[test]
fn netbird_userspace_listener_port_matches_proxy() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Netbird,
        ..VpnConfig::default()
    };
    c.netbird.setup_key = "nbkey-xyz".into();
    c.mode = VpnMode::Proxy;
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();

    // Netstack mode on, and the listener port explicitly set.
    assert_eq!(env_val(&plan.env, "NB_USE_NETSTACK_MODE"), Some("true"));
    let listener = env_val(&plan.env, "NB_SOCKS5_LISTENER_PORT")
        .expect("userspace netbird must pin its SOCKS5 listener port");

    // The injected proxy points at that exact port.
    let proxy = plan.proxy.expect("userspace exposes a proxy");
    assert_eq!(proxy.all_proxy, format!("socks5://127.0.0.1:{listener}"));
    assert_eq!(listener, "1055");
}

/// TUN (sidecar) netbird must NOT set a userspace listener or a proxy.
#[test]
fn netbird_tun_mode_has_no_proxy_listener() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Netbird,
        ..VpnConfig::default()
    };
    c.netbird.setup_key = "nbkey-xyz".into(); // default mode = sidecar => TUN
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();
    assert!(env_val(&plan.env, "NB_SOCKS5_LISTENER_PORT").is_none());
    assert!(plan.proxy.is_none());
}

#[test]
fn zerotier_plan_requires_network_and_joins_it() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Zerotier,
        ..VpnConfig::default()
    };
    // Missing network id -> error.
    assert!(
        for_provider(&spec(c.clone()))
            .sidecar_plan("c-tgvpn")
            .is_err()
    );
    c.zerotier = ZerotierConfig {
        network_id: "8056c2e21c000001".into(),
        ..ZerotierConfig::default()
    };
    let plan = for_provider(&spec(c)).sidecar_plan("c-tgvpn").unwrap();
    assert_eq!(
        env_val(&plan.env, "ZEROTIER_JOIN_NETWORKS"),
        Some("8056c2e21c000001")
    );
}

#[test]
fn custom_plan_expands_templates_and_requires_up() {
    let mut c = VpnConfig {
        provider: VpnProviderKind::Custom,
        ..VpnConfig::default()
    };
    // Missing `up` -> error.
    c.custom = CustomVpnConfig {
        image: "docker.io/me/tunnel".into(),
        ..CustomVpnConfig::default()
    };
    assert!(
        for_provider(&spec(c.clone()))
            .sidecar_plan("c-tgvpn")
            .is_err()
    );

    c.custom.up = "mytunnel up --id {name}".into();
    c.custom.ready_check = "mytunnel status {worktree}".into();
    let s = spec(c);
    let plan = for_provider(&s).sidecar_plan("the-sidecar").unwrap();
    assert_eq!(plan.image, "docker.io/me/tunnel");
    let up = plan.command.last().unwrap();
    assert_eq!(up, "mytunnel up --id the-sidecar");
    // ready_check expands {worktree} to the hostname (the container name here).
    if let ReadyWhen::ExitZero = plan.ready.when {
        assert!(plan.ready.argv.last().unwrap().contains("thegn-repo-feat"));
    } else {
        panic!("custom ready uses exit-zero");
    }
}

#[test]
fn in_container_mode_moves_caps_to_worktree() {
    let mut c = ts_cfg();
    c.mode = VpnMode::InContainer;
    let req = for_provider(&spec(c)).requirements();
    assert!(req.worktree_needs_net_admin && req.worktree_needs_tun);
    assert!(!req.sidecar_needs_net_admin && !req.sidecar_needs_tun);
}

#[test]
fn proxy_env_exports_cover_upper_and_lower_case() {
    let p = Proxy {
        all_proxy: "socks5://127.0.0.1:1055".into(),
        no_proxy: vec!["localhost".into(), "127.0.0.1".into()],
    };
    let exports = p.env_exports();
    for key in ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "NO_PROXY"] {
        assert!(exports.iter().any(|(k, _)| k == key), "missing {key}");
    }
    assert_eq!(env_val(&exports, "NO_PROXY"), Some("localhost,127.0.0.1"));
}

#[test]
fn oci_runtime_argv_prefixes_subcommand() {
    let rt = OciRuntime::new(vec!["sudo".into(), "-n".into(), "podman".into()]);
    assert_eq!(
        rt.argv(&["run", "-d"]),
        vec!["sudo", "-n", "podman", "run", "-d"]
    );
}

/// The secret file must be 0600 the instant it exists — created with the mode,
/// not chmodded after a world-readable window.
#[cfg(unix)]
#[test]
fn write_secret_0600_creates_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wg.conf");
    write_secret_0600(&path, b"[Interface]\nPrivateKey=xxx\n").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "secret file must be created 0600");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"[Interface]\nPrivateKey=xxx\n"
    );
    // Overwriting an existing (possibly wrong-mode) file re-establishes 0600.
    write_secret_0600(&path, b"new").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
}

/// `stage_files` writes each secret 0600 in a 0700 dir, and `cleanup_staged`
/// removes the whole directory so secrets don't outlive the tunnel.
#[cfg(unix)]
#[test]
fn stage_files_are_0600_in_0700_dir_and_cleaned_up() {
    use std::os::unix::fs::PermissionsExt;
    // Isolate XDG_STATE_HOME so state_dir() lands in a scratch tree. This test
    // owns the env var for its duration (marked serial by the unique container).
    let home = tempfile::tempdir().unwrap();
    // The shared env lock, so no other env-mutating test in this binary can
    // observe or clobber it — and so the variable is restored even if an
    // assertion below panics, which the trailing remove_var could not do.
    let _env =
        thegn_core::testenv::EnvGuard::set(&[("XDG_STATE_HOME", &home.path().to_string_lossy())]);
    let container = format!("thegn-stagetest-{}", std::process::id());
    let plan = SidecarPlan {
        container: container.clone(),
        image: "img".into(),
        run_flags: vec![],
        env: vec![],
        mounts: vec![],
        files: vec![SidecarFile {
            contents: "secret-body".into(),
            dest: "/etc/wireguard/wg0.conf".into(),
        }],
        command: vec![],
        ready: ReadyProbe {
            argv: vec!["true".into()],
            when: ReadyWhen::ExitZero,
        },
        proxy: None,
    };
    let staged = stage_files(&plan).unwrap();
    assert_eq!(staged.len(), 1);
    let (host, dest) = &staged[0];
    assert_eq!(dest, "/etc/wireguard/wg0.conf");
    let host_path = std::path::Path::new(host);
    assert_eq!(std::fs::read(host_path).unwrap(), b"secret-body");
    let fmode = std::fs::metadata(host_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(fmode, 0o600);
    let dmode = std::fs::metadata(host_path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dmode, 0o700, "staged dir must be 0700");

    // Cleanup removes the whole per-sidecar dir.
    cleanup_staged(&container);
    assert!(!host_path.exists());
    assert!(!host_path.parent().unwrap().exists());
}

/// A wedged `exec` must be killed at the per-attempt bound, not block forever —
/// so `poll_ready`'s readiness deadline is actually honored.
#[cfg(unix)]
#[test]
fn run_output_timed_kills_hung_child() {
    // `sleep 30` stands in for a wedged `podman exec`.
    let argv = vec!["sleep".to_string(), "30".to_string()];
    let start = std::time::Instant::now();
    let res = run_output_timed(&argv, Duration::from_millis(200));
    let elapsed = start.elapsed();
    assert!(res.is_err(), "a hung child must surface as a timeout error");
    assert!(
        elapsed < Duration::from_secs(5),
        "must return near the timeout, not wait for the child ({elapsed:?})"
    );
}

/// A fast command still returns its output normally under the timeout.
#[cfg(unix)]
#[test]
fn run_output_timed_returns_fast_command_output() {
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        "printf hello".to_string(),
    ];
    let (ok, out) = run_output_timed(&argv, Duration::from_secs(5)).unwrap();
    assert!(ok);
    assert_eq!(out, "hello");
}

#[test]
fn filter_only_dns_suppresses_magicdns() {
    let mut c = ts_cfg();
    c.dns = thegn_core::config::VpnDnsMode::FilterOnly;
    let s = spec(c);
    assert!(for_provider(&s).dns().nameserver.is_none());
    // ...and TS_ACCEPT_DNS is off so the overlay doesn't override the filter.
    let plan = for_provider(&s).sidecar_plan("c-tgvpn").unwrap();
    assert_eq!(env_val(&plan.env, "TS_ACCEPT_DNS"), Some("false"));
}
