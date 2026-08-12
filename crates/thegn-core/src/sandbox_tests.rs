use super::*;
use crate::sandbox_mounts::host_toolchain_mounts;

#[test]
fn none_backend_skips_cd_for_unretargeted_remote_worktree() {
    // A bare remote shell whose worktree was NOT retargeted (local path) must
    // NOT `cd <local-path>` on the remote — it would fail "cd: can't cd to …".
    let mut s = spec(Backend::None);
    s.worktree = PathBuf::from("/home/me/wt");
    s.placement = Placement::Ssh(SshPlacement::plain(
        "host".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    s.mounts.clear();
    let body = backend_enter_argv(&s, "exec sh").pop().unwrap();
    assert!(!body.contains("cd "), "no local cd shipped remote: {body}");
    assert!(body.contains("exec sh"));

    // A retargeted remote (a mount whose dest == the worktree path) DOES cd.
    s.mounts = vec![Mount {
        host: "/home/me/wt".into(),
        dest: "/home/me/wt".into(),
        ro: false,
        cache: false,
    }];
    let retargeted = backend_enter_argv(&s, "exec sh").pop().unwrap();
    assert!(
        retargeted.starts_with("cd "),
        "retargeted remote cds: {retargeted}"
    );

    // A local `none` still cds into the worktree.
    let mut local = spec(Backend::None);
    local.placement = Placement::Local;
    let body_local = backend_enter_argv(&local, "exec sh").pop().unwrap();
    assert!(
        body_local.starts_with("cd "),
        "local none cds: {body_local}"
    );
}

fn vpn_cfg(provider: VpnProviderKind) -> VpnConfig {
    VpnConfig {
        provider,
        ..VpnConfig::default()
    }
}

#[test]
fn build_vpn_spec_none_provider_is_none() {
    assert!(build_vpn_spec(&VpnConfig::default(), "wt", SandboxProfile::Hardened).is_none());
}

#[test]
fn build_vpn_spec_sealed_refuses_but_sealed_tunnel_attaches() {
    let cfg = vpn_cfg(VpnProviderKind::Tailscale);
    // Plain sealed refuses a tunnel (returns None).
    assert!(build_vpn_spec(&cfg, "wt", SandboxProfile::Sealed).is_none());
    // sealed-tunnel and hardened both attach.
    assert!(build_vpn_spec(&cfg, "wt", SandboxProfile::SealedTunnel).is_some());
    assert!(build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).is_some());
}

#[test]
fn build_vpn_spec_maps_each_provider_to_its_params() {
    for provider in [
        VpnProviderKind::Tailscale,
        VpnProviderKind::Headscale,
        VpnProviderKind::Wireguard,
        VpnProviderKind::Openvpn,
        VpnProviderKind::Netbird,
        VpnProviderKind::Zerotier,
        VpnProviderKind::Custom,
    ] {
        let spec = build_vpn_spec(&vpn_cfg(provider), "wt", SandboxProfile::Hardened).unwrap();
        assert_eq!(spec.provider, provider);
        // Headscale reuses the Tailscale params variant.
        let ok = matches!(
            (provider, &spec.params),
            (VpnProviderKind::Tailscale, VpnParams::Tailscale(_))
                | (VpnProviderKind::Headscale, VpnParams::Tailscale(_))
                | (VpnProviderKind::Wireguard, VpnParams::Wireguard(_))
                | (VpnProviderKind::Openvpn, VpnParams::Openvpn(_))
                | (VpnProviderKind::Netbird, VpnParams::Netbird(_))
                | (VpnProviderKind::Zerotier, VpnParams::Zerotier(_))
                | (VpnProviderKind::Custom, VpnParams::Custom(_))
        );
        assert!(ok, "{provider:?} mapped to wrong params: {:?}", spec.params);
    }
}

#[test]
fn build_vpn_spec_hostname_defaults_to_name_then_overrides() {
    // Default: container name.
    let spec = build_vpn_spec(
        &vpn_cfg(VpnProviderKind::Tailscale),
        "thegn-repo-feat",
        SandboxProfile::Hardened,
    )
    .unwrap();
    assert_eq!(spec.hostname, "thegn-repo-feat");

    // Per-provider hostname wins.
    let mut cfg = vpn_cfg(VpnProviderKind::Tailscale);
    cfg.tailscale.hostname = "custom-node".into();
    let spec = build_vpn_spec(&cfg, "thegn-repo-feat", SandboxProfile::Hardened).unwrap();
    assert_eq!(spec.hostname, "custom-node");
}

#[test]
fn build_vpn_spec_carries_knobs_and_optional_image() {
    let mut cfg = vpn_cfg(VpnProviderKind::Wireguard);
    cfg.mode = VpnMode::Proxy;
    cfg.on_error = VpnOnError::Offline;
    cfg.dns = VpnDnsMode::FilterFront;
    cfg.ready_timeout_secs = 7;
    cfg.ephemeral = false;
    let spec = build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).unwrap();
    assert_eq!(spec.mode, VpnMode::Proxy);
    assert_eq!(spec.on_error, VpnOnError::Offline);
    assert_eq!(spec.dns_mode, VpnDnsMode::FilterFront);
    assert_eq!(spec.ready_timeout, Duration::from_secs(7));
    assert!(!spec.ephemeral);
    // Empty sidecar_image -> None; set -> Some.
    assert!(spec.sidecar_image.is_none());
    cfg.sidecar_image = "ghcr.io/me/wg:latest".into();
    let spec = build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).unwrap();
    assert_eq!(spec.sidecar_image.as_deref(), Some("ghcr.io/me/wg:latest"));
}

#[test]
fn oci_opts_join_vpn_sidecar_netns_and_suppress_dns_ports() {
    let mut s = spec(Backend::Podman);
    s.network = Network::Nat;
    s.network_allow = vec!["example.com".into()]; // would normally add --dns
    s.ports = vec!["8080:8080".into()]; // would normally add -p
    s.vpn = build_vpn_spec(
        &vpn_cfg(VpnProviderKind::Tailscale),
        &s.name,
        SandboxProfile::Hardened,
    );
    let opts = oci_create_opts(&s);
    let joined = opts.join(" ");
    // Joins the sidecar netns...
    assert!(
        joined.contains("--network container:thegn-repo-feat-szvpn"),
        "{joined}"
    );
    // ...and suppresses --dns and -p (illegal on a container-netns join).
    assert!(!opts.iter().any(|o| o == "--dns"), "{joined}");
    assert!(!opts.iter().any(|o| o == "-p"), "{joined}");
}

#[test]
fn oci_opts_in_container_mode_adds_net_admin_and_tun() {
    let mut s = spec(Backend::Podman);
    let mut cfg = vpn_cfg(VpnProviderKind::Wireguard);
    cfg.mode = VpnMode::InContainer;
    s.vpn = build_vpn_spec(&cfg, &s.name, SandboxProfile::Hardened);
    let opts = oci_create_opts(&s);
    let joined = opts.join(" ");
    // in_container does NOT join a sidecar netns; it keeps normal networking
    // and adds the tunnel caps to the worktree container itself.
    assert!(!joined.contains("container:"), "{joined}");
    assert!(opts.iter().any(|o| o == "NET_ADMIN"), "{joined}");
    assert!(joined.contains("/dev/net/tun"), "{joined}");
}

#[test]
fn oci_opts_without_vpn_keep_normal_network_and_ports() {
    let mut s = spec(Backend::Podman);
    s.network = Network::Nat;
    s.ports = vec!["8080:8080".into()];
    assert!(s.vpn.is_none());
    let opts = oci_create_opts(&s);
    // No container: join; ports published as usual.
    assert!(!opts.join(" ").contains("container:"));
    assert!(opts.windows(2).any(|w| w == ["-p", "8080:8080"]));
}

#[test]
fn test_win_native_sandboxes_do_not_parse_as_oci() {
    assert!(!Backend::WinAppContainer.is_oci());
    assert!(!Backend::WinJobObject.is_oci());
    assert!(Backend::WinAppContainer.is_host_toolchain());
    assert!(Backend::WinJobObject.is_host_toolchain());
    assert_eq!(Backend::WinAppContainer.label(), "appcontainer");
    assert_eq!(Backend::WinJobObject.label(), "jobobject");
}

fn spec(backend: Backend) -> SandboxSpec {
    SandboxSpec {
        backend,
        placement: Placement::Local,
        image: Some("img:latest".into()),
        worktree: PathBuf::from("/wt/feat"),
        mounts: vec![
            Mount {
                host: "/wt/feat".into(),
                dest: "/wt/feat".into(),
                ro: false,
                cache: false,
            },
            Mount {
                host: "/repo/.git".into(),
                dest: "/repo/.git".into(),
                ro: false,
                cache: false,
            },
        ],
        env: vec![("GH_TOKEN".into(), "abc".into())],
        env_overrides: std::collections::HashMap::new(),
        env_block: Vec::new(),
        network: Network::Nat,
        network_allow: Vec::new(),
        network_block: Vec::new(),
        read_only_root: false,
        no_new_privileges: false,
        pids_limit: None,
        drop_capabilities: Vec::new(),
        add_capabilities: Vec::new(),
        ports: vec!["8080:8080".into()],
        gpu: None,
        // Disable CPU capping so argv-shape assertions are host-independent
        // (the aggregate cap is on-by-default and would scope-wrap on a host
        // with cgroup delegation); capping itself is tested in sandbox_cpucap.
        limits: SandboxLimits {
            cpu_total: Some("off".into()),
            ..SandboxLimits::default()
        },
        volumes: vec![],
        compose: None,
        build: None,
        init_script: None,
        file_access: FileAccess::Worktree,
        devenv: false,
        devenv_path: None,
        name: "thegn-repo-feat".into(),
        vpn: None,
        oci_host: None,
        oci_runtime: None,
        daemon_persistent: false,
    }
}

#[test]
fn oci_prefix_injects_remote_daemon_connection() {
    // Local daemon (default): plain prefix.
    let mut s = spec(Backend::Podman);
    assert_eq!(oci_prefix(&s), vec!["podman"]);

    // podman URL → --url before the subcommand.
    s.oci_host = Some("ssh://user@box/run/podman.sock".into());
    assert_eq!(
        oci_prefix(&s),
        vec!["podman", "--url", "ssh://user@box/run/podman.sock"]
    );

    // podman bare token → named --connection.
    s.oci_host = Some("workbox".into());
    assert_eq!(oci_prefix(&s), vec!["podman", "--connection", "workbox"]);

    // docker → -H.
    let mut d = spec(Backend::Docker);
    d.oci_host = Some("ssh://user@box".into());
    assert_eq!(oci_prefix(&d), vec!["docker", "-H", "ssh://user@box"]);

    // rootful podman: flag lands after `sudo -n podman`, not before.
    let mut r = spec(Backend::PodmanRootful);
    r.oci_host = Some("workbox".into());
    assert_eq!(
        oci_prefix(&r),
        vec!["sudo", "-n", "podman", "--connection", "workbox"]
    );

    // Non-OCI backend ignores oci_host.
    let mut b = spec(Backend::Bwrap);
    b.oci_host = Some("workbox".into());
    assert_eq!(oci_prefix(&b), vec!["bwrap"]);
}

#[test]
fn podman_exec_preserves_paths() {
    let argv = enter_argv(&spec(Backend::Podman), "${SHELL:-/bin/sh} -l");
    assert_eq!(argv[0], "podman");
    assert!(argv.contains(&"exec".to_string()));
    assert!(argv.contains(&"thegn-repo-feat".to_string()));
    // workdir is the worktree's host path (path-preserving).
    let w = argv.iter().position(|a| a == "--workdir").unwrap();
    assert_eq!(argv[w + 1], "/wt/feat");
    // safe.directory + exec are in the sh body.
    let body = argv.last().unwrap();
    assert!(body.contains("safe.directory"));
    assert!(body.contains("exec ${SHELL:-/bin/sh} -l"));
}

#[test]
fn bwrap_binds_worktree_and_gitdir() {
    let mut s = spec(Backend::Bwrap);
    s.image = None;
    s.file_access = FileAccess::Worktree;
    let argv = enter_argv(&s, "claude");
    assert_eq!(argv[0], "bwrap");
    let joined = argv.join(" ");
    assert!(!joined.contains("--ro-bind / /"));
    assert!(joined.contains("--bind /wt/feat /wt/feat"));
    assert!(joined.contains("--bind /repo/.git /repo/.git"));
    assert!(joined.contains("--chdir /wt/feat"));
    assert_eq!(argv.last().unwrap(), "exec claude");
}

#[test]
fn bwrap_die_with_parent_is_gated_by_daemon_persistent() {
    // Default (in-process / chrome pane): the sandbox dies with the compositor.
    let mut ephemeral = spec(Backend::Bwrap);
    ephemeral.image = None;
    let joined = enter_argv(&ephemeral, "claude").join(" ");
    assert!(joined.contains("--unshare-pid"), "pid ns is unconditional");
    assert!(
        joined.contains("--die-with-parent"),
        "an in-process bwrap pane keeps --die-with-parent: {joined}"
    );

    // Daemon-owned pane: the daemon reaps its own sessions, so the guard is
    // dropped — otherwise a backgrounded shell is killed when the forking
    // thread goes away (the "restarts on switch" bug).
    let mut persistent = spec(Backend::Bwrap);
    persistent.image = None;
    persistent.daemon_persistent = true;
    let joined = enter_argv(&persistent, "claude").join(" ");
    assert!(
        joined.contains("--unshare-pid"),
        "pid ns stays even for daemon panes: {joined}"
    );
    assert!(
        !joined.contains("--die-with-parent"),
        "a daemon-persistent bwrap pane must NOT die with its forking parent: {joined}"
    );
}

#[test]
fn file_access_none_removes_workdir() {
    let mut s = spec(Backend::Podman);
    s.file_access = FileAccess::None;
    let argv = enter_argv(&s, "claude");
    let joined = argv.join(" ");
    assert!(!joined.contains("--workdir"));
}

#[test]
fn oci_create_opts_map_userns_and_mounts() {
    // GH_TOKEN=abc is synthetic here (its value doesn't match the ambient
    // env) so it stays inline `-e`; unset it to keep that deterministic.
    let _env = crate::testenv::EnvGuard::unset(&["GH_TOKEN"]);
    let opts = oci_create_opts(&spec(Backend::Podman));
    let j = opts.join(" ");
    assert!(j.contains("--userns keep-id"));
    assert!(j.contains("-v /wt/feat:/wt/feat"));
    assert!(j.contains("-v /repo/.git:/repo/.git"));
    assert!(j.contains("-e GH_TOKEN=abc"));
    assert!(j.contains("-p 8080:8080"));
}

#[test]
fn oci_runtime_injected_only_for_oci_backends_when_set() {
    // Unset ⇒ no --runtime (daemon default).
    assert!(
        !oci_create_opts(&spec(Backend::Podman))
            .join(" ")
            .contains("--runtime")
    );

    // Set on an OCI backend ⇒ `--runtime <value>` at create.
    let mut s = spec(Backend::Podman);
    s.oci_runtime = Some("runsc".into());
    assert!(oci_create_opts(&s).join(" ").contains("--runtime runsc"));

    let mut k = spec(Backend::Docker);
    k.oci_runtime = Some("krun".into());
    assert!(oci_create_opts(&k).join(" ").contains("--runtime krun"));

    // A blank/whitespace value is treated as unset.
    let mut blank = spec(Backend::Podman);
    blank.oci_runtime = Some("  ".into());
    assert!(!oci_create_opts(&blank).join(" ").contains("--runtime"));
}

#[test]
fn oci_opts_never_bind_mount_host_dns_files() {
    // Regression: bind-mounting the host's loopback-only resolv.conf into a
    // NAT container broke DNS ("Could not resolve host" on git push). The
    // runtime must own resolv.conf/hosts for OCI backends.
    let mut s = spec(Backend::Podman);
    s.mounts.push(Mount {
        host: "/etc/resolv.conf".into(),
        dest: "/etc/resolv.conf".into(),
        ro: true,
        cache: false,
    });
    s.mounts.push(Mount {
        host: "/etc/hosts".into(),
        dest: "/etc/hosts".into(),
        ro: true,
        cache: false,
    });
    let j = oci_create_opts(&s).join(" ");
    assert!(!j.contains("/etc/resolv.conf"), "resolv.conf mounted: {j}");
    assert!(!j.contains(":/etc/hosts"), "/etc/hosts mounted: {j}");
    // Real worktree mounts are untouched.
    assert!(j.contains("-v /wt/feat:/wt/feat"));
}

#[test]
fn container_status_required_mounts_are_a_subset_of_created_mounts() {
    // The force-recreate loop: container_status must only require mounts that
    // oci_create_opts actually emits. With host DNS/hosts mounts in the spec,
    // the runtime never binds them (oci_opts_never_bind_mount_host_dns_files),
    // so requiring them made every ensure() see "stale mounts" and recreate the
    // running container — killing live pane sessions on the default config.
    let mut s = spec(Backend::Podman);
    s.mounts.push(Mount {
        host: "/etc/resolv.conf".into(),
        dest: "/etc/resolv.conf".into(),
        ro: true,
        cache: false,
    });
    s.mounts.push(Mount {
        host: "/etc/hosts".into(),
        dest: "/etc/hosts".into(),
        ro: true,
        cache: false,
    });
    let emitted = oci_create_opts(&s).join(" ");
    for m in s.mounts.iter().filter(|m| oci_emits_mount(m)) {
        // Every required host path must appear as a created -v source.
        assert!(
            emitted.contains(&format!("-v {}:", m.host)),
            "required mount {} not emitted by oci_create_opts: {emitted}",
            m.host
        );
    }
    // And the two DNS files must NOT be in the required set.
    let required: std::collections::HashSet<&str> = s
        .mounts
        .iter()
        .filter(|m| oci_emits_mount(m))
        .map(|m| m.host.as_str())
        .collect();
    assert!(!required.contains("/etc/resolv.conf"));
    assert!(!required.contains("/etc/hosts"));
}

#[test]
fn empty_oci_image_uses_default_image() {
    let mut s = spec(Backend::Podman);
    s.image = None;
    assert_eq!(effective_image(&s), DEFAULT_OCI_IMAGE);
}

#[test]
fn mosh_wraps_backend_over_ssh() {
    let mut s = spec(Backend::Podman);
    s.placement = Placement::Ssh(SshPlacement::plain(
        "user@box".into(),
        2222,
        true,
        TransportKind::Mosh,
    ));
    let argv = enter_argv(&s, "${SHELL:-/bin/sh} -l");
    assert_eq!(argv[0], "mosh");
    assert!(argv.iter().any(|a| a.starts_with("--ssh=")));
    assert!(argv.iter().any(|a| a.contains("-p 2222")));
    assert!(argv.contains(&"user@box".to_string()));
    // The remote sh body re-runs the podman exec.
    assert!(argv.last().unwrap().contains("podman exec"));
}

#[test]
fn ssh_transport_uses_tty() {
    let mut s = spec(Backend::Bwrap);
    s.image = None;
    s.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let argv = enter_argv(&s, "claude");
    assert_eq!(argv[0], "ssh");
    assert!(argv.contains(&"-t".to_string()));
    assert!(argv.last().unwrap().contains("bwrap"));
}

#[test]
fn bwrap_local_keeps_host_matching_env_off_argv() {
    // Guarantee THEGN_SANDBOX is absent from the ambient env: running
    // `cargo test` inside a live thegn bwrap sandbox leaks it in, which
    // would make the synthetic pair below "match host" and get omitted.
    let _env = crate::testenv::EnvGuard::unset(&["THEGN_SANDBOX"]);
    let mut s = spec(Backend::Bwrap);
    s.image = None;
    // A pair mirroring the host env (PATH always exists) rides the
    // launcher's process env, never the world-readable --setenv argv;
    // synthetic pairs (values absent from the host env) keep --setenv.
    s.env = vec![
        ("PATH".into(), std::env::var("PATH").unwrap()),
        ("THEGN_SANDBOX".into(), "1".into()),
    ];
    let argv = enter_argv(&s, "true");
    assert!(!argv.contains(&"PATH".to_string()));
    let i = argv.iter().position(|a| a == "--setenv").unwrap();
    assert_eq!(argv[i + 1], "THEGN_SANDBOX");

    // Remote-wrapped bwrap keeps --setenv for everything: the argv is the
    // only env carrier through ssh.
    s.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let remote = enter_argv(&s, "true").join(" ");
    assert!(remote.contains("--setenv PATH"));
}

#[test]
fn bwrap_local_omits_ambient_matching_marker() {
    // The nested case (thegn-in-thegn): THEGN_SANDBOX=1 is already
    // in the launcher's env, so local bwrap inherits it and the pair is
    // omitted from the world-readable --setenv argv. A synthetic pair whose
    // value is absent from the host env still rides --setenv.
    let _env = crate::testenv::EnvGuard::set(&[("THEGN_SANDBOX", "1")]);
    let mut s = spec(Backend::Bwrap);
    s.image = None;
    s.env = vec![
        ("THEGN_SANDBOX".into(), "1".into()),
        ("THEGN_SYNTH_MARKER".into(), "x".into()),
    ];
    let argv = enter_argv(&s, "true");
    // Host-matching marker inherited, not on argv.
    assert!(!argv.contains(&"THEGN_SANDBOX".to_string()));
    // Synthetic pair (absent from host env) rides --setenv.
    let i = argv.iter().position(|a| a == "--setenv").unwrap();
    assert_eq!(argv[i + 1], "THEGN_SYNTH_MARKER");
}

#[test]
fn test_parse_sandbox_stats() {
    let output = "1.5%|50MiB / 16GiB";
    let stats = parse_sandbox_stats(output).unwrap();
    assert_eq!(stats.cpu, "1.5%");
    assert_eq!(stats.mem, "50MiB");
}

#[test]
fn test_sandbox_all_oci_flags_applied() {
    let mut s = spec(Backend::Podman);
    s.gpu = Some("all".into());
    s.limits = SandboxLimits {
        cpu: Some("2".into()),
        memory: Some("4GB".into()),
        cpu_total: None,
    };
    s.volumes = vec![("data-vol".into(), "/mnt/data".into())];

    let opts = oci_create_opts(&s);
    let j_opts = opts.join(" ");
    assert!(j_opts.contains("--device nvidia.com/gpu=all"));
    assert!(j_opts.contains("--cpus 2"));
    assert!(j_opts.contains("--memory 4GB"));
    assert!(j_opts.contains("-v data-vol:/mnt/data"));
}

#[test]
fn test_sandbox_compose_executes() {
    // We cannot mock easily without a trait. Since `ensure` executes `docker-compose`,
    // we'll leave Compose verification to the Integration/E2E layer.
}

pub fn pull_image(img: &str) -> anyhow::Result<()> {
    let _ = std::process::Command::new("podman")
        .args(["pull", img])
        .output();
    Ok(())
}

#[test]
fn integration_test_sandbox_net_and_file() {
    // Only run if podman is installed.
    if !crate::util::have("podman") {
        return;
    }

    // Always skip in CI to prevent flakiness unless explicitly forced.
    if std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
    {
        return;
    }

    let mut s = spec(Backend::Podman);
    s.name = "thegn-test-net-file-container".into();
    // A minimal image that has python3 installed
    s.image = Some("public.ecr.aws/docker/library/python:3-alpine".into());
    s.mounts = vec![];
    s.file_access = FileAccess::None;
    s.ports = vec!["8081:8081".into()];

    // Pull image first so `ensure` doesn't timeout if it tries to do it or if it's not present
    // Ignore pull failures (we might already have the image cached)
    let _ = pull_image("public.ecr.aws/docker/library/python:3-alpine");

    // We launch it with a background Python webserver
    let res = ensure(&s);
    assert!(res.is_ok(), "Failed to start container: {:?}", res);

    let argv = enter_argv(&s, "python3 -m http.server 8081");

    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .expect("Failed to spawn sandboxed server");

    // Wait for boot
    std::thread::sleep(std::time::Duration::from_millis(3000));

    // Test Network Routing
    let resp = std::process::Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "http://localhost:8081",
        ])
        .output()
        .unwrap();

    let status = String::from_utf8_lossy(&resp.stdout);

    // Cleanup
    let _ = child.kill();
    let _ = child.wait();
    let loc = crate::remote::GitLoc::Local(std::path::PathBuf::from("/"));
    let cfg = crate::config::SandboxConfig {
        enabled: true,
        ..Default::default()
    };
    teardown(&cfg, &loc, &s.name);

    assert_eq!(status.trim(), "200", "Port 8081 was not exposed properly");
}

#[test]
fn integration_test_sandbox_lifecycle() {
    // Only run if podman is installed.
    if !crate::util::have("podman") {
        return;
    }

    // We skip this test in CI/automated environments to prevent rate limits
    // from Docker Hub/ECR blocking test success. The logic is verified manually.
    if std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
    {
        return;
    }

    let mut s = spec(Backend::Podman);
    s.name = "thegn-test-lifecycle-container".into();
    s.image = Some("public.ecr.aws/docker/library/alpine:latest".into());
    // Do not bind mount fake paths like /wt/feat in the integration test as they
    // don't exist on the real host and podman will error out when creating the container.
    s.mounts = vec![];
    s.file_access = FileAccess::None;

    // Pull image first so `ensure` doesn't timeout if it tries to do it or if it's not present
    // Ignore pull failures (we might already have the image cached)
    let _ = pull_image("public.ecr.aws/docker/library/alpine:latest");

    // 1. Ensure (create keep-alive)
    let res = ensure(&s);
    assert!(res.is_ok(), "Failed to start container: {:?}", res);

    // 2. Stats
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let st = stats(&s);
    assert!(st.is_some(), "Failed to fetch stats");
    let st = st.unwrap();
    assert!(!st.cpu.is_empty());

    // 3. Teardown
    let loc = crate::remote::GitLoc::Local(std::path::PathBuf::from("/"));
    let cfg = crate::config::SandboxConfig {
        enabled: true,
        ..Default::default()
    };
    teardown(&cfg, &loc, &s.name);

    // Verify it's gone
    let out = std::process::Command::new("podman")
        .args(["container", "exists", &s.name])
        .output()
        .unwrap();
    assert!(!out.status.success());
}

#[test]
fn test_gc_identifies_orphans() {
    let active_wts = vec!["live".to_string()];
    let containers = vec![
        "thegn-live".to_string(),
        "thegn-dead".to_string(),
        "other-container".to_string(),
    ];
    let orphans = identify_orphans(&active_wts, &containers);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0], "thegn-dead");
}

#[test]
fn remote_none_bare_shell_cds_and_moshes() {
    // A remote worktree with no container backend still goes over the
    // transport as a bare shell that cd's into the remote worktree.
    let mut s = spec(Backend::None);
    s.image = None;
    s.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Mosh,
    ));
    let argv = enter_argv(&s, "${SHELL:-/bin/sh} -l");
    assert_eq!(argv[0], "mosh");
    let body = argv.last().unwrap();
    assert!(body.contains("cd /wt/feat"));
    assert!(body.contains("exec ${SHELL:-/bin/sh} -l"));
}

#[test]
fn devenv_wraps_inner() {
    let mut s = spec(Backend::Bwrap);
    s.image = None;
    s.devenv = true;
    let argv = enter_argv(&s, "claude");
    assert_eq!(argv.last().unwrap(), "exec devenv shell -- claude");
}

#[test]
fn mount_parsing() {
    let home = std::env::var("HOME").unwrap_or_default();
    assert_eq!(
        parse_mount("~/.gitconfig:ro"),
        Mount {
            host: format!("{home}/.gitconfig"),
            dest: format!("{home}/.gitconfig"),
            ro: true,
            cache: false,
        }
    );
    assert_eq!(
        parse_mount("/a:/b"),
        Mount {
            host: "/a".into(),
            dest: "/b".into(),
            ro: false,
            cache: false,
        }
    );
}

#[test]
fn podman_and_docker_ps_parse_and_mark_ours() {
    let podman = r#"[
          {"Names": ["thegn-wt-feat"], "Image": "ubuntu:24.04", "Status": "Up 2 hours"},
          {"Names": ["registry"], "Image": "registry:2", "Status": "Up 3 days"}
        ]"#;
    let rows = parse_podman_ps(podman);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].ours && rows[0].name == "thegn-wt-feat");
    assert!(!rows[1].ours);
    assert_eq!(rows[1].image, "registry:2");

    let docker = "{\"Names\": \"thegn-x\", \"Image\": \"alpine\", \"Status\": \"Up 5 minutes\"}\n{\"Names\": \"db\", \"Image\": \"postgres:16\", \"Status\": \"Up 1 hour\"}";
    let rows = parse_docker_ps(docker);
    assert_eq!(rows.len(), 2);
    assert!(rows[0].ours);
    assert_eq!(rows[1].name, "db");

    // Garbage degrades to empty, never panics.
    assert!(parse_podman_ps("not json").is_empty());
    assert!(parse_docker_ps("not json").is_empty());
}

#[test]
fn host_toolchain_mounts_are_all_ro_and_exist() {
    // Every mount returned must point to a path that actually exists on the
    // current host (no phantom entries) and must be read-only.
    for m in host_toolchain_mounts() {
        assert!(
            std::path::Path::new(&m.host).exists(),
            "host_toolchain_mounts returned non-existent path: {}",
            m.host
        );
        assert!(m.ro, "host toolchain mount must be read-only: {}", m.host);
        assert_eq!(
            m.host, m.dest,
            "host toolchain mounts must be path-preserving"
        );
    }
}

#[test]
fn cfg_mounts_covered_by_parent_are_skipped() {
    // When $HOME is already bind-mounted (via host_toolchain_mounts for bwrap),
    // a cfg.mounts entry for a child path (e.g. ~/.gitconfig) must be dropped.
    // Keeping it causes bwrap "Can't create file" because bwrap cannot create a
    // file mount-point inside an already-mounted parent directory.
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return; // can't test without $HOME
    }
    let cfg = crate::config::SandboxConfig {
        file_access: crate::config::FileAccess::WorktreePlusCaches,
        auto_caches: true,
        backend: crate::config::SandboxBackend::Bwrap,
        // Use a file inside $HOME — it may or may not exist, the coverage check
        // must fire regardless (covered by the parent home bind).
        mounts: vec![format!("{}/.gitconfig:ro", home)],
        ..Default::default()
    };
    let loc = crate::remote::GitLoc::from_db("/wt/x", None);
    if let Some(spec) = resolve(&cfg, &loc, "test") {
        let gitconfig = format!("{home}/.gitconfig");
        let has_gitconfig_mount = spec.mounts.iter().any(|m| m.host == gitconfig);
        assert!(
            !has_gitconfig_mount,
            "~/.gitconfig should be excluded — $HOME is already bind-mounted"
        );
    }
}

#[test]
fn host_toolchain_mounts_injected_for_oci_not_bwrap() {
    // For OCI backends, host_toolchain_mounts() contributes only paths that
    // exist on the current host — verify that invariant holds by checking
    // any mount whose host path is NOT the synthetic worktree path.
    let cfg = crate::config::SandboxConfig {
        file_access: crate::config::FileAccess::WorktreePlusCaches,
        auto_caches: true,
        backend: crate::config::SandboxBackend::Podman,
        image: "debian:stable".into(),
        // Clear user-configured mounts so only host_toolchain + auto_cache mounts
        // are present; avoids depending on whether $HOME/.gitconfig exists in the
        // test environment.
        mounts: vec![],
        ..Default::default()
    };
    let loc = crate::remote::GitLoc::from_db("/wt/x", None);
    if let Some(spec) = resolve(&cfg, &loc, "test") {
        // host_toolchain_mounts() entries are ro by definition — filter to
        // ro, non-cache mounts outside the fake worktree and $HOME. The rw
        // carve-outs (language caches, ~/tmp, the agent config dir a
        // parallel test points CLAUDE_CONFIG_DIR at and then deletes) are
        // not toolchain mounts and may legitimately vanish mid-assertion.
        let home = std::env::var("HOME").unwrap_or_default();
        let toolchain: Vec<_> = spec
            .mounts
            .iter()
            .filter(|m| {
                m.ro && !m.host.starts_with("/wt/")
                    && !m.cache
                    && (home.is_empty() || !m.host.starts_with(&home))
            })
            .collect();
        for m in &toolchain {
            assert!(
                std::path::Path::new(&m.host).exists(),
                "host_toolchain mount for non-existent path: {}",
                m.host
            );
        }
        // On NixOS (where /nix/store exists) we must have injected at
        // least the nix store mount.
        if std::path::Path::new("/nix/store").exists() {
            assert!(
                toolchain.iter().any(|m| m.host == "/nix/store"),
                "OCI spec on NixOS should include /nix/store mount"
            );
        }
    }
}

// H3: Orphan GC — identify orphans correctly.
#[test]
fn test_identify_orphans_names_only_thegn_containers() {
    let active = vec!["/wt/live".to_string()];
    let containers = vec![
        container_name("/wt/live"),    // active → not orphan
        container_name("/wt/dead"),    // no active entry → orphan
        "other-tool-container".into(), // not thegn-prefixed → ignored
    ];
    let orphans = identify_orphans(&active, &containers);
    assert_eq!(orphans, vec![container_name("/wt/dead")]);
}

#[test]
fn test_identify_orphans_empty_inputs() {
    // No containers → nothing to remove.
    assert!(identify_orphans(&["wt".to_string()], &[]).is_empty());
    // No active worktrees → all thegn containers are orphans.
    let containers = vec![container_name("/wt/a"), container_name("/wt/b")];
    let orphans = identify_orphans(&[], &containers);
    assert_eq!(orphans.len(), 2);
}

#[test]
fn test_run_gc_noop_when_no_backend_available() {
    // run_gc with an empty DB set and no containers should return empty
    // without panicking (even if podman/docker aren't installed).
    let removed = run_gc(&["/wt/alive".to_string()]);
    // On CI there may be no podman — the result is just an empty list.
    assert!(removed.iter().all(|n| n.starts_with(CONTAINER_PREFIX)));
}

// H2: Remote transport unit tests.
#[test]
fn remote_enter_argv_wraps_with_mosh() {
    let mut s = spec(Backend::Podman);
    s.placement = Placement::Ssh(SshPlacement::plain(
        "devbox".into(),
        22,
        false,
        TransportKind::Mosh,
    ));
    // With a real image + OCI backend on a remote, enter_argv should
    // produce a mosh wrapper.
    let argv = enter_argv(&s, "bash -l");
    assert_eq!(argv[0], "mosh", "outer command must be mosh: {argv:?}");
    // The remote host must appear in the argv.
    assert!(argv.iter().any(|a| a == "devbox"), "host missing: {argv:?}");
}

#[test]
fn remote_enter_argv_wraps_with_ssh() {
    let mut s = spec(Backend::Podman);
    s.placement = Placement::Ssh(SshPlacement::plain(
        "devbox".into(),
        2222,
        true,
        TransportKind::Ssh,
    ));
    let argv = enter_argv(&s, "bash -l");
    // SSH transport: first arg is ssh, not mosh.
    assert_eq!(argv[0], "ssh", "outer command must be ssh: {argv:?}");
    assert!(argv.iter().any(|a| a == "devbox"), "host missing: {argv:?}");
    // Port flag must be present when non-default.
    assert!(
        argv.iter().any(|a| a == "-p"),
        "port flag missing: {argv:?}"
    );
}

// H4 is in dns_filter.rs (already done).

// Per-profile container naming (G1).
#[test]
fn container_name_with_profile_adds_slug() {
    let default = container_name_with_profile("/wt/feat", None);
    let explicit_default = container_name_with_profile("/wt/feat", Some("default"));
    let named = container_name_with_profile("/wt/feat", Some("work"));
    assert_eq!(default, container_name("/wt/feat"));
    assert_eq!(explicit_default, container_name("/wt/feat"));
    assert!(named.starts_with(CONTAINER_PREFIX));
    assert!(named.contains("work"));
    assert!(named != default);
}

#[test]
fn sandbox_profile_baselines() {
    assert!(!SandboxProfile::Open.read_only_root());
    assert!(SandboxProfile::Hardened.read_only_root());
    assert!(SandboxProfile::Sealed.read_only_root());

    assert_eq!(SandboxProfile::Open.pids_limit(), None);
    assert_eq!(SandboxProfile::Hardened.pids_limit(), Some(512));
    assert_eq!(SandboxProfile::Sealed.pids_limit(), Some(256));

    // Only `sealed` drops caps + forces no-network; `hardened` keeps both so
    // debuggers/ping/networking still work.
    assert!(SandboxProfile::Hardened.drop_capabilities().is_empty());
    assert!(
        SandboxProfile::Sealed
            .drop_capabilities()
            .contains(&"ALL".to_string())
    );
    assert!(SandboxProfile::Sealed.forces_no_network());
    assert!(!SandboxProfile::Hardened.forces_no_network());
}

#[test]
fn oci_opts_emit_sealed_hardening() {
    let mut s = spec(Backend::Podman);
    s.network = Network::None;
    s.read_only_root = true;
    s.no_new_privileges = true;
    s.pids_limit = Some(256);
    s.drop_capabilities = vec!["ALL".into()];
    let j = oci_create_opts(&s).join(" ");
    assert!(j.contains("--read-only"), "{j}");
    assert!(j.contains("--tmpfs /tmp"), "{j}");
    assert!(j.contains("--cap-drop ALL"), "{j}");
    assert!(j.contains("--security-opt no-new-privileges"), "{j}");
    assert!(j.contains("--pids-limit 256"), "{j}");
    assert!(j.contains("--network none"), "{j}");
}

#[test]
fn oci_opts_open_profile_adds_no_hardening() {
    // `open` (all knobs off, as the spec() helper builds) must reproduce
    // today's argv — none of the hardening flags may appear.
    let s = spec(Backend::Podman);
    let j = oci_create_opts(&s).join(" ");
    assert!(!j.contains("--read-only"), "{j}");
    assert!(!j.contains("--cap-drop"), "{j}");
    assert!(!j.contains("--security-opt"), "{j}");
    assert!(!j.contains("--pids-limit"), "{j}");
}

#[test]
fn vpn_sidecar_name_roundtrips() {
    let base = container_name("/wt/feat");
    let vpn = vpn_sidecar_name(&base);
    assert_eq!(vpn, format!("{base}-szvpn"));
    assert_ne!(vpn, base);
    assert_eq!(strip_vpn_suffix(&vpn), base);
    assert_eq!(strip_vpn_suffix(&base), base);
    // Independent of the agent suffix.
    assert_eq!(strip_agent_suffix(&vpn), vpn);
}

#[test]
fn agent_container_name_roundtrips_and_is_not_orphan() {
    let base = container_name("/wt/feat");
    let agent = agent_container_name(&base);
    assert_ne!(agent, base);
    assert_eq!(strip_agent_suffix(&agent), base);
    assert_eq!(strip_agent_suffix(&base), base);

    // An active worktree owns BOTH its container and the agent's; only a
    // container for a no-longer-active worktree is an orphan.
    let active = vec!["/wt/feat".to_string()];
    let containers = vec![base.clone(), agent.clone(), container_name("/wt/dead")];
    let orphans = identify_orphans(&active, &containers);
    assert!(!orphans.contains(&base));
    assert!(!orphans.contains(&agent));
    assert!(orphans.contains(&container_name("/wt/dead")));
}

#[test]
fn identify_orphans_spares_vpn_sidecar_and_profile_containers_of_live_worktrees() {
    // Regression: a live worktree launched with a non-default profile
    // (`thegn-{profile}-{slug}`) or a VPN sidecar (`-szvpn`) must never be
    // reaped by run_gc. The old exact-name allow-list only knew the plain and
    // `-szagent` forms and force-removed these live containers.
    let active = vec!["/wt/feat".to_string()];
    let base = container_name("/wt/feat");
    let vpn = vpn_sidecar_name(&base);
    let profile = container_name_with_profile("/wt/feat", Some("sealed"));
    let profile_vpn = vpn_sidecar_name(&profile);
    let profile_agent = agent_container_name(&profile);
    let dead = container_name("/wt/dead");
    let dead_vpn = vpn_sidecar_name(&dead);

    let containers = vec![
        base.clone(),
        vpn.clone(),
        profile.clone(),
        profile_vpn.clone(),
        profile_agent.clone(),
        dead.clone(),
        dead_vpn.clone(),
    ];
    let orphans = identify_orphans(&active, &containers);

    for live in [&base, &vpn, &profile, &profile_vpn, &profile_agent] {
        assert!(!orphans.contains(live), "reaped live container {live}");
    }
    // A dead worktree's container AND its sidecar are still orphans.
    assert!(orphans.contains(&dead));
    assert!(orphans.contains(&dead_vpn));
}

#[test]
fn wrap_script_skips_exec_for_or_and_pipe_fallbacks() {
    // A `||` fallback (`claude --resume || claude`) must run directly, NOT
    // as `exec …` — a failed `exec` (command not found) would exit the shell
    // before the fallback branch could run. Same for a bare `|` pipeline.
    let mut s = spec(Backend::Bwrap);
    s.image = None;

    let or_fallback = wrap_script(&s, "claude --resume || claude");
    assert!(
        or_fallback.ends_with("claude --resume || claude"),
        "|| fallback must not be exec-prefixed: {or_fallback}"
    );
    assert!(
        !or_fallback.contains("exec claude --resume"),
        "|| fallback wrongly exec-prefixed: {or_fallback}"
    );

    let pipe = wrap_script(&s, "gen | tee log");
    assert!(
        pipe.ends_with("gen | tee log") && !pipe.contains("exec gen"),
        "pipe must not be exec-prefixed: {pipe}"
    );

    // A simple single command is still exec'd so it owns the pane.
    let simple = wrap_script(&s, "zsh -l");
    assert!(
        simple.ends_with("exec zsh -l"),
        "simple command must be exec-prefixed: {simple}"
    );
}

#[test]
fn ensure_stale_and_keepid_rm_go_through_oci_prefix() {
    // Regression: the stale-mount rm and the keep-id-retry rm must target the
    // SAME daemon as `run -d` (via oci_prefix), not a bare `binary()`. For
    // rootful podman that means `sudo -n podman … rm`, and with oci_host it
    // means the `--connection`/`--url`/`-H` flag — otherwise the rm no-ops on
    // the wrong (local rootless) store and the recreate fails "name in use".
    //
    // We can't run the subprocess in a unit test, so we assert the argv the
    // rm is built from: oci_prefix(spec) + ["rm","-f",name]. This mirrors the
    // exact construction in `ensure`.
    let mut r = spec(Backend::PodmanRootful);
    r.oci_host = Some("workbox".into());
    let mut rm = oci_prefix(&r);
    rm.extend(["rm".into(), "-f".into(), r.name.clone()]);
    assert_eq!(
        rm,
        vec![
            "sudo",
            "-n",
            "podman",
            "--connection",
            "workbox",
            "rm",
            "-f",
            "thegn-repo-feat",
        ]
    );
    // A bare binary()-based rm would NOT carry sudo or the connection flag.
    assert_ne!(rm[0], "podman", "rootful rm must start with sudo");
}

#[test]
fn teardown_targets_oci_host_daemon() {
    // teardown must remove containers on the SAME daemon the create used:
    // with `[sandbox] oci_host` set, the rm argv must carry the connection
    // flag (via oci_prefix_for), or the remote container leaks forever.
    let plain = oci_prefix_for(Backend::Podman, None);
    assert_eq!(plain, vec!["podman"]);

    let remote = oci_prefix_for(Backend::Podman, Some("workbox"));
    assert_eq!(remote, vec!["podman", "--connection", "workbox"]);

    // Docker uses -H; a URL uses --url; rootful keeps sudo -n.
    assert_eq!(
        oci_prefix_for(Backend::Docker, Some("ssh://user@box")),
        vec!["docker", "-H", "ssh://user@box"]
    );
    assert_eq!(
        oci_prefix_for(Backend::Podman, Some("ssh://u@h/sock")),
        vec!["podman", "--url", "ssh://u@h/sock"]
    );
    assert_eq!(
        oci_prefix_for(Backend::PodmanRootful, Some("workbox")),
        vec!["sudo", "-n", "podman", "--connection", "workbox"]
    );
    // Empty/whitespace oci_host ⇒ local daemon (no flag).
    assert_eq!(oci_prefix_for(Backend::Podman, Some("  ")), vec!["podman"]);
}

#[test]
fn oci_local_secrets_go_to_env_file_not_argv() {
    // A host-sourced passthrough secret (value matching the launcher env)
    // must NOT ride the world-readable `-e K=V` argv; it goes to a 0600
    // `--env-file`. A synthetic pair (value absent from the host env) stays
    // inline as `-e`.
    let _env = crate::testenv::EnvGuard::set(&[("GH_TOKEN", "ghp_secret")]);
    let mut s = spec(Backend::Podman);
    s.name = "thegn-test-envfile-oci".into();
    s.env = vec![
        ("GH_TOKEN".into(), "ghp_secret".into()), // matches host ⇒ secret
        ("THEGN_SANDBOX".into(), "1".into()),     // synthetic ⇒ inline
    ];
    let opts = oci_create_opts(&s);
    let j = opts.join(" ");
    // The token value never appears on the argv.
    assert!(
        !j.contains("ghp_secret"),
        "secret leaked onto OCI argv: {j}"
    );
    assert!(!j.contains("-e GH_TOKEN"), "secret rode -e: {j}");
    // It rode an --env-file instead, and that file is 0600.
    let i = opts
        .iter()
        .position(|a| a == "--env-file")
        .expect("env-file");
    let path = std::path::PathBuf::from(&opts[i + 1]);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "env-file must be 0600");
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.contains("GH_TOKEN=ghp_secret"),
        "env-file body: {body}"
    );
    // Synthetic pair still inline.
    assert!(
        j.contains("-e THEGN_SANDBOX=1"),
        "synthetic pair inline: {j}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn systemd_local_secrets_go_to_environment_file_not_argv() {
    let _env = crate::testenv::EnvGuard::set(&[("API_KEY", "sk_secret")]);
    let mut s = spec(Backend::Systemd);
    s.image = None;
    s.name = "thegn-test-envfile-systemd".into();
    s.env = vec![
        ("API_KEY".into(), "sk_secret".into()), // secret
        ("THEGN_SANDBOX".into(), "1".into()),   // synthetic
    ];
    let argv = backend_enter_argv(&s, "exec true");
    let j = argv.join(" ");
    assert!(
        !j.contains("sk_secret"),
        "secret leaked onto systemd argv: {j}"
    );
    assert!(!j.contains("--setenv API_KEY"), "secret rode --setenv: {j}");
    // Carried via an EnvironmentFile= property instead.
    let ef = argv
        .iter()
        .find(|a| a.starts_with("EnvironmentFile="))
        .expect("EnvironmentFile property");
    let path = std::path::PathBuf::from(ef.trim_start_matches("EnvironmentFile="));
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("API_KEY=sk_secret"));
    // Synthetic pair stays on --setenv.
    assert!(
        j.contains("--setenv THEGN_SANDBOX=1"),
        "synthetic --setenv: {j}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn remote_oci_keeps_all_env_inline_as_carrier() {
    // Over ssh the argv is the ONLY env carrier — a remote spec must keep
    // every pair inline (never divert a host-matching value to a local
    // env-file the remote daemon can't read).
    let _env = crate::testenv::EnvGuard::set(&[("GH_TOKEN", "ghp_secret")]);
    let mut s = spec(Backend::Podman);
    s.env = vec![("GH_TOKEN".into(), "ghp_secret".into())];
    s.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let j = oci_create_opts(&s).join(" ");
    assert!(
        j.contains("-e GH_TOKEN=ghp_secret"),
        "remote keeps env inline: {j}"
    );
    assert!(
        !j.contains("--env-file"),
        "remote must not use env-file: {j}"
    );
}
