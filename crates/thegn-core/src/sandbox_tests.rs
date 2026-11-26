use super::*;
use crate::sandbox_mounts::{host_toolchain_mounts, parse_mount};

#[cfg(unix)]
fn private_tempdir() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;

    tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap()
}

#[test]
fn ambient_container_listing_runs_ps_only_for_present_backends() {
    let mut calls = Vec::new();
    let containers = running_containers_with(
        false,
        |backend| match backend {
            Backend::Podman | Backend::PodmanRootful => RuntimeProbe::Absent,
            Backend::Docker => RuntimeProbe::Present,
            _ => unreachable!(),
        },
        |backend, _| {
            calls.push(backend);
            Some(r#"{"Names":"foreign","Image":"alpine","Status":"Up 2 seconds"}"#.into())
        },
        |_| std::collections::HashMap::new(),
    );
    assert_eq!(calls, vec![Backend::Docker]);
    assert_eq!(containers.len(), 1);
    assert_eq!(containers[0].backend, "docker");
}

#[test]
fn ambient_container_listing_with_no_present_backend_is_empty_without_ps() {
    let mut calls = Vec::new();
    let containers = running_containers_with(
        false,
        |_| RuntimeProbe::Absent,
        |backend, _| {
            calls.push(backend);
            None
        },
        |_| std::collections::HashMap::new(),
    );
    assert!(containers.is_empty());
    assert!(calls.is_empty());
}

#[cfg(unix)]
#[test]
fn output_and_stderr_with_timeout_runs_the_command_once() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("runs");
    let script = format!(
        "printf x >> {}; printf out; printf err >&2; exit 7",
        crate::util::sh_quote(&marker.to_string_lossy())
    );
    let argv = vec!["sh".into(), "-c".into(), script];
    let result = output_and_stderr_with_timeout(&argv, Duration::from_secs(1)).unwrap();
    assert_eq!(result, (false, "out".into(), "err".into()));
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "x");
}

#[cfg(unix)]
#[test]
fn output_with_timeout_flood_is_bounded() {
    let argv = vec!["sh".into(), "-c".into(), "head -c 4000000 /dev/zero".into()];
    let started = std::time::Instant::now();
    assert!(
        output_with_timeout(&argv, Duration::from_secs(2)).is_none(),
        "output beyond the capture cap must not be reported as complete"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "flooding stdout must not block the bounded runner"
    );
}

#[cfg(unix)]
#[test]
fn output_with_timeout_descendant_holding_pipe_is_quiesced() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("stdout-descendant-pid");
    let script = format!(
        "sleep 30 & echo $! > '{}'; printf output",
        pid_file.display()
    );
    let argv = vec!["sh".into(), "-c".into(), script];
    let started = std::time::Instant::now();
    let (success, output) =
        output_with_timeout(&argv, Duration::from_millis(300)).expect("leader output is retained");

    assert!(success);
    assert_eq!(output, "output");
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "stdout-holding descendant {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
#[test]
fn stderr_with_timeout_flood_is_bounded() {
    let argv = vec![
        "sh".into(),
        "-c".into(),
        "head -c 4000000 /dev/zero >&2".into(),
    ];
    let started = std::time::Instant::now();
    assert!(
        stderr_with_timeout(&argv, Duration::from_secs(2)).is_none(),
        "stderr beyond the capture cap must not be reported as complete"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "flooding stderr must not block the bounded runner"
    );
}

#[cfg(unix)]
#[test]
fn stderr_with_timeout_descendant_holding_pipe_is_quiesced() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("stderr-descendant-pid");
    let script = format!(
        "sleep 30 & echo $! > '{}'; printf diagnostic >&2",
        pid_file.display()
    );
    let argv = vec!["sh".into(), "-c".into(), script];
    let started = std::time::Instant::now();
    let (success, stderr) = stderr_with_timeout(&argv, Duration::from_millis(300))
        .expect("leader diagnostics are retained");

    assert!(success);
    assert_eq!(stderr, "diagnostic");
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "stderr-holding descendant {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

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
    let body = backend_enter_argv(&s, "exec sh").unwrap().pop().unwrap();
    assert!(!body.contains("cd "), "no local cd shipped remote: {body}");
    assert!(body.contains("exec sh"));

    // A retargeted remote (a mount whose dest == the worktree path) DOES cd.
    s.mounts = vec![Mount {
        host: "/home/me/wt".into(),
        dest: "/home/me/wt".into(),
        ro: false,
        cache: false,
    }];
    let retargeted = backend_enter_argv(&s, "exec sh").unwrap().pop().unwrap();
    assert!(
        retargeted.starts_with("cd "),
        "retargeted remote cds: {retargeted}"
    );

    // A local `none` still cds into the worktree.
    let mut local = spec(Backend::None);
    local.placement = Placement::Local;
    let body_local = backend_enter_argv(&local, "exec sh")
        .unwrap()
        .pop()
        .unwrap();
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
fn tunnel_only_profiles_refuse_only_when_vpn_is_configured() {
    let cfg = vpn_cfg(VpnProviderKind::Tailscale);
    let tunnel = build_vpn_spec(&cfg, "wt", SandboxProfile::SealedTunnel).unwrap();
    assert!(tunnel.tunnel_only);
    let hardened = build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).unwrap();
    assert!(!hardened.tunnel_only);

    for profile in [
        SandboxProfile::Open,
        SandboxProfile::Hardened,
        SandboxProfile::Sealed,
        SandboxProfile::SealedTunnel,
    ] {
        for vpn_configured in [false, true] {
            let mut sandbox = spec(Backend::Podman);
            sandbox.vpn = if vpn_configured {
                build_vpn_spec(&cfg, "wt", profile)
            } else {
                None
            };
            let result = enter_argv(&sandbox, "sh");
            if profile == SandboxProfile::SealedTunnel && vpn_configured {
                match result {
                    Err(EnterError::Refused(refusal)) => {
                        assert_eq!(refusal.class, RefusalClass::TunnelUnenforced);
                        assert!(refusal.message.contains("sealed-tunnel"));
                        assert!(refusal.message.contains("standard"));
                    }
                    other => panic!("expected tunnel-only refusal, got {other:?}"),
                }
            } else {
                assert!(!matches!(
                    result,
                    Err(EnterError::Refused(refusal))
                        if refusal.class == RefusalClass::TunnelUnenforced
                ));
            }
        }
    }
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
    cfg.dns = VpnDnsMode::FilterOnly;
    cfg.ready_timeout_secs = 7;
    cfg.ephemeral = false;
    let spec = build_vpn_spec(&cfg, "wt", SandboxProfile::Hardened).unwrap();
    assert_eq!(spec.mode, VpnMode::Proxy);
    assert_eq!(spec.on_error, VpnOnError::Offline);
    assert_eq!(spec.dns_mode, VpnDnsMode::FilterOnly);
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
    s.network_allow = vec!["example.com".into()]; // hostname rules do not add runtime DNS flags
    s.ports = vec!["8080:8080".into()]; // would normally add -p
    s.vpn = build_vpn_spec(
        &vpn_cfg(VpnProviderKind::Tailscale),
        &s.name,
        SandboxProfile::Hardened,
    );
    let opts = oci_create_opts(&s).expect("valid volume names");
    let joined = opts.join(" ");
    // Joins the sidecar netns...
    assert!(
        joined.contains("--network container:thegn-repo-feat-tgvpn"),
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
    let opts = oci_create_opts(&s).expect("valid volume names");
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
    let opts = oci_create_opts(&s).expect("valid volume names");
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

#[test]
fn to_config_round_trips_from_config() {
    use crate::seam::Kind as _;
    for kind in SandboxBackend::ALL.iter().copied() {
        match Backend::from_config(kind) {
            Some(backend) => assert_eq!(backend.to_config(), kind, "{kind:?}"),
            None => assert_eq!(kind, SandboxBackend::Auto),
        }
    }
    for backend in Backend::ALL {
        assert_eq!(Backend::from_config(backend.to_config()), Some(backend));
    }
}

#[test]
fn smol_profile_label_and_binary() {
    assert_eq!(Backend::Smol.label(), "smol");
    assert_eq!(Backend::Smol.binary(), "smolvm");
    assert!(matches!(
        Backend::Smol.profile().events,
        EventsCap::Reserved(reason) if reason.contains("reserved")
    ));
}

#[test]
fn gc_list_argv_none_for_smol() {
    assert_eq!(gc_list_argv(Backend::Smol), None);
    assert!(gc_list_argv(Backend::Docker).is_some());
}

#[test]
fn smol_is_never_a_docker_clone_in_create_opts() {
    // No `--user` and no `--gpus`: the docker-clone arms are gone.
    let mut s = spec(Backend::Smol);
    s.gpu = Some("all".into());
    let opts = oci_create_opts(&s).expect("valid volume names");
    assert!(!opts.iter().any(|arg| arg == "--gpus"), "{opts:?}");
    assert!(!opts.iter().any(|arg| arg == "--user"), "{opts:?}");
}

#[test]
fn oci_runtimes_excludes_reserved() {
    let runtimes: Vec<Backend> = Backend::oci_runtimes().collect();
    assert!(
        runtimes.iter().all(|backend| !backend.is_reserved()),
        "{runtimes:?}"
    );
    assert!(!runtimes.contains(&Backend::Smol));
    assert!(!runtimes.contains(&Backend::Wsl));
    assert!(runtimes.contains(&Backend::Podman));
    let reserved: Vec<Backend> = Backend::ALL
        .into_iter()
        .filter(|backend| backend.is_reserved())
        .collect();
    assert_eq!(
        reserved,
        vec![Backend::Smol, Backend::Wsl, Backend::WinAppContainer]
    );
}

fn spec(backend: Backend) -> SandboxSpec {
    SandboxSpec {
        backend,
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
        ports: vec!["8080:8080".into()],
        // Disable CPU capping so argv-shape assertions are host-independent
        // (the aggregate cap is on-by-default and would scope-wrap on a host
        // with cgroup delegation); capping itself is tested in sandbox_cpucap.
        limits: SandboxLimits {
            cpu_total: Some("off".into()),
            ..SandboxLimits::default()
        },
        file_access: FileAccess::Worktree,
        name: "thegn-repo-feat".into(),
        ..SandboxSpec::default()
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
fn compose_target_selects_runtime_connection_and_placement() {
    use crate::sandbox_compose::{ComposeSpec, ComposeTarget, ps_argv, up_argv};

    let compose = ComposeSpec {
        files: vec!["compose.yml".into()],
        service: Some("app".into()),
        run_services: Vec::new(),
    };
    let mut docker = spec(Backend::Docker);
    let target = ComposeTarget::for_spec(&docker).unwrap();
    assert_eq!(
        up_argv(&target, "proj", &compose),
        [
            "docker",
            "compose",
            "-f",
            "compose.yml",
            "-p",
            "proj",
            "up",
            "-d",
            "app"
        ]
    );
    docker.oci_host = Some("tcp://h".into());
    let target = ComposeTarget::for_spec(&docker).unwrap();
    assert_eq!(
        up_argv(&target, "proj", &compose)[..4],
        ["docker", "-H", "tcp://h", "compose"]
    );

    let mut podman = spec(Backend::Podman);
    podman.oci_host = Some("ssh://u@h/run/podman.sock".into());
    let target = ComposeTarget::for_spec(&podman).unwrap();
    assert_eq!(
        up_argv(&target, "proj", &compose)[..4],
        ["podman", "--url", "ssh://u@h/run/podman.sock", "compose"]
    );
    let rootful = ComposeTarget::for_spec(&spec(Backend::PodmanRootful)).unwrap();
    assert_eq!(
        up_argv(&rootful, "proj", &compose)[..4],
        ["sudo", "-n", "podman", "compose"]
    );

    for backend in [Backend::Apple, Backend::Smol, Backend::Wsl] {
        assert!(matches!(
            ComposeTarget::for_spec(&spec(backend)),
            Err(crate::sandbox_compose::ComposeError::Unsupported { .. })
        ));
    }

    let mut remote = spec(Backend::Docker);
    remote.placement = Placement::Ssh(SshPlacement::plain(
        "host".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let target = ComposeTarget::for_spec(&remote).unwrap();
    let local_up = up_argv(&target, &remote.name, &compose);
    let local_ps = ps_argv(&target, &remote.name, &compose);
    assert_eq!(
        target.control_argv(&local_up),
        remote.placement.control_argv(&local_up)
    );
    assert_eq!(
        target.control_argv(&local_ps),
        remote.placement.control_argv(&local_ps)
    );
    let routed_up = target.control_argv(&local_up);
    assert!(routed_up.first().is_some_and(|arg| arg == "ssh"));
    assert!(
        routed_up
            .last()
            .is_some_and(|arg| arg.contains("docker compose"))
    );
}

#[test]
fn compose_enter_refuses_unsupported_apple_backend() {
    let mut s = spec(Backend::Apple);
    s.compose = Some(
        crate::sandbox_compose::ComposeSpec {
            files: vec!["compose.yml".into()],
            service: Some("app".into()),
            run_services: Vec::new(),
        }
        .encode(),
    );
    match enter_argv(&s, "true") {
        Err(EnterError::Refused(refusal)) => {
            assert_eq!(refusal.class, RefusalClass::ComposeTarget);
            assert_eq!(refusal.backend, "apple");
            assert!(refusal.path.is_none());
            assert!(refusal.message.contains("unsupported for backend 'apple'"));
            assert!(
                enter_argv(&s, "true")
                    .unwrap_err()
                    .to_string()
                    .contains("unsupported for backend 'apple'")
            );
        }
        other => panic!("expected unsupported Compose refusal, got {other:?}"),
    }
}

#[test]
fn sealed_home_refusal_is_structured_and_keeps_its_display() {
    let mut s = spec(Backend::Bwrap);
    s.seal_home = Some("relative-home".into());
    let error = enter_argv(&s, "true").expect_err("relative sealed HOME cannot be hidden");
    match &error {
        EnterError::Refused(refusal) => {
            assert_eq!(refusal.class, RefusalClass::SealedHome);
            assert_eq!(refusal.backend, "bwrap");
            assert!(refusal.path.is_none());
            assert_eq!(error.to_string(), refusal.message);
        }
        other => panic!("expected sealed-home refusal, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn required_mount_symlink_loop_is_refused() {
    use std::os::unix::fs::symlink;

    let td = private_tempdir();
    let a = td.path().join("a");
    let b = td.path().join("b");
    symlink(&b, &a).unwrap();
    symlink(&a, &b).unwrap();
    let path = a.to_string_lossy().into_owned();
    let mut s = spec(Backend::Bwrap);
    s.mounts = vec![Mount {
        host: path.clone(),
        dest: path.clone(),
        ro: false,
        cache: false,
    }];

    match enter_argv(&s, "true") {
        Err(EnterError::Refused(refusal)) => {
            assert_eq!(refusal.class, RefusalClass::MountUnresolvable);
            assert_eq!(refusal.path.as_deref(), Some(path.as_str()));
            assert!(refusal.message.contains(&path));
        }
        other => panic!("expected unresolvable-mount refusal, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn required_mount_dangling_symlink_is_refused() {
    use std::os::unix::fs::symlink;

    let td = private_tempdir();
    let path = td.path().join("link");
    symlink(td.path().join("missing"), &path).unwrap();
    let path = path.to_string_lossy().into_owned();
    let mut s = spec(Backend::Bwrap);
    s.mounts = vec![Mount {
        host: path.clone(),
        dest: path.clone(),
        ro: false,
        cache: false,
    }];

    match enter_argv(&s, "true") {
        Err(EnterError::Refused(refusal)) => {
            assert_eq!(refusal.class, RefusalClass::MountUnresolvable);
            assert_eq!(refusal.path.as_deref(), Some(path.as_str()));
        }
        other => panic!("expected unresolvable-mount refusal, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn required_mount_permission_denied_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    if nix::unistd::geteuid().as_raw() == 0 {
        // Root bypasses directory permissions, so this cannot exercise EACCES.
        return;
    }
    let td = private_tempdir();
    let locked = td.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let path = locked.join("child");
    std::fs::write(&path, "child").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let path = path.to_string_lossy().into_owned();
    let mut s = spec(Backend::Bwrap);
    s.mounts = vec![Mount {
        host: path.clone(),
        dest: path.clone(),
        ro: false,
        cache: false,
    }];
    let result = enter_argv(&s, "true");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();

    match result {
        Err(EnterError::Refused(refusal)) => {
            assert_eq!(refusal.class, RefusalClass::MountUnresolvable);
            assert_eq!(refusal.path.as_deref(), Some(path.as_str()));
        }
        other => panic!("expected unresolvable-mount refusal, got {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn optional_cache_mount_that_cannot_resolve_is_dropped() {
    use std::os::unix::fs::symlink;

    let td = private_tempdir();
    let loop_a = td.path().join("loop-a");
    let loop_b = td.path().join("loop-b");
    symlink(&loop_b, &loop_a).unwrap();
    symlink(&loop_a, &loop_b).unwrap();
    let worktree = td.path().join("worktree");
    std::fs::create_dir(&worktree).unwrap();
    let worktree = worktree.to_string_lossy().into_owned();
    let loop_a = loop_a.to_string_lossy().into_owned();
    let mut s = spec(Backend::Bwrap);
    s.worktree = PathBuf::from(&worktree);
    s.mounts = vec![
        Mount {
            host: worktree.clone(),
            dest: worktree.clone(),
            ro: false,
            cache: false,
        },
        Mount {
            host: loop_a.clone(),
            dest: loop_a.clone(),
            ro: true,
            cache: true,
        },
    ];

    let argv = enter_argv(&s, "true").expect("required worktree mount resolves");
    let joined = argv.join(" ");
    assert!(joined.contains(&worktree));
    assert!(!joined.contains(&loop_a));
}

#[cfg(unix)]
#[test]
fn absent_mount_resolves_through_its_existing_ancestor() {
    use std::os::unix::fs::symlink;

    let td = private_tempdir();
    let real = td.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = td.path().join("alias");
    symlink(&real, &alias).unwrap();
    let absent = alias.join("new");
    let resolved = real.join("new");
    let absent = absent.to_string_lossy().into_owned();
    let resolved = resolved.to_string_lossy().into_owned();
    let mut s = spec(Backend::Bwrap);
    s.mounts = vec![Mount {
        host: absent.clone(),
        dest: "/mnt/absent".into(),
        ro: false,
        cache: false,
    }];

    let argv = enter_argv(&s, "true").expect("an absent leaf resolves via its existing parent");
    let joined = argv.join(" ");
    assert!(joined.contains(&resolved));
    assert!(!argv.contains(&absent));
}

#[test]
fn absent_mount_without_symlinks_keeps_its_path() {
    let argv = enter_argv(&spec(Backend::Bwrap), "true")
        .expect("the shared fixture's absent paths resolve through existing ancestors");
    let joined = argv.join(" ");
    assert!(joined.contains("/wt/feat"));
    assert!(joined.contains("/repo/.git"));
}

#[test]
fn refusal_constructor_preserves_each_class_and_field() {
    let spec = spec(Backend::Docker);
    for class in [
        RefusalClass::ComposePolicy,
        RefusalClass::SealedHome,
        RefusalClass::UnsafeMountPath,
        RefusalClass::SystemdMountPath,
        RefusalClass::ComposeTarget,
        RefusalClass::ComposeServiceMissing,
        RefusalClass::HostnamePolicy,
        RefusalClass::MountUnresolvable,
        RefusalClass::VolumeOwner,
        RefusalClass::TunnelUnenforced,
        RefusalClass::MountDrift,
        RefusalClass::ControlSocketReachable,
        RefusalClass::StrictUnsupported,
        RefusalClass::ContainerOwner,
        RefusalClass::NetworkUnexpressible,
    ] {
        let error = refusal(
            &spec,
            class,
            Some("/safe/path".into()),
            "safe message".into(),
        );
        match &error {
            EnterError::Refused(refusal) => {
                assert_eq!(refusal.class, class);
                assert_eq!(refusal.backend, "docker");
                assert_eq!(refusal.path.as_deref(), Some("/safe/path"));
                assert_eq!(refusal.message, "safe message");
                assert_eq!(error.to_string(), "safe message");
            }
            other => panic!("expected structured refusal, got {other:?}"),
        }
    }
}

#[test]
fn refusal_remediation_every_class_has_label_and_text() {
    let mut labels = std::collections::HashSet::new();
    for class in [
        RefusalClass::ComposePolicy,
        RefusalClass::SealedHome,
        RefusalClass::UnsafeMountPath,
        RefusalClass::SystemdMountPath,
        RefusalClass::ComposeTarget,
        RefusalClass::ComposeServiceMissing,
        RefusalClass::HostnamePolicy,
        RefusalClass::MountUnresolvable,
        RefusalClass::VolumeOwner,
        RefusalClass::TunnelUnenforced,
        RefusalClass::MountDrift,
        RefusalClass::ControlSocketReachable,
        RefusalClass::StrictUnsupported,
        RefusalClass::ContainerOwner,
        RefusalClass::NetworkUnexpressible,
    ] {
        let label = class.label();
        assert!(!label.is_empty());
        assert!(label.is_ascii());
        assert!(
            label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        );
        assert!(labels.insert(label), "duplicate refusal label: {label}");

        let remediation = class.remediation();
        assert!(!remediation.is_empty());
        assert!(!remediation.ends_with('.'));
    }
    assert_eq!(RefusalClass::ContainerOwner.label(), "container-owner");
    assert_eq!(
        RefusalClass::ContainerOwner.remediation(),
        "another worktree's sandbox container has this container's name (worktree paths that differ only in punctuation or case share one): close that worktree's sandbox first, or rename one worktree directory"
    );
}

#[test]
fn refusal_is_identical_for_every_placement_and_builds_no_argv() {
    let placements = vec![
        Placement::Local,
        Placement::Ssh(SshPlacement::plain(
            "devbox".into(),
            22,
            false,
            TransportKind::Ssh,
        )),
        Placement::Provider(crate::placement::ProviderPlacement {
            provider: "test".into(),
            id: "sandbox".into(),
            interactive_prefix: vec![],
            control_prefix: vec![],
            up_command: vec![],
            down_command: vec![],
        }),
        Placement::K8s(crate::placement::K8sPlacement {
            kubectl: "kubectl".into(),
            context: None,
            namespace: None,
            pod: "pod".into(),
            container: Some("dev".into()),
            pod_template: None,
            image: None,
        }),
    ];
    let compose = crate::sandbox_compose::ComposeSpec {
        files: vec!["compose.yml".into()],
        service: Some("app".into()),
        run_services: Vec::new(),
    }
    .encode();
    let mut messages = Vec::new();
    let mut hints = Vec::new();

    for placement in &placements {
        let mut s = spec(Backend::Podman);
        s.read_only_root = true;
        s.compose = Some(compose.clone());
        s.placement = placement.clone();

        let error =
            enter_argv(&s, "true").expect_err("a Compose spec with read_only_root is refused");
        match &error {
            EnterError::Refused(refusal) => {
                assert_eq!(refusal.class, RefusalClass::ComposePolicy);
                assert_eq!(refusal.backend, "podman-rootless");
                assert!(refusal.path.is_none());
                assert_eq!(error.to_string(), refusal.message);
                messages.push(refusal.message.clone());
            }
            other => panic!("expected Compose policy refusal, got {other:?}"),
        }

        let hint = error.remediation_hint();
        assert_eq!(
            enter_argv_guarded(&s, "true").err(),
            Some(error.clone()),
            "guarded and unguarded builders must return the same typed refusal"
        );
        hints.push(hint);
    }

    assert!(messages.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(hints.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(
        hints.first().is_some_and(
            |hint| hint.contains(&format!("[{}]", RefusalClass::ComposePolicy.label()))
        )
    );

    // Positive control: a valid SSH placement still produces a wrapped argv.
    let mut ok = spec(Backend::Podman);
    ok.placement = Placement::Ssh(SshPlacement::plain(
        "devbox".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let argv = enter_argv(&ok, "bash -l").expect("valid volume names");
    assert_eq!(argv[0], "ssh");
}

#[test]
fn refusal_remediation_hint_names_class_path_and_fix() {
    let spec = spec(Backend::Docker);
    let error = refusal(
        &spec,
        RefusalClass::UnsafeMountPath,
        Some("/a b".into()),
        "safe message".into(),
    );
    let hint = error.remediation_hint();
    assert!(hint.contains("[unsafe-mount-path]"));
    assert!(hint.contains("`/a b`"));
    assert!(hint.contains(RefusalClass::UnsafeMountPath.remediation()));

    let error = refusal(
        &spec,
        RefusalClass::UnsafeMountPath,
        Some("/a\nb".into()),
        "safe message".into(),
    );
    let hint = error.remediation_hint();
    assert!(hint.contains("`/a\\nb`"));
    assert!(!hint.contains('\n'));

    let error = refusal(
        &spec,
        RefusalClass::UnsafeMountPath,
        None,
        "safe message".into(),
    );
    assert!(!error.remediation_hint().contains(" at "));

    let mut spec = spec;
    spec.volumes = vec![("/tmp/state".into(), "/mnt/state".into())];
    let error = enter_argv(&spec, "true").expect_err("invalid volume name must refuse");
    assert!(error.remediation_hint().contains("[volume-name]"));
}

#[test]
fn refusal_context_leads_with_the_remediation_hint() {
    let mut spec = spec(Backend::Docker);
    spec.volumes = vec![("/tmp/state".into(), "/mnt/state".into())];
    let error =
        anyhow::Error::new(enter_argv(&spec, "true").expect_err("invalid volume name must refuse"))
            .context("outer step");

    let context = refusal_context(&error);
    assert!(context.starts_with("sandbox entry refused [volume-name]"));
    assert!(context.contains("outer step"));
    assert_eq!(refusal_context(&anyhow::anyhow!("boom")), "boom");
}

#[test]
fn refusal_remediation_display_is_unchanged() {
    let error = refusal(
        &spec(Backend::Docker),
        RefusalClass::UnsafeMountPath,
        Some("/a b".into()),
        "safe message".into(),
    );
    assert_eq!(error.to_string(), "safe message");
}

#[test]
fn owner_verdict_classifies_worktree_labels() {
    let expected = "v1-expected";
    assert_eq!(
        owner_verdict(Some((true, "v1-expected\n".into())), expected),
        OwnerVerdict::Ours
    );
    assert_eq!(
        owner_verdict(Some((true, "\n".into())), expected),
        OwnerVerdict::Legacy
    );
    assert_eq!(
        owner_verdict(Some((true, "<no value>\n".into())), expected),
        OwnerVerdict::Legacy
    );
    assert_eq!(
        owner_verdict(Some((true, "v1-foreign".into())), expected),
        OwnerVerdict::Foreign("v1-foreign".into())
    );
    assert_eq!(
        owner_verdict(Some((true, "garbage".into())), expected),
        OwnerVerdict::Foreign("garbage".into())
    );
    assert_eq!(owner_verdict(None, expected), OwnerVerdict::Unknown);
    assert_eq!(
        owner_verdict(Some((false, String::new())), expected),
        OwnerVerdict::Unknown
    );
}

#[test]
fn owner_label_probe_reads_only_the_worktree_label() {
    let spec = spec(Backend::Docker);
    let argv = owner_label_probe_argv(&spec);
    let mut expected = oci_prefix(&spec);
    expected.extend([
        "container".into(),
        "inspect".into(),
        "--format".into(),
        "{{index .Config.Labels \"thegn.worktree\"}}".into(),
        spec.name.clone(),
    ]);
    assert_eq!(
        argv, expected,
        "the runtime must receive a Go template action"
    );
}

#[test]
fn foreign_or_unknown_owner_refuses_and_keeps_the_container() {
    let spec = spec(Backend::Docker);
    for verdict in [
        OwnerVerdict::Foreign("v1-other".into()),
        OwnerVerdict::Unknown,
    ] {
        let error = foreign_owner_refusal(&spec, &verdict)
            .expect("foreign or unreadable owner must refuse before removal");
        match error {
            EnterError::Refused(refusal) => {
                assert_eq!(refusal.class, RefusalClass::ContainerOwner);
                assert!(refusal.message.contains(&spec.name));
                assert!(refusal.message.contains("left running"));
                assert!(!refusal.message.contains("stopped "));
                assert!(!refusal.message.contains("shutdown refused for "));
                assert!(!refusal.message.contains("still running after "));
            }
            other => panic!("expected structured container-owner refusal, got {other:?}"),
        }
    }
    assert_eq!(foreign_owner_refusal(&spec, &OwnerVerdict::Ours), None);
    assert_eq!(foreign_owner_refusal(&spec, &OwnerVerdict::Legacy), None);
}

#[test]
fn slug_colliding_worktrees_get_a_foreign_verdict() {
    let first = "/w/feat-a";
    let second = "/w/feat_a";
    assert_eq!(container_name(first), container_name(second));
    let first_label = crate::sandbox_manage::worktree_label_value(first.as_bytes());
    let second_label = crate::sandbox_manage::worktree_label_value(second.as_bytes());
    assert_ne!(first_label, second_label);
    assert_eq!(
        owner_verdict(Some((true, first_label)), &second_label),
        OwnerVerdict::Foreign(crate::sandbox_manage::worktree_label_value(
            first.as_bytes()
        ))
    );
}

#[test]
fn podman_exec_preserves_paths() {
    let argv =
        enter_argv(&spec(Backend::Podman), "${SHELL:-/bin/sh} -l").expect("valid volume names");
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
    let argv = enter_argv(&s, "claude").expect("valid volume names");
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
    let joined = enter_argv(&ephemeral, "claude")
        .expect("valid volume names")
        .join(" ");
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
    let joined = enter_argv(&persistent, "claude")
        .expect("valid volume names")
        .join(" ");
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
    let argv = enter_argv(&s, "claude").expect("valid volume names");
    let joined = argv.join(" ");
    assert!(!joined.contains("--workdir"));
}

#[test]
fn oci_create_opts_map_userns_and_mounts() {
    // GH_TOKEN=abc is synthetic here (its value doesn't match the ambient
    // env) so it stays inline `-e`; unset it to keep that deterministic.
    let _env = crate::testenv::EnvGuard::unset(&["GH_TOKEN"]);
    let opts = oci_create_opts(&spec(Backend::Podman)).expect("valid volume names");
    let j = opts.join(" ");
    assert!(j.contains("--userns keep-id"));
    assert!(j.contains("-v /wt/feat:/wt/feat"));
    assert!(j.contains("-v /repo/.git:/repo/.git"));
    assert!(j.contains("-e GH_TOKEN=abc"));
    assert!(j.contains("-p 8080:8080"));
}

#[test]
fn apple_network_none_is_refused_before_argv() {
    let mut s = spec(Backend::Apple);
    s.network = Network::None;
    for result in [
        enter_argv(&s, "true").map(|_| ()),
        launchable_with(&s, &[]).map(|_| ()),
    ] {
        match result {
            Err(EnterError::Refused(refusal)) => {
                assert_eq!(refusal.class, RefusalClass::NetworkUnexpressible);
                assert_eq!(refusal.backend, "apple");
                assert!(refusal.path.is_none());
                assert!(refusal.message.contains("network = \"none\""));
            }
            other => panic!("expected network-unexpressible refusal, got {other:?}"),
        }
    }
}

#[test]
fn apple_sealed_profile_is_refused() {
    // The sealed profile forces `network = "none"`; the network refusal names
    // the real reason ahead of the strict hardening gate.
    let mut s = spec(Backend::Apple);
    s.network = Network::None;
    s.seal_home = Some("/home/test".into());
    s.read_only_root = true;
    s.no_new_privileges = true;
    s.pids_limit = Some(256);
    s.drop_capabilities = vec!["ALL".into()];
    match launchable_with(&s, &[]) {
        Err(EnterError::Refused(refusal)) => {
            assert_eq!(refusal.class, RefusalClass::NetworkUnexpressible);
            assert_eq!(refusal.class.label(), "network-unexpressible");
        }
        other => panic!("expected network-unexpressible refusal, got {other:?}"),
    }
}

#[test]
fn apple_nat_launches() {
    let s = spec(Backend::Apple);
    assert_eq!(s.network, Network::Nat);
    assert!(network_unexpressible_refusal(&s).is_none());
    assert!(launchable_with(&s, &[]).is_ok());
    let j = oci_create_opts(&s).expect("valid volume names").join(" ");
    assert!(!j.contains("--network"), "{j}");
}

#[test]
fn apple_network_none_refusal_names_host_reachability() {
    // Owner decision O21: Apple `none` stays refused while `container network
    // create --internal` still reaches services bound on the Mac. The refusal
    // must say so, not just "unexpressible".
    let mut s = spec(Backend::Apple);
    s.network = Network::None;
    let message = network_unexpressible_refusal(&s).expect("apple none is unexpressible");
    assert!(message.contains("reaches your Mac's services"), "{message}");
    assert!(message.contains("podman"), "{message}");
    assert!(message.contains("docker"), "{message}");
    assert!(message.contains("no interface"), "{message}");
}

#[test]
fn apple_network_host_refused_before_argv() {
    let mut s = spec(Backend::Apple);
    s.network = Network::Host;
    for result in [
        enter_argv(&s, "true").map(|_| ()),
        launchable_with(&s, &[]).map(|_| ()),
    ] {
        match result {
            Err(EnterError::Refused(refusal)) => {
                assert_eq!(refusal.class, RefusalClass::NetworkUnexpressible);
                assert_eq!(refusal.backend, "apple");
                assert!(refusal.path.is_none());
                assert!(
                    refusal.message.contains("no host network"),
                    "{}",
                    refusal.message
                );
                assert!(refusal.message.contains("`nat`"), "{}", refusal.message);
            }
            other => panic!("expected network-unexpressible refusal, got {other:?}"),
        }
    }
}

#[test]
fn apple_network_host_not_emitted() {
    // Even a direct argv compose (past the launch gate) must not hand Apple a
    // `--network host` or `--network none` it has no keyword for.
    for network in [Network::Host, Network::None] {
        let mut s = spec(Backend::Apple);
        s.network = network;
        let argv = oci_create_opts_in(&s, &[], None).expect("valid volume names");
        assert!(
            !argv.iter().any(|arg| arg == "--network"),
            "{network:?}: {argv:?}"
        );
        assert!(
            !argv.iter().any(|arg| arg == "host"),
            "{network:?}: {argv:?}"
        );
        assert!(
            !argv.iter().any(|arg| arg == "none"),
            "{network:?}: {argv:?}"
        );
    }
}

#[test]
fn oci_network_none_unchanged_for_podman_docker() {
    for backend in [Backend::Podman, Backend::PodmanRootful, Backend::Docker] {
        let mut s = spec(backend);
        s.network = Network::None;
        assert!(network_unexpressible_refusal(&s).is_none(), "{backend:?}");
        let j = oci_create_opts(&s).expect("valid volume names").join(" ");
        assert!(j.contains("--network none"), "{backend:?}: {j}");
    }
}

#[test]
fn refusal_class_label_is_stable() {
    assert_eq!(
        RefusalClass::NetworkUnexpressible.label(),
        "network-unexpressible"
    );
    assert!(
        RefusalClass::NetworkUnexpressible
            .remediation()
            .contains("network = \"nat\"")
    );
}

#[test]
fn apple_create_opts_omit_flags_its_run_rejects() {
    // `container run` (Apple `container` 1.2.2) has no `--security-opt` and no
    // `--pids-limit`; emitting either makes create exit 64 (EX_USAGE), so the
    // default hardened profile could never start an `apple` container.
    let mut a = spec(Backend::Apple);
    a.no_new_privileges = true;
    a.pids_limit = Some(512);
    let j = oci_create_opts(&a).expect("valid volume names").join(" ");
    assert!(!j.contains("--security-opt"), "{j}");
    assert!(!j.contains("--pids-limit"), "{j}");
    // The flags Apple DOES accept must survive — this is a narrowing of two
    // knobs, not a wholesale drop of hardening.
    let mut a2 = spec(Backend::Apple);
    a2.read_only_root = true;
    a2.drop_capabilities = vec!["ALL".into()];
    let j2 = oci_create_opts(&a2).expect("valid volume names").join(" ");
    assert!(j2.contains("--read-only"), "{j2}");
    assert!(j2.contains("--cap-drop ALL"), "{j2}");

    // Every other OCI backend keeps both flags.
    for b in [Backend::Podman, Backend::PodmanRootful, Backend::Docker] {
        let mut s = spec(b);
        s.no_new_privileges = true;
        s.pids_limit = Some(512);
        let j = oci_create_opts(&s).expect("valid volume names").join(" ");
        assert!(j.contains("--security-opt no-new-privileges"), "{b:?}: {j}");
        assert!(j.contains("--pids-limit 512"), "{b:?}: {j}");
    }
}

#[test]
fn apple_cpu_limit_is_rounded_to_whole_cores() {
    // Apple's `--cpus` parses an integer; docker/podman take a fraction. Round
    // UP so a cap never silently tightens, and floor at 1 so a sub-core request
    // can't become `--cpus 0`.
    assert_eq!(cpu_limit_for(Backend::Apple, "1.5"), "2");
    assert_eq!(cpu_limit_for(Backend::Apple, "0.5"), "1");
    assert_eq!(cpu_limit_for(Backend::Apple, "4"), "4");
    // Unparseable passes through — the runtime's own error is clearer than a
    // number we invented.
    assert_eq!(cpu_limit_for(Backend::Apple, "lots"), "lots");
    // Everyone else is untouched.
    assert_eq!(cpu_limit_for(Backend::Docker, "1.5"), "1.5");
    assert_eq!(cpu_limit_for(Backend::Podman, "0.5"), "0.5");
}

#[test]
fn dropped_hardening_is_reported_not_swallowed() {
    // A weaker profile than the config asked for must be visible to `doctor`
    // and the Sandbox panel — same rule as "display observed containment".
    let mut a = spec(Backend::Apple);
    a.no_new_privileges = true;
    a.pids_limit = Some(512);
    assert_eq!(
        unsupported_hardening(&a),
        vec!["no-new-privileges", "pids-limit"]
    );
    // Nothing requested ⇒ nothing to report.
    let plain = spec(Backend::Apple);
    assert!(unsupported_hardening(&plain).is_empty());
    // A runtime that supports them reports nothing even when they're set.
    let mut p = spec(Backend::Podman);
    p.no_new_privileges = true;
    p.pids_limit = Some(512);
    assert!(unsupported_hardening(&p).is_empty());
}

#[test]
fn oci_runtime_injected_only_for_engines_with_runtime_flag() {
    // Unset ⇒ no --runtime (daemon default).
    assert!(
        !oci_create_opts(&spec(Backend::Podman))
            .expect("valid volume names")
            .join(" ")
            .contains("--runtime")
    );

    // Set on an engine that accepts the flag ⇒ `--runtime <value>` at create.
    let mut s = spec(Backend::Podman);
    s.oci_runtime = Some("runsc".into());
    assert!(
        oci_create_opts(&s)
            .expect("valid volume names")
            .join(" ")
            .contains("--runtime runsc")
    );

    let mut k = spec(Backend::Docker);
    k.oci_runtime = Some("krun".into());
    assert!(
        oci_create_opts(&k)
            .expect("valid volume names")
            .join(" ")
            .contains("--runtime krun")
    );

    // Apple's `container` rejects unknown flags; the value is ignored.
    let mut apple = spec(Backend::Apple);
    apple.oci_runtime = Some("runsc".into());
    assert!(
        !oci_create_opts(&apple)
            .expect("valid volume names")
            .join(" ")
            .contains("--runtime")
    );

    // A blank/whitespace value is treated as unset.
    let mut blank = spec(Backend::Podman);
    blank.oci_runtime = Some("  ".into());
    assert!(
        !oci_create_opts(&blank)
            .expect("valid volume names")
            .join(" ")
            .contains("--runtime")
    );
}

#[test]
fn runtime_flag_absent_for_apple() {
    // Apple's `container` exits 64 on an unknown flag, so `create` with a
    // configured runtime would fail every pane.
    let mut apple = spec(Backend::Apple);
    apple.oci_runtime = Some("runsc".into());
    let joined = oci_create_opts(&apple)
        .expect("valid volume names")
        .join(" ");
    assert!(!joined.contains("--runtime"), "{joined}");
}

#[test]
fn podman_runsc_emits_flag() {
    let mut podman = spec(Backend::Podman);
    podman.oci_runtime = Some("runsc".into());
    let joined = oci_create_opts(&podman)
        .expect("valid volume names")
        .join(" ");
    assert!(joined.contains("--runtime runsc"), "{joined}");
    assert_eq!(
        crate::capabilities::Capabilities::from_parts(
            Backend::Podman,
            &crate::placement::Placement::Local,
            crate::config::Network::Nat,
            false,
            Some("runsc"),
        )
        .isolation,
        crate::capabilities::IsolationClass::UserspaceKernel
    );
}

#[test]
fn docker_krun_emits_flag() {
    let mut docker = spec(Backend::Docker);
    docker.oci_runtime = Some("krun".into());
    let joined = oci_create_opts(&docker)
        .expect("valid volume names")
        .join(" ");
    assert!(joined.contains("--runtime krun"), "{joined}");
    assert_eq!(
        crate::capabilities::Capabilities::from_parts(
            Backend::Docker,
            &crate::placement::Placement::Local,
            crate::config::Network::Nat,
            false,
            Some("krun"),
        )
        .isolation,
        crate::capabilities::IsolationClass::GuestKernel
    );
}

#[test]
fn oci_runtime_on_apple_warns_once() {
    let mut apple = spec(Backend::Apple);
    apple.oci_runtime = Some("runsc".into());
    let mut warnings = Vec::new();
    warn_unsupported_oci_runtime(&apple, &mut warnings);
    warn_unsupported_oci_runtime(&apple, &mut warnings);
    assert_eq!(
        warnings,
        vec!["oci_runtime ignored: apple has no --runtime"]
    );

    // An engine that accepts the flag never warns, and a blank value is unset.
    let mut podman = spec(Backend::Podman);
    podman.oci_runtime = Some("runsc".into());
    warn_unsupported_oci_runtime(&podman, &mut warnings);
    let mut blank = spec(Backend::Apple);
    blank.oci_runtime = Some("  ".into());
    warn_unsupported_oci_runtime(&blank, &mut warnings);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
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
    let j = oci_create_opts(&s).expect("valid volume names").join(" ");
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
    let emitted = oci_create_opts(&s).expect("valid volume names").join(" ");
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
    let argv = enter_argv(&s, "${SHELL:-/bin/sh} -l").expect("valid volume names");
    assert_eq!(
        argv[4], "mosh",
        "mosh argv rides the fallback wrapper: {argv:?}"
    );
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
    let argv = enter_argv(&s, "claude").expect("valid volume names");
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
    let argv = enter_argv(&s, "true").expect("valid volume names");
    assert!(!argv.contains(&"PATH".to_string()));
    let i = argv.iter().position(|a| a == "--setenv").unwrap();
    assert_eq!(argv[i + 1], "THEGN_SANDBOX");

    // Remote-wrapped bwrap has no launch environment on the far side, so it
    // cannot inherit the pair and must not put the value on the command string
    // that ssh carries. It omits the key with a keys-only warning instead.
    s.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let entered = enter_argv_guarded(&s, "true").expect("valid volume names");
    let remote = entered.argv().join(" ");
    let path_value = std::env::var("PATH").unwrap();
    assert!(!remote.contains("--setenv PATH"), "remote argv: {remote}");
    assert!(
        !remote.contains(&path_value),
        "remote argv leaked PATH: {remote}"
    );
    assert!(remote.contains("--setenv THEGN_SANDBOX 1"), "{remote}");
    let warning = entered
        .omitted_secrets_warning()
        .expect("omitted remote secret should warn");
    assert!(warning.contains("PATH"), "{warning}");
    assert!(
        !warning.contains(&path_value),
        "warning leaked PATH: {warning}"
    );
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
    let argv = enter_argv(&s, "true").expect("valid volume names");
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
        memory_total: None,
        memory_high: None,
        memory_swap: None,
        memory_swap_total: None,
    };
    s.volumes = vec![("data-vol".into(), "/mnt/data".into())];

    let opts = oci_create_opts(&s).expect("valid volume names");
    let j_opts = opts.join(" ");
    assert!(j_opts.contains("--device nvidia.com/gpu=all"));
    assert!(j_opts.contains("--cpus 2"));
    assert!(j_opts.contains("--memory 4GB"));
    assert!(j_opts.contains("-v data-vol:/mnt/data"));
}

#[test]
fn named_volume_admission_accepts_existing_names_and_rejects_paths() {
    for valid in [
        "thegn-nix-store",
        "thegn-cargo",
        "nix-store",
        "cargo",
        "data.vol",
    ] {
        assert!(validate_volume_name(valid).is_ok(), "valid name: {valid}");
    }
    for invalid in [
        "",
        "a",
        ".",
        "..",
        ".cache",
        "-cache",
        "/tmp/state",
        "./state",
        "../state",
        "state/sub",
        r"state\sub",
        "C:/state",
        r"C:\state",
        r"\\server\share",
        "~/state",
        "name:ro",
        "name,ro",
        "name\tvalue",
        "name\0value",
        "é-cache",
    ] {
        let error = validate_volume_name(invalid).expect_err(invalid);
        let rendered = error.to_string();
        assert!(!rendered.contains("/tmp/state"));
        assert!(!rendered.contains("state\\sub"));
        assert!(!rendered.contains("name:ro"));
        assert!(!rendered.contains("name\\tvalue"));
        assert!(!rendered.contains("é-cache"));
    }
}

#[test]
fn sealed_systemd_argv_uses_protecthome_tmpfs_and_binds_back_only_the_allowlist() {
    // `/home/...` is where `ProtectHome=tmpfs` reaches; `backend_enter_argv` is
    // used directly because the gate (correctly) needs a real, resolvable HOME.
    let mut s = spec(Backend::Systemd);
    s.read_only_root = true;
    s.seal_home = Some("/home/synthetic-user".into());
    s.mounts = vec![
        Mount {
            host: "/nix/store/x-zshrc".into(),
            dest: "/home/synthetic-user/.zshrc".into(),
            ro: true,
            cache: false,
        },
        Mount {
            host: "/home/synthetic-user/wt".into(),
            dest: "/home/synthetic-user/wt".into(),
            ro: false,
            cache: false,
        },
    ];
    let j = backend_enter_argv(&s, "true").unwrap().join(" ");
    assert!(j.contains("ProtectHome=tmpfs"), "{j}");
    assert!(
        j.contains("BindReadOnlyPaths=/nix/store/x-zshrc:/home/synthetic-user/.zshrc"),
        "{j}"
    );
    assert!(
        j.contains("BindPaths=/home/synthetic-user/wt:/home/synthetic-user/wt"),
        "{j}"
    );
    assert!(
        !j.contains(".local/state") && !j.contains(".local/share"),
        "{j}"
    );
}

#[test]
fn named_volume_admission_reports_pair_index_without_host_fallback() {
    let mut s = spec(Backend::Podman);
    s.volumes = vec![
        ("ok-volume".into(), "/mnt/ok".into()),
        ("/tmp/state".into(), "/mnt/bad".into()),
    ];
    let error = admit_volumes(&s).expect_err("path-like source must refuse");
    assert_eq!(error.pair_index, 1);
    assert_eq!(error.name_len, "/tmp/state".len());
    let fallible = enter_argv(&s, "true").expect_err("direct entry must refuse");
    assert_eq!(fallible, EnterError::Volume(error));
    assert!(!fallible.to_string().contains("/tmp/state"));
    assert!(oci_create_opts_with_keep_id(&s, false).is_err());
}

#[test]
fn named_volume_admission_precedes_oci_ensure_without_side_effects() {
    let mut s = spec(Backend::Podman);
    s.volumes = vec![("/tmp/state".into(), "/mnt/state".into())];
    let error = ensure(&s).expect_err("invalid volume must stop before OCI ensure");
    assert!(matches!(
        error.downcast_ref::<EnterError>(),
        Some(EnterError::Volume(_))
    ));
    assert!(!error.to_string().contains("/tmp/state"));
}

#[test]
fn ensure_preserves_typed_refusal_before_oci_side_effects() {
    let mut s = spec(Backend::Podman);
    s.mounts.push(Mount {
        host: "/tmp/source".into(),
        dest: "/mnt/bad path".into(),
        ro: false,
        cache: false,
    });
    let error = ensure(&s).expect_err("unsafe OCI mount must refuse before ensure");
    assert!(matches!(
        error.downcast_ref::<EnterError>(),
        Some(EnterError::Refused(Refusal {
            class: RefusalClass::UnsafeMountPath,
            backend,
            path: Some(path),
            ..
        })) if backend == "podman-rootless" && path == "/mnt/bad path"
    ));
}

#[test]
fn compose_refusal_tracks_only_unenforced_restrictions() {
    let mut base = spec(Backend::Docker);
    base.compose = Some("compose.yml".into());
    base.ports.clear();
    base.limits = SandboxLimits::default();
    assert!(compose_policy_refusal(&base).is_none());
    for (field, set) in [
        (
            "read_only_root",
            Box::new(|s: &mut SandboxSpec| s.read_only_root = true)
                as Box<dyn Fn(&mut SandboxSpec)>,
        ),
        (
            "no_new_privileges",
            Box::new(|s| s.no_new_privileges = true),
        ),
        ("pids_limit", Box::new(|s| s.pids_limit = Some(100))),
        (
            "drop_capabilities",
            Box::new(|s| s.drop_capabilities.push("ALL".into())),
        ),
        (
            "seal_home",
            Box::new(|s| s.seal_home = Some("/home/test".into())),
        ),
        ("network_none", Box::new(|s| s.network = Network::None)),
        (
            "network_allow",
            Box::new(|s| s.network_allow.push("example.com".into())),
        ),
        (
            "network_block",
            Box::new(|s| s.network_block.push("example.com".into())),
        ),
        ("network_host", Box::new(|s| s.network = Network::Host)),
        (
            "egress",
            Box::new(|s| {
                s.egress = Some(crate::egress::plan::EgressPlan {
                    lease: crate::egress::plan::LeaseId([0; 16]),
                    path: crate::egress::plan::EgressPath::External,
                    endpoint: crate::egress::plan::ProxyEndpoint::Relay {
                        host: "proxy.invalid".into(),
                        port: 443,
                    },
                    auth: crate::egress::plan::ProxyAuth::Token {
                        lease_user: "test".into(),
                        token: crate::egress::plan::LeaseToken::new("synthetic".into()),
                    },
                    env: Vec::new(),
                    binds: Vec::new(),
                    policy_digest: [0; 32],
                });
            }),
        ),
        (
            "oci_runtime",
            Box::new(|s| s.oci_runtime = Some("runsc".into())),
        ),
        ("cpu", Box::new(|s| s.limits.cpu = Some("1".into()))),
        ("memory", Box::new(|s| s.limits.memory = Some("1g".into()))),
    ] {
        let mut candidate = base.clone();
        set(&mut candidate);
        assert!(
            compose_policy_refusal(&candidate).is_some(),
            "{field} must refuse"
        );
    }
    let mut vpn = base.clone();
    vpn.vpn = build_vpn_spec(
        &vpn_cfg(VpnProviderKind::Tailscale),
        "test",
        SandboxProfile::Hardened,
    );
    assert!(
        compose_policy_refusal(&vpn).is_some(),
        "VPN must refuse Compose"
    );
    for (field, set) in [
        (
            "ports",
            Box::new(|s: &mut SandboxSpec| s.ports.push("8080:80".into()))
                as Box<dyn Fn(&mut SandboxSpec)>,
        ),
        ("gpu", Box::new(|s| s.gpu = Some("all".into()))),
        (
            "volumes",
            Box::new(|s| s.volumes.push(("data".into(), "/data".into()))),
        ),
        (
            "add_capabilities",
            Box::new(|s| s.add_capabilities.push("NET_ADMIN".into())),
        ),
        (
            "cpu_total_off",
            Box::new(|s| s.limits.cpu_total = Some("off".into())),
        ),
        (
            "memory_total",
            Box::new(|s| s.limits.memory_total = Some("2g".into())),
        ),
    ] {
        let mut candidate = base.clone();
        set(&mut candidate);
        assert!(
            compose_policy_refusal(&candidate).is_none(),
            "{field} is not a refusal"
        );
    }
    let compose_worker = crate::sandbox_compose::ComposeSpec {
        files: vec!["compose.yml".into()],
        service: Some("worker".into()),
        run_services: vec![],
    }
    .encode();
    let mut bwrap = base;
    bwrap.compose = Some(compose_worker.clone());
    bwrap.backend = Backend::Bwrap;
    bwrap.read_only_root = true;
    let argv = enter_argv(&bwrap, "true").unwrap();
    assert!(argv.iter().any(|arg| arg == "bwrap"), "{argv:?}");
    assert!(!argv.iter().any(|arg| arg == "compose" || arg == "exec"));
    let mut isolated = spec(Backend::Bwrap);
    isolated.read_only_root = true;
    let bwrap_argv = backend_enter_argv(&isolated, "true").unwrap();
    assert!(
        bwrap_argv
            .windows(2)
            .any(|pair| pair[0] == "--tmpfs" && pair[1] == "/tmp")
    );
    assert!(
        !bwrap_argv
            .iter()
            .any(|arg| { arg == "/run/user" || arg.starts_with("/run/user/") })
    );
    let mut systemd = bwrap;
    systemd.backend = Backend::Systemd;
    let argv = enter_argv(&systemd, "true").unwrap();
    assert_eq!(argv.first().map(String::as_str), Some("systemd-run"));
    assert!(!argv.iter().any(|arg| arg == "compose" || arg == "exec"));
    let mut runtime_spec = spec(Backend::Systemd);
    runtime_spec.read_only_root = true;
    let runtime_argv = backend_enter_argv_with_env(
        &runtime_spec,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user/42".into()),
            control_dirs: Vec::new(),
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        runtime_argv
            .windows(2)
            .any(|pair| { pair[0] == "-p" && pair[1] == "InaccessiblePaths=-/run/user/42/thegn" })
    );
}

#[test]
fn empty_xdg_runtime_dir_uses_run_user_for_systemd_hardening() {
    assert_eq!(
        systemd_runtime_dir(Some(std::ffi::OsString::new())),
        Some(std::path::PathBuf::from("/run/user"))
    );
}

#[test]
fn user_bus_hardened_bwrap_pane_unsets_the_session_bus_address() {
    let _env = crate::testenv::EnvGuard::unset(&["DBUS_SESSION_BUS_ADDRESS"]);
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.read_only_root = true;
    sandbox
        .env
        .push(("DBUS_SESSION_BUS_ADDRESS".into(), "unix:path=/x".into()));
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user/42".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let unset = argv
        .windows(2)
        .position(|pair| pair == ["--unsetenv", "DBUS_SESSION_BUS_ADDRESS"])
        .expect("hardened bwrap explicitly unsets the inherited session bus");
    let end = argv
        .iter()
        .position(|arg| arg == "--")
        .expect("bwrap argv has a command separator");
    assert!(
        unset < end,
        "bus address is unset before the pane command: {argv:?}"
    );
    assert!(
        !argv[unset + 2..end].iter().any(|arg| arg == "--setenv"),
        "unset follows every explicit setenv: {argv:?}"
    );
    assert!(
        argv.windows(3)
            .any(|triple| { triple == ["--setenv", "DBUS_SESSION_BUS_ADDRESS", "unix:path=/x"] }),
        "fixture proves an explicit bus passthrough precedes the unset: {argv:?}"
    );
}

#[test]
fn user_bus_open_bwrap_pane_keeps_the_session_bus_address() {
    // Positive control: absence of an unset is already true at the frozen base.
    let sandbox = spec(Backend::Bwrap);
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user/42".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        !argv
            .windows(2)
            .any(|pair| pair == ["--unsetenv", "DBUS_SESSION_BUS_ADDRESS"]),
        "open bwrap panes keep the session bus address: {argv:?}"
    );
}

#[test]
fn user_bus_hardened_systemd_pane_hides_the_user_bus() {
    let mut sandbox = spec(Backend::Systemd);
    sandbox.read_only_root = true;
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user/42".into()),
            ..Default::default()
        },
    )
    .unwrap();
    for property in [
        "UnsetEnvironment=DBUS_SESSION_BUS_ADDRESS",
        "InaccessiblePaths=-/run/user/42/bus",
        "InaccessiblePaths=-/run/user/42/systemd/private",
        "InaccessiblePaths=-/run/user/42/thegn",
        "InaccessiblePaths=-/run/user/42/podman",
        "InaccessiblePaths=-/run/user/42/docker",
        "InaccessiblePaths=-/run/user/42/docker.sock",
        "UnsetEnvironment=THEGN_CONTROL_SOCKET",
    ] {
        assert!(
            argv.windows(2)
                .any(|pair| pair[0] == "-p" && pair[1] == property),
            "hardened systemd pane includes -p {property}: {argv:?}"
        );
    }
}

#[test]
fn user_bus_systemd_run_user_fallback_adds_no_bus_paths() {
    let mut sandbox = spec(Backend::Systemd);
    sandbox.read_only_root = true;
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        argv.windows(2).any(|pair| {
            pair[0] == "-p" && pair[1] == "UnsetEnvironment=DBUS_SESSION_BUS_ADDRESS"
        }),
        "fallback systemd panes still unset the inherited bus address: {argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| {
            arg == "InaccessiblePaths=-/run/user/bus"
                || arg == "InaccessiblePaths=-/run/user/systemd"
        }),
        "fallback runtime is already inaccessible as a whole: {argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| arg.contains("/run/user/podman")),
        "fallback runtime adds no per-entry container paths: {argv:?}"
    );
}

/// A temp runtime dir holding a stand-in bus (plain file), `podman/` and the
/// thegn control dir, plus a bwrap spec that binds the whole runtime dir.
fn runtime_escape_fixture() -> (tempfile::TempDir, PathBuf, SandboxSpec) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let runtime = temp.path().join("rt");
    std::fs::create_dir_all(runtime.join("podman")).unwrap();
    std::fs::create_dir_all(runtime.join("thegn/abc")).unwrap();
    std::fs::write(runtime.join("bus"), b"").unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: runtime.display().to_string(),
        dest: runtime.display().to_string(),
        ro: false,
        cache: false,
    });
    (temp, runtime, sandbox)
}

#[test]
fn runtime_escape_bwrap_masks_bus_podman_and_unsets_the_control_socket() {
    let (_temp, runtime, mut sandbox) = runtime_escape_fixture();
    let control_dir = runtime.join("thegn");
    let socket = control_dir.join("abc/daemon.sock");
    sandbox
        .env
        .push(("THEGN_CONTROL_SOCKET".into(), socket.display().to_string()));
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some(runtime.clone()),
            control_dirs: vec![control_dir],
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    let rt = runtime.display().to_string();
    let bind = argv
        .windows(3)
        .position(|triple| triple == ["--bind", rt.as_str(), rt.as_str()])
        .expect("runtime dir is bound");
    let podman = format!("{rt}/podman");
    let bus = format!("{rt}/bus");
    let tmpfs = argv
        .windows(2)
        .position(|pair| pair == ["--tmpfs", podman.as_str()])
        .expect("podman dir is masked with a tmpfs");
    let null = argv
        .windows(3)
        .position(|triple| triple == ["--ro-bind", "/dev/null", bus.as_str()])
        .expect("bus socket is masked with /dev/null");
    let unset = argv
        .windows(2)
        .position(|pair| pair == ["--unsetenv", "THEGN_CONTROL_SOCKET"])
        .expect("dangling control socket is unset");
    let command = argv.iter().position(|arg| arg == "--").unwrap();
    assert!(bind < tmpfs && bind < null && bind < unset, "{argv:?}");
    assert!(tmpfs < command && null < command && unset < command);
    assert!(
        !argv[unset + 2..command].iter().any(|arg| arg == "--setenv"),
        "unset follows every explicit setenv: {argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| *arg == format!("{rt}/systemd")),
        "absent entries are not masked: {argv:?}"
    );
}

#[test]
fn runtime_escape_bwrap_unsets_the_control_socket_the_daemon_injects_later() {
    let (_temp, runtime, sandbox) = runtime_escape_fixture();
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some(runtime.clone()),
            control_dirs: vec![runtime.join("thegn")],
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--unsetenv", "THEGN_CONTROL_SOCKET"]),
        "the daemon-injected control socket is unset: {argv:?}"
    );
}

#[test]
fn runtime_escape_bwrap_sees_through_a_non_canonical_mount_source() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let runtime = temp.path().join("rt");
    std::fs::create_dir_all(runtime.join("podman")).unwrap();
    std::fs::create_dir_all(runtime.join("thegn/abc")).unwrap();
    std::fs::write(runtime.join("bus"), b"").unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: format!("{}/podman/..", runtime.display()),
        dest: runtime.display().to_string(),
        ro: false,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            runtime_dir: Some(runtime.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let podman = format!("{}/podman", runtime.display());
    let bus = format!("{}/bus", runtime.display());
    assert!(
        argv.windows(3)
            .any(|triple| triple == ["--ro-bind", "/dev/null", bus.as_str()]),
        "bus socket is masked through a `..` source: {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--tmpfs", podman.as_str()]),
        "podman dir is masked through a `..` source: {argv:?}"
    );
}

#[test]
fn runtime_escape_bwrap_honours_an_explicit_podman_socket_mount() {
    let (_temp, runtime, mut sandbox) = runtime_escape_fixture();
    let socket = runtime.join("podman/podman.sock");
    std::fs::write(&socket, b"").unwrap();
    sandbox.mounts.push(Mount {
        host: socket.display().to_string(),
        dest: socket.display().to_string(),
        ro: false,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some(runtime.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let podman = format!("{}/podman", runtime.display());
    let bus = format!("{}/bus", runtime.display());
    assert!(
        !argv
            .windows(2)
            .any(|pair| pair == ["--tmpfs", podman.as_str()]),
        "an explicit socket mount is honoured: {argv:?}"
    );
    assert!(
        argv.windows(3)
            .any(|triple| triple == ["--ro-bind", "/dev/null", bus.as_str()]),
        "{argv:?}"
    );
}

/// The OCI twin of [`runtime_escape_fixture`]: a temp runtime dir with a bus
/// stand-in, `podman/` and `systemd/`, and a spec binding its parent (the
/// `/run/user` shape).
fn oci_runtime_escape_fixture(backend: Backend) -> (tempfile::TempDir, PathBuf, SandboxSpec) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let base = std::fs::canonicalize(temp.path()).unwrap();
    let runtime = base.join("1000");
    std::fs::create_dir_all(runtime.join("podman")).unwrap();
    std::fs::create_dir_all(runtime.join("systemd")).unwrap();
    std::fs::write(runtime.join("bus"), b"").unwrap();
    let mut sandbox = spec(backend);
    sandbox.mounts.push(Mount {
        host: base.display().to_string(),
        dest: base.display().to_string(),
        ro: false,
        cache: false,
    });
    (temp, runtime, sandbox)
}

#[test]
fn runtime_escape_oci_masks_a_local_run_user_mount() {
    let (_temp, runtime, sandbox) = oci_runtime_escape_fixture(Backend::Podman);
    let argv = oci_create_opts_in(&sandbox, &[], Some(&runtime)).unwrap();
    let rt = runtime.display().to_string();
    let base = runtime.parent().unwrap().display().to_string();
    let bind_arg = format!("{base}:{base}");
    let systemd_arg = format!("{rt}/systemd");
    let podman_arg = format!("{rt}/podman");
    let bus_arg = format!("/dev/null:{rt}/bus:ro");
    let bind = argv
        .windows(2)
        .position(|pair| pair == ["-v", bind_arg.as_str()])
        .expect("runtime parent is bound");
    let systemd = argv
        .windows(2)
        .position(|pair| pair == ["--tmpfs", systemd_arg.as_str()])
        .expect("systemd dir is masked with a tmpfs");
    let podman = argv
        .windows(2)
        .position(|pair| pair == ["--tmpfs", podman_arg.as_str()])
        .expect("podman dir is masked with a tmpfs");
    let bus = argv
        .windows(2)
        .position(|pair| pair == ["-v", bus_arg.as_str()])
        .expect("bus socket is masked with /dev/null");
    assert!(bind < systemd && bind < podman && bind < bus, "{argv:?}");
    let docker = format!("{rt}/docker");
    assert!(
        !argv.iter().any(|arg| arg.contains(&docker)),
        "absent entries are not masked: {argv:?}"
    );
    // Docker and rootful podman take the same flags.
    for backend in [Backend::Docker, Backend::PodmanRootful] {
        let (_temp, runtime, sandbox) = oci_runtime_escape_fixture(backend);
        let argv = oci_create_opts_in(&sandbox, &[], Some(&runtime)).unwrap();
        let bus = format!("/dev/null:{}/bus:ro", runtime.display());
        assert!(argv.contains(&bus), "{backend:?}: {argv:?}");
    }
}

#[test]
fn runtime_escape_oci_honours_an_explicit_podman_socket_mount() {
    let (_temp, runtime, mut sandbox) = oci_runtime_escape_fixture(Backend::Podman);
    let socket = runtime.join("podman/podman.sock");
    std::fs::write(&socket, b"").unwrap();
    sandbox.mounts.push(Mount {
        host: socket.display().to_string(),
        dest: socket.display().to_string(),
        ro: false,
        cache: false,
    });
    let argv = oci_create_opts_in(&sandbox, &[], Some(&runtime)).unwrap();
    let rt = runtime.display().to_string();
    let podman_arg = format!("{rt}/podman");
    let systemd_arg = format!("{rt}/systemd");
    let bus_arg = format!("/dev/null:{rt}/bus:ro");
    assert!(
        !argv
            .windows(2)
            .any(|pair| pair == ["--tmpfs", podman_arg.as_str()]),
        "an explicit socket mount is honoured: {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--tmpfs", systemd_arg.as_str()]),
        "{argv:?}"
    );
    assert!(argv.contains(&bus_arg), "{argv:?}");
}

#[test]
fn runtime_escape_oci_skips_remote_and_unexposed_specs() {
    let (_temp, runtime, mut remote) = oci_runtime_escape_fixture(Backend::Podman);
    remote.placement = remote_placement();
    let rt = format!("{}/", runtime.display());
    let masked = |argv: &[String]| {
        argv.iter()
            .any(|arg| arg.starts_with(&rt) || arg.starts_with("/dev/null:"))
    };
    let argv = oci_create_opts_in(&remote, &[], Some(&runtime)).unwrap();
    assert!(!masked(&argv), "remote OCI is unchanged: {argv:?}");
    let plain = spec(Backend::Podman);
    let argv = oci_create_opts_in(&plain, &[], Some(&runtime)).unwrap();
    assert!(!masked(&argv), "no mask without an exposing bind: {argv:?}");
    let (_temp2, runtime2, apple) = oci_runtime_escape_fixture(Backend::Apple);
    assert!(oci_runtime_escape_args(&apple, Some(&runtime2)).is_empty());
    let (_temp3, runtime3, local) = oci_runtime_escape_fixture(Backend::Podman);
    // bus null bind (2 args) + systemd and podman tmpfs (2 args each).
    assert_eq!(oci_runtime_escape_args(&local, Some(&runtime3)).len(), 6);
    assert!(oci_runtime_escape_args(&local, None).is_empty());
}

fn remote_placement() -> Placement {
    Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ))
}

fn deploy_git(key: &str) -> crate::config::SandboxGitConfig {
    crate::config::SandboxGitConfig {
        mode: crate::config::SandboxGitMode::Full,
        credential: crate::config::GitCredential::DeployKey,
        deploy_key: key.to_string(),
        ..Default::default()
    }
}

fn good_key_facts(
    _: &std::path::Path,
) -> Option<crate::config_sandbox::git_policy::DeployKeyFacts> {
    Some(crate::config_sandbox::git_policy::DeployKeyFacts {
        regular: true,
        symlink: false,
        owned_by_user: true,
        mode: Some(0o600),
    })
}

fn deploy_cx<'a>(
    placement: &'a Placement,
    backend: Backend,
    mounts: &'a [Mount],
    env: &'a [(String, String)],
) -> DeployKeyContext<'a> {
    DeployKeyContext {
        placement,
        backend,
        file_access: FileAccess::default(),
        seal_home: None,
        mounts,
        env,
    }
}

fn deploy_plan(
    git: &crate::config::SandboxGitConfig,
    placement: &Placement,
    backend: Backend,
    mounts: &[Mount],
    env: &[(String, String)],
) -> DeployKeyPlan {
    plan_deploy_key(
        git,
        &deploy_cx(placement, backend, mounts, env),
        &good_key_facts,
    )
}

#[test]
fn deploy_key_plan_binds_read_only_at_the_fixed_path() {
    let git = deploy_git("/keys/deploy");
    for backend in [Backend::Bwrap, Backend::Podman, Backend::Docker] {
        let plan = deploy_plan(&git, &Placement::Local, backend, &[], &[]);
        assert_eq!(
            plan,
            DeployKeyPlan::Grant {
                mount: Some(Mount {
                    host: "/keys/deploy".into(),
                    dest: "/run/thegn-deploy-key".into(),
                    ro: true,
                    cache: false,
                }),
                ssh_command: "ssh -i /run/thegn-deploy-key -o IdentitiesOnly=yes".into(),
            },
            "{backend:?}"
        );
    }
    // systemd shares the host tree: a path-preserving read-only mount.
    let DeployKeyPlan::Grant { mount, ssh_command } =
        deploy_plan(&git, &Placement::Local, Backend::Systemd, &[], &[])
    else {
        panic!("systemd grants");
    };
    let mount = mount.expect("systemd mount");
    assert!(mount.ro && mount.dest == "/keys/deploy" && mount.host == "/keys/deploy");
    assert_eq!(ssh_command, "ssh -i /keys/deploy -o IdentitiesOnly=yes");
    // bwrap with the host tree bound needs no extra mount.
    let mut cx = deploy_cx(&Placement::Local, Backend::Bwrap, &[], &[]);
    cx.file_access = FileAccess::Host;
    assert_eq!(
        plan_deploy_key(&git, &cx, &good_key_facts),
        DeployKeyPlan::Grant {
            mount: None,
            ssh_command: "ssh -i /keys/deploy -o IdentitiesOnly=yes".into(),
        }
    );
}

#[test]
fn deploy_key_plan_off_unless_full_deploy_key() {
    let mut git = deploy_git("/keys/deploy");
    git.mode = crate::config::SandboxGitMode::PushLane;
    assert_eq!(
        deploy_plan(&git, &Placement::Local, Backend::Bwrap, &[], &[]),
        DeployKeyPlan::Off
    );
    let mut git = deploy_git("/keys/deploy");
    git.credential = crate::config::GitCredential::RepoToken;
    assert_eq!(
        deploy_plan(&git, &Placement::Local, Backend::Bwrap, &[], &[]),
        DeployKeyPlan::Off
    );
}

fn withheld_reason(plan: DeployKeyPlan) -> String {
    match plan {
        DeployKeyPlan::Withheld(reason) => reason,
        other => panic!("expected withheld, got {other:?}"),
    }
}

#[test]
fn deploy_key_plan_withholds_and_says_why() {
    let git = deploy_git("/keys/deploy");
    let local = Placement::Local;
    let writable = [Mount {
        host: "/keys".into(),
        dest: "/keys".into(),
        ro: false,
        cache: false,
    }];
    let run = [Mount {
        host: "/run".into(),
        dest: "/run".into(),
        ro: true,
        cache: false,
    }];
    let user_ssh = [("GIT_SSH_COMMAND".to_string(), "ssh -i /mine".to_string())];
    let cases: Vec<(DeployKeyPlan, &str)> = vec![
        (
            deploy_plan(&deploy_git(""), &local, Backend::Bwrap, &[], &[]),
            "deploy_key is empty",
        ),
        (
            deploy_plan(&git, &remote_placement(), Backend::Podman, &[], &[]),
            "local-only",
        ),
        (
            deploy_plan(&git, &local, Backend::Bwrap, &[], &user_ssh),
            "GIT_SSH_COMMAND from [sandbox] env_passthrough wins",
        ),
        (
            deploy_plan(
                &deploy_git("/keys/my key"),
                &local,
                Backend::Podman,
                &[],
                &[],
            ),
            "the key path contains whitespace",
        ),
        (
            deploy_plan(&git, &local, Backend::Apple, &[], &[]),
            "not supported",
        ),
        (
            deploy_plan(&git, &local, Backend::Bwrap, &writable, &[]),
            "inside a writable sandbox mount",
        ),
        (
            deploy_plan(&git, &local, Backend::Podman, &run, &[]),
            "covers /run/thegn-deploy-key",
        ),
        (
            plan_deploy_key(&git, &deploy_cx(&local, Backend::Bwrap, &[], &[]), &|_| {
                None
            }),
            "does not exist",
        ),
    ];
    for (plan, want) in cases {
        let reason = withheld_reason(plan);
        assert!(reason.contains(want), "{want}: {reason}");
    }
    // A sealed tier keeps its credential deny list: a key under ~/.ssh stays out.
    let mut sealed = deploy_cx(&local, Backend::Bwrap, &[], &[]);
    sealed.seal_home = Some("/sealed-home");
    let reason = withheld_reason(plan_deploy_key(
        &deploy_git("/sealed-home/.ssh/deploy"),
        &sealed,
        &good_key_facts,
    ));
    assert!(reason.contains("sealed tier"), "{reason}");
    assert!(matches!(
        plan_deploy_key(&deploy_git("/srv/keys/deploy"), &sealed, &good_key_facts),
        DeployKeyPlan::Grant { .. }
    ));
}

#[test]
fn deploy_key_plan_probes_the_real_key_file() {
    let temp = tempfile::tempdir().unwrap();
    let key = temp.path().join("deploy");
    std::fs::write(&key, b"dummy key bytes\n").unwrap();
    let local = Placement::Local;
    let plan_for = |path: &std::path::Path| {
        plan_deploy_key(
            &deploy_git(&path.display().to_string()),
            &deploy_cx(&local, Backend::Bwrap, &[], &[]),
            &deploy_key_facts,
        )
    };
    if crate::fsperm::set_mode_for_test(&key, 0o600).is_ok() {
        assert!(matches!(plan_for(&key), DeployKeyPlan::Grant { .. }));
        crate::fsperm::set_mode_for_test(&key, 0o644).unwrap();
        assert!(withheld_reason(plan_for(&key)).contains("group/world"));
        crate::fsperm::set_mode_for_test(&key, 0o600).unwrap();
    }
    assert!(withheld_reason(plan_for(&temp.path().join("missing"))).contains("does not exist"));
    assert!(withheld_reason(plan_for(temp.path())).contains("not a regular file"));
    let link = temp.path().join("link");
    if crate::fsperm::create_symlink(&key, &link, false).is_ok() {
        assert!(withheld_reason(plan_for(&link)).contains("symlink"));
    }
}

#[test]
fn deploy_key_grant_reaches_the_oci_and_bwrap_argv() {
    let _env = crate::testenv::EnvGuard::unset(&["GIT_SSH_COMMAND"]);
    let DeployKeyPlan::Grant { mount, ssh_command } = deploy_plan(
        &deploy_git("/keys/deploy"),
        &Placement::Local,
        Backend::Podman,
        &[],
        &[],
    ) else {
        panic!("grant");
    };
    let mut oci = spec(Backend::Podman);
    oci.mounts.extend(mount.clone());
    oci.env
        .push(("GIT_SSH_COMMAND".into(), ssh_command.clone()));
    let argv = oci_create_opts_in(&oci, &[], None).unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["-v", "/keys/deploy:/run/thegn-deploy-key:ro"]),
        "{argv:?}"
    );
    assert!(
        argv.windows(2).any(|pair| pair
            == [
                "-e",
                "GIT_SSH_COMMAND=ssh -i /run/thegn-deploy-key -o IdentitiesOnly=yes"
            ]),
        "{argv:?}"
    );
    let mut bwrap = spec(Backend::Bwrap);
    bwrap.mounts.extend(mount);
    bwrap.env.push(("GIT_SSH_COMMAND".into(), ssh_command));
    let argv = backend_enter_argv_with_env(&bwrap, "true", SandboxArgvEnv::default()).unwrap();
    assert!(
        argv.windows(3)
            .any(|triple| triple == ["--ro-bind", "/keys/deploy", "/run/thegn-deploy-key"]),
        "{argv:?}"
    );
    assert!(
        argv.iter()
            .any(|arg| arg == "ssh -i /run/thegn-deploy-key -o IdentitiesOnly=yes"),
        "{argv:?}"
    );
}

#[test]
fn runtime_escape_bwrap_adds_nothing_without_a_runtime_dir_mount() {
    let (_temp, runtime, _) = runtime_escape_fixture();
    let sandbox = spec(Backend::Bwrap);
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some(runtime.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    let rt = runtime.display().to_string();
    assert!(
        !argv.iter().any(|arg| arg.starts_with(&rt)),
        "no mask without an exposing bind: {argv:?}"
    );
    assert!(
        !argv
            .windows(2)
            .any(|pair| pair == ["--unsetenv", "THEGN_CONTROL_SOCKET"]),
        "{argv:?}"
    );
}

#[test]
fn canonical_mount_sources_preserve_visible_destinations_for_every_backend_family() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let td = tempfile::tempdir().unwrap();
        let target = td.path().join("real-provider-cache");
        let alias = td.path().join("provider-cache-alias");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &alias).unwrap();
        let canonical_target = std::fs::canonicalize(&target).unwrap();
        for backend in [Backend::Bwrap, Backend::Systemd, Backend::Docker] {
            let mut candidate = spec(backend);
            candidate.mounts.push(Mount {
                host: alias.to_string_lossy().into_owned(),
                dest: "/work/cache-through-alias".into(),
                ro: false,
                cache: false,
            });
            let planned = launchable(&candidate).unwrap();
            let mount = planned
                .mounts
                .iter()
                .find(|mount| mount.dest == "/work/cache-through-alias")
                .unwrap();
            assert_eq!(mount.host, canonical_target.to_string_lossy().as_ref());
            assert_eq!(mount.dest, "/work/cache-through-alias");
        }
        let mut remote = spec(Backend::Bwrap);
        remote.placement = crate::placement::Placement::Ssh(crate::placement::SshPlacement::plain(
            "example.test".into(),
            22,
            false,
            crate::placement::TransportKind::Ssh,
        ));
        remote.mounts.push(Mount {
            host: alias.to_string_lossy().into_owned(),
            dest: "/remote/cache".into(),
            ro: false,
            cache: false,
        });
        let planned = launchable(&remote).unwrap();
        let mount = planned
            .mounts
            .iter()
            .find(|mount| mount.dest == "/remote/cache")
            .unwrap();
        assert_eq!(mount.host, alias.to_string_lossy().as_ref());
    }
}

pub fn pull_image(img: &str) -> anyhow::Result<()> {
    if sandbox_backend_forced_none() {
        return Ok(());
    }
    let _ = std::process::Command::new("podman") // best-effort: test scaffolding: pull failures are ignored (the image may be cached)
        .args(["pull", img])
        .output();
    Ok(())
}

fn sandbox_backend_forced_none() -> bool {
    std::env::var("THEGN_SANDBOX_BACKEND")
        .is_ok_and(|backend| matches!(backend.trim(), "none" | "host"))
}

#[test]
fn integration_test_sandbox_net_and_file() {
    if sandbox_backend_forced_none() {
        return;
    }
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

    let argv = enter_argv(&s, "python3 -m http.server 8081").expect("valid volume names");

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
    let _ = child.kill(); // best-effort: test cleanup: kill the webserver child
    let _ = child.wait(); // best-effort: test cleanup: kill the webserver child
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
    if sandbox_backend_forced_none() {
        return;
    }
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
    let argv = enter_argv(&s, "${SHELL:-/bin/sh} -l").expect("valid volume names");
    assert_eq!(
        argv[4], "mosh",
        "mosh argv rides the fallback wrapper: {argv:?}"
    );
    let body = argv.last().unwrap();
    assert!(body.contains("cd /wt/feat"));
    assert!(body.contains("exec ${SHELL:-/bin/sh} -l"));
}

#[test]
fn devenv_wraps_inner() {
    let mut s = spec(Backend::Bwrap);
    s.image = None;
    s.devenv = true;
    let argv = enter_argv(&s, "claude").expect("valid volume names");
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
        // The premise is "$HOME is already bind-mounted". Assert that premise
        // rather than assuming it: bwrap is Linux-only, so on another host this
        // resolves to whatever the chain finds (an OCI backend when a runtime is
        // running, the host shell otherwise), and a guest with a different ABI
        // deliberately gets no host-toolchain substrate at all — so there is no
        // parent bind for `~/.gitconfig` to be covered by, and nothing to test.
        let home_bound = spec.mounts.iter().any(|m| m.host == home);
        if !home_bound {
            return;
        }
        let gitconfig = format!("{home}/.gitconfig");
        let has_gitconfig_mount = spec.mounts.iter().any(|m| m.host == gitconfig);
        assert!(
            !has_gitconfig_mount,
            "~/.gitconfig should be excluded — $HOME is already bind-mounted"
        );
    }
}

#[test]
fn resolve_mounts_keeps_existing_and_skips_missing_sources() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let existing = temp.path().to_string_lossy().into_owned();
    let missing = temp.path().join("missing").to_string_lossy().into_owned();
    let loc = crate::remote::GitLoc::from_db("/wt/x", None);
    let cfg = crate::config::SandboxConfig {
        backend: crate::config::SandboxBackend::Bwrap,
        backend_chain: vec!["bwrap".into()],
        mounts: vec![existing.clone()],
        ..Default::default()
    };
    let Some(resolved) = resolve(&cfg, &loc, "test") else {
        return;
    };
    assert!(
        resolved.mounts.iter().any(|mount| mount.host == existing),
        "an existing configured mount source must reach the resolved spec"
    );

    let missing_cfg = crate::config::SandboxConfig {
        mounts: vec![missing.clone()],
        ..cfg
    };
    let Some(resolved_missing) = resolve(&missing_cfg, &loc, "test") else {
        return;
    };
    assert!(
        !resolved_missing
            .mounts
            .iter()
            .any(|mount| mount.host == missing)
    );
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
        // least the nix store mount — but only where the host and the container
        // guest share an ABI. An OCI guest is always Linux; on a macOS host the
        // store holds Mach-O binaries, and injecting it (with /usr and /bin) is
        // what broke container creation outright. See
        // `host_toolchain_mounts_are_withheld_when_the_guest_abi_differs`.
        let same_abi = crate::sandbox_backend::host_os() == crate::sandbox_backend::HostOs::Linux;
        if same_abi && std::path::Path::new("/nix/store").exists() {
            assert!(
                toolchain.iter().any(|m| m.host == "/nix/store"),
                "OCI spec on NixOS should include /nix/store mount"
            );
        }
        if !same_abi {
            assert!(
                !toolchain.iter().any(|m| m.host == "/nix/store"),
                "a Linux guest must not get a non-Linux host's /nix/store"
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
fn oci_create_opts_carry_this_profiles_owner_label() {
    let podman = oci_create_opts(&spec(Backend::Podman)).unwrap();
    assert!(
        podman
            .windows(2)
            .any(|pair| pair[0] == "--label" && pair[1].starts_with("thegn.owner="))
    );

    let apple = oci_create_opts(&spec(Backend::Apple)).unwrap();
    assert!(!apple.iter().any(|arg| arg.starts_with("thegn.owner=")));
}

#[test]
fn oci_create_opts_stamp_worktree_and_role_labels() {
    let podman_spec = spec(Backend::Podman);
    let podman = oci_create_opts(&podman_spec).unwrap();
    assert!(
        podman
            .windows(2)
            .any(|pair| { pair[0] == "--label" && pair[1] == "thegn.role=main" })
    );
    assert!(podman.windows(2).any(|pair| {
        pair[0] == "--label"
            && pair[1]
                == format!(
                    "thegn.worktree={}",
                    crate::sandbox_manage::worktree_label_value(
                        podman_spec.worktree.as_os_str().as_encoded_bytes()
                    )
                )
    }));

    let apple = oci_create_opts(&spec(Backend::Apple)).unwrap();
    assert!(
        !apple
            .iter()
            .any(|arg| { arg.starts_with("thegn.role=") || arg.starts_with("thegn.worktree=") })
    );
}

#[test]
fn test_run_gc_noop_when_no_backend_available() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[("XDG_STATE_HOME", Some(&state_home))]);
    // run_gc with an empty DB set and no containers should return empty
    // without panicking (even if podman/docker aren't installed).
    let removed = run_gc(&["/wt/alive".to_string()]);
    // On CI there may be no podman — the result is just an empty list.
    assert!(removed.iter().all(|n| n.starts_with(CONTAINER_PREFIX)));
}

#[test]
fn gc_sweeps_every_oci_backend_that_can_hold_a_container() {
    use crate::sandbox::{Backend, gc_list_argv};
    // The leak this fixes: the sweep used a hardcoded [Podman, Docker, Smol], so
    // rootful-podman containers (a SEPARATE store the rootless pass never sees)
    // leaked on Linux and `apple` containers leaked on macOS — where each one
    // also pins its own Linux VM. Anything that holds containers must be swept.
    for b in Backend::all_oci() {
        assert!(b.is_oci(), "all_oci must not drift from is_oci: {b:?}");
        if b.is_reserved() {
            // Deliberately unswept: a reserved kind (wsl, smol) is never
            // created, its command shape is unverified, and this feeds a
            // force-remove, where a wrong guess deletes real containers.
            assert_eq!(gc_list_argv(b), None, "{b:?}");
            continue;
        }
        assert!(
            gc_list_argv(b).is_some(),
            "{b:?} holds containers, so the GC must know how to list them"
        );
    }
    assert!(
        gc_list_argv(Backend::PodmanRootful).is_some(),
        "rootful podman was the silent Linux leak"
    );
    // Non-OCI backends have nothing to sweep.
    assert_eq!(gc_list_argv(Backend::Bwrap), None);
    assert_eq!(gc_list_argv(Backend::None), None);
}

#[test]
fn container_list_parsing_is_fail_closed_per_backend() {
    use crate::sandbox::{Backend, parse_container_list};
    // Docker/podman: one name per line.
    assert_eq!(
        parse_container_list(Backend::Docker, "thegn-a\nthegn-b\n\n"),
        vec!["thegn-a".to_string(), "thegn-b".to_string()],
        "blank lines must not become an empty container name"
    );
    // Apple: JSON array whose entries carry a top-level `id`. `container run
    // --name X` "uses the specified name as the container ID", so thegn's
    // existing thegn-{slug} scheme lands there unchanged.
    let json = r#"[{"id":"thegn-alpha","status":"running"},{"id":"thegn-beta"}]"#;
    assert_eq!(
        parse_container_list(Backend::Apple, json),
        vec!["thegn-alpha".to_string(), "thegn-beta".to_string()]
    );
    assert!(parse_container_list(Backend::Apple, "[]").is_empty());
    // FAIL CLOSED: this list is force-removed, so output we cannot parse must
    // delete nothing. Never everything.
    for junk in ["", "not json", "{\"id\":\"x\"}", "[{\"no_id\":1}]"] {
        assert!(
            parse_container_list(Backend::Apple, junk).is_empty(),
            "unparseable apple output must yield no removals: {junk:?}"
        );
    }
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
    let argv = enter_argv(&s, "bash -l").expect("valid volume names");
    assert_eq!(argv[0], "/bin/sh", "local fallback wrapper: {argv:?}");
    assert_eq!(argv[4], "mosh", "mosh argv rides the wrapper: {argv:?}");
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
    let argv = enter_argv(&s, "bash -l").expect("valid volume names");
    // SSH transport: first arg is ssh, not mosh.
    assert_eq!(argv[0], "ssh", "outer command must be ssh: {argv:?}");
    assert!(argv.iter().any(|a| a == "devbox"), "host missing: {argv:?}");
    // Port flag must be present when non-default.
    assert!(
        argv.iter().any(|a| a == "-p"),
        "port flag missing: {argv:?}"
    );
}

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
fn config_profile_container_name_matches_create_site_normalization() {
    assert_eq!(
        container_name_for_config_profile("/wt/feat", " vim "),
        container_name_with_profile("/wt/feat", Some("vim"))
    );
    assert_eq!(
        container_name_for_config_profile("/wt/feat", "   "),
        container_name("/wt/feat")
    );
}

#[test]
fn owned_containers_need_the_exact_profile_name() {
    let worktree = OwnedContainers::new("default", [("/wt/feat", Some("podman"))]);
    let plain = container_name("/wt/feat");
    assert!(worktree.owns(&plain, "podman"));
    assert!(!worktree.owns("thegn-foo", "podman"));
    assert!(!worktree.owns(
        &container_name_with_profile("/wt/feat", Some("work")),
        "podman"
    ));
    assert!(worktree.owns(&vpn_sidecar_name(&plain), "podman"));
    assert!(worktree.owns(&agent_container_name(&plain), "podman"));

    let named_profile = OwnedContainers::new("work", [("/wt/feat", Some("podman"))]);
    assert!(named_profile.owns(
        &container_name_with_profile("/wt/feat", Some("work")),
        "podman"
    ));
    assert!(!named_profile.owns(&plain, "podman"));
}

#[test]
fn owned_containers_bind_the_backend_and_fail_closed() {
    let rootless = OwnedContainers::new("default", [("/wt/feat", Some("podman"))]);
    let name = container_name("/wt/feat");
    assert!(rootless.owns(&name, "podman"));
    assert!(!rootless.owns(&name, "podman-rootful"));
    assert!(!rootless.owns(&name, "docker"));
    assert!(!rootless.owns(&name, "unknown-runtime"));

    let rootful = OwnedContainers::new("default", [("/wt/feat", Some("podman-rootful"))]);
    assert!(rootful.owns(&name, "podman-rootful"));
    assert!(!rootful.owns(&name, "podman"));

    for observed in [None, Some("bwrap"), Some("host"), Some("future-runtime")] {
        let unproven = OwnedContainers::new("default", [("/wt/feat", observed)]);
        assert!(!unproven.owns(&name, "podman"));
    }
}

#[test]
fn stamp_only_lowers_ours() {
    let own = container_name("/wt/feat");
    let mut rows = parse_podman_ps(&format!(
        r#"[
          {{"Names": ["thegn-foo"], "Image": "alpine", "Status": "Up"}},
          {{"Names": ["{own}"], "Image": "alpine", "Status": "Up"}},
          {{"Names": ["registry"], "Image": "registry:2", "Status": "Up"}}
        ]"#
    ));
    rows.push(ContainerInfo {
        image: "hand-built foreign".into(),
        ours: false,
        ..container_info(own.clone(), "alpine".into(), "Up".into(), "podman")
    });
    let owned = OwnedContainers::new("default", [("/wt/feat", Some("podman"))]);
    owned.stamp(&mut rows);

    assert_eq!(rows[0].name, own);
    assert!(rows[0].ours);
    let registry = rows.iter().find(|row| row.name == "registry").unwrap();
    assert!(!registry.ours);
    let prefix_collision = rows.iter().find(|row| row.name == "thegn-foo").unwrap();
    assert!(!prefix_collision.ours);
    let preexisting_foreign = rows
        .iter()
        .find(|row| row.image == "hand-built foreign")
        .unwrap();
    assert!(!preexisting_foreign.ours);
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
    let j = oci_create_opts(&s).expect("valid volume names").join(" ");
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
    let j = oci_create_opts(&s).expect("valid volume names").join(" ");
    assert!(!j.contains("--read-only"), "{j}");
    assert!(!j.contains("--cap-drop"), "{j}");
    assert!(!j.contains("--security-opt"), "{j}");
    assert!(!j.contains("--pids-limit"), "{j}");
}

#[test]
fn vpn_sidecar_name_roundtrips() {
    let base = container_name("/wt/feat");
    let vpn = vpn_sidecar_name(&base);
    assert_eq!(vpn, format!("{base}-tgvpn"));
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
    // (`thegn-{profile}-{slug}`) or a VPN sidecar (`-tgvpn`) must never be
    // reaped by run_gc. The old exact-name allow-list only knew the plain and
    // `-tgagent` forms and force-removed these live containers.
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
fn identify_orphans_keeps_a_live_worktree_whose_slug_ends_in_a_companion_suffix() {
    let active = vec!["/wt/feat-tgvpn".to_string(), "/wt/fix-tgagent".to_string()];
    let feat = container_name("/wt/feat-tgvpn");
    let containers = vec![
        feat.clone(),
        container_name("/wt/fix-tgagent"),
        container_name_with_profile("/wt/feat-tgvpn", Some("sealed")),
        vpn_sidecar_name(&feat),
        container_name("/wt/dead"),
    ];

    assert_eq!(
        identify_orphans(&active, &containers),
        vec![container_name("/wt/dead")]
    );
}

#[test]
fn identify_orphans_overkeeps_a_main_container_that_aliases_a_companion() {
    // Positive control: this name is already kept at the base as the live
    // worktree's VPN companion; the raw-name pass preserves that decision.
    let active = vec!["/wt/feat".to_string()];
    let containers = vec![container_name("/wt/feat-tgvpn")];

    assert!(identify_orphans(&active, &containers).is_empty());
}

#[test]
fn identify_orphans_spares_compose_service_containers_of_live_worktrees() {
    let active = vec!["/w/my.repo".to_string(), "/w/my-repo".to_string()];
    let first = container_name("/w/my.repo");
    let first_hashed =
        crate::sandbox_compose::hashed_project_name(&first, std::path::Path::new("/w/my.repo"));
    let first_profile_hashed = crate::sandbox_compose::hashed_project_name(
        &container_name_with_profile("/w/my.repo", Some("sealed")),
        std::path::Path::new("/w/my.repo"),
    );
    let second_hashed = crate::sandbox_compose::hashed_project_name(
        &container_name("/w/my-repo"),
        std::path::Path::new("/w/my-repo"),
    );
    let dead_hashed = crate::sandbox_compose::hashed_project_name(
        &container_name("/w/dead"),
        std::path::Path::new("/w/dead"),
    );
    let containers = vec![
        format!("{first_hashed}-dev-1"),
        format!("{first_hashed}_dev_1"),
        format!("{first_profile_hashed}-db-2"),
        format!("{second_hashed}_api_1"),
        format!("{dead_hashed}-dev-1"),
    ];
    let orphans = identify_orphans(&active, &containers);
    assert_eq!(orphans, vec![format!("{dead_hashed}-dev-1")]);
}

#[test]
fn compose_down_argv_uses_the_selected_oci_context() {
    use crate::sandbox_compose::down_argv_for;

    assert_eq!(
        down_argv_for(Backend::Docker, None, "thegn-wt-feat").unwrap(),
        [
            "docker",
            "compose",
            "-p",
            "thegn-wt-feat",
            "down",
            "--remove-orphans"
        ]
    );
    assert_eq!(
        down_argv_for(Backend::PodmanRootful, None, "proj").unwrap(),
        [
            "sudo",
            "-n",
            "podman",
            "compose",
            "-p",
            "proj",
            "down",
            "--remove-orphans"
        ]
    );
    assert_eq!(
        down_argv_for(Backend::Podman, None, "proj").unwrap()[..2],
        ["podman", "compose"]
    );
    let remote = down_argv_for(Backend::Docker, Some("ssh://user@host"), "proj").unwrap();
    let prefix = oci_prefix_for(Backend::Docker, Some("ssh://user@host"));
    assert_eq!(&remote[..prefix.len()], prefix.as_slice());
    for backend in [Backend::Apple, Backend::Smol, Backend::Bwrap] {
        assert_eq!(down_argv_for(backend, None, "proj"), None);
    }
}

#[test]
fn teardown_argvs_include_compose_project_down() {
    let name = container_name("/wt/feat");
    let hashed =
        crate::sandbox_compose::hashed_project_name(&name, std::path::Path::new("/wt/feat"));
    let commands = teardown_argvs(Backend::Docker, None, &name, std::slice::from_ref(&hashed));
    assert!(commands.contains(&vec![
        "docker".into(),
        "compose".into(),
        "-p".into(),
        hashed.clone(),
        "down".into(),
        "--remove-orphans".into(),
    ]));
    assert_eq!(
        commands
            .iter()
            .filter(|argv| argv.contains(&"down".into()))
            .count(),
        1
    );
    let adopted = teardown_argvs(Backend::Docker, None, &name, &[hashed, name.clone()]);
    assert_eq!(
        adopted
            .iter()
            .filter(|argv| argv.contains(&"down".into()))
            .count(),
        2
    );
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
fn wrap_script_does_not_double_exec_a_pre_execd_simple_inner() {
    let mut s = spec(Backend::Bwrap);
    s.image = None;

    let out = wrap_script(&s, "exec zsh -l");
    let last_line = out.lines().last().unwrap();
    assert!(last_line.ends_with("exec zsh -l"), "{last_line}");
    assert!(!last_line.contains("exec exec"), "{last_line}");
    assert_eq!(last_line.matches("exec ").count(), 1, "{last_line}");

    let compound = wrap_script(&s, "exec zsh -l; true");
    assert!(compound.ends_with("exec zsh -l; true"), "{compound}");

    s.devenv = true;
    s.devenv_path = Some("/nix/store/devenv/bin/devenv".into());
    let devenv = wrap_script(&s, "exec zsh -l");
    let devenv_line = devenv.lines().last().unwrap();
    assert_eq!(
        devenv_line,
        "exec /nix/store/devenv/bin/devenv shell -- zsh -l"
    );
    assert!(!devenv_line.contains("-- exec "), "{devenv_line}");
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
    // Also clear ambient THEGN_SANDBOX (a live thegn shell sets it) — these
    // tests use it as a SYNTHETIC inline pair, so its ambient presence would
    // divert it to the env-file and break the assertion. Hermetic under nextest
    // AND in-process `cargo test`.
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("GH_TOKEN", Some("ghp_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Podman);
    s.name = "thegn-test-envfile-oci".into();
    s.env = vec![
        ("GH_TOKEN".into(), "ghp_secret".into()), // matches host ⇒ secret
        ("THEGN_SANDBOX".into(), "1".into()),     // synthetic ⇒ inline
    ];
    let opts = oci_create_opts(&s).expect("valid volume names");
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
    let mode = crate::fsperm::mode_bits(&path).unwrap();
    if cfg!(unix) {
        assert_eq!(mode, Some(0o600), "env-file must be 0600");
    }
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
    let _ = std::fs::remove_file(&path); // best-effort: test cleanup: scratch removal must never fail the test
}

#[test]
fn systemd_local_secrets_go_to_environment_file_not_argv() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("sk_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Systemd);
    s.image = None;
    s.name = "thegn-test-envfile-systemd".into();
    s.env = vec![
        ("API_KEY".into(), "sk_secret".into()), // secret
        ("THEGN_SANDBOX".into(), "1".into()),   // synthetic
    ];
    let argv = backend_enter_argv(&s, "exec true").unwrap();
    let argv_again = backend_enter_argv(&s, "exec true").unwrap();
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
    let ef_again = argv_again
        .iter()
        .find(|a| a.starts_with("EnvironmentFile="))
        .expect("second EnvironmentFile property");
    let path_again = std::path::PathBuf::from(ef_again.trim_start_matches("EnvironmentFile="));
    assert_ne!(path, path_again, "systemd launches need isolated env-files");
    let rm = crate::util::which_path("rm").expect("rm on PATH for cleanup assertion");
    let cleanup = format!("ExecStopPost=-+{rm} -f -- {}", path.display());
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "-p" && pair[1] == cleanup)
    );
    assert!(!argv.windows(2).any(|pair| {
        pair[0] == "-p" && pair[1].starts_with("ExecStopPost=") && pair[1] != cleanup
    }));
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("API_KEY=sk_secret"));
    // Synthetic pair stays on --setenv.
    assert!(
        j.contains("--setenv THEGN_SANDBOX=1"),
        "synthetic --setenv: {j}"
    );
    let _ = std::fs::remove_file(&path); // best-effort: test cleanup: scratch removal must never fail the test
    let _ = std::fs::remove_dir_all(&state); // best-effort test cleanup
}

#[cfg(unix)]
#[test]
fn systemd_env_file_failure_reports_omitted_secret_keys() {
    let state_home = private_tempdir();
    let state_home_path = state_home.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("sk_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home_path)),
    ]);
    let thegn_dir = state_home.path().join("thegn");
    std::fs::create_dir(&thegn_dir).unwrap();
    let target = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(target.path(), thegn_dir.join("sandbox-env")).unwrap();

    let mut sandbox = spec(Backend::Systemd);
    sandbox.image = None;
    sandbox.name = "thegn-test-envfile-failure".into();
    sandbox.env = vec![("API_KEY".into(), "sk_secret".into())];
    let entered =
        backend_enter_argv_guarded_with_env(&sandbox, "exec true", SandboxArgvEnv::default())
            .unwrap();
    let argv = entered.argv().join(" ");

    assert!(
        !argv.contains("EnvironmentFile="),
        "unexpected env file: {argv}"
    );
    assert!(!argv.contains("sk_secret"), "secret leaked in argv: {argv}");
    let warning = entered
        .omitted_secrets_warning()
        .expect("omitted secret warning");
    assert!(
        warning.contains("API_KEY"),
        "warning omitted key: {warning}"
    );
    assert!(
        !warning.contains("sk_secret"),
        "warning leaked value: {warning}"
    );
}

#[test]
fn systemd_env_file_success_reports_nothing_omitted() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("sk_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut sandbox = spec(Backend::Systemd);
    sandbox.image = None;
    sandbox.name = "thegn-test-envfile-success".into();
    sandbox.env = vec![("API_KEY".into(), "sk_secret".into())];

    let entered =
        backend_enter_argv_guarded_with_env(&sandbox, "exec true", SandboxArgvEnv::default())
            .unwrap();
    assert!(
        entered
            .argv()
            .iter()
            .any(|arg| arg.starts_with("EnvironmentFile=")),
        "successful secret env file missing from argv: {:?}",
        entered.argv()
    );
    assert!(entered.omitted_secrets_warning().is_none());
}

#[cfg(unix)]
#[test]
fn omitted_secrets_warning_names_keys_never_values() {
    let state_home = private_tempdir();
    let state_home_path = state_home.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("api_secret")),
        ("GH_TOKEN", Some("gh_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home_path)),
    ]);
    let thegn_dir = state_home.path().join("thegn");
    std::fs::create_dir(&thegn_dir).unwrap();
    let target = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(target.path(), thegn_dir.join("sandbox-env")).unwrap();

    let mut sandbox = spec(Backend::Systemd);
    sandbox.image = None;
    sandbox.name = "thegn-test-envfile-multiple".into();
    sandbox.env = vec![
        ("GH_TOKEN".into(), "gh_secret".into()),
        ("API_KEY".into(), "api_secret".into()),
    ];
    let entered =
        backend_enter_argv_guarded_with_env(&sandbox, "exec true", SandboxArgvEnv::default())
            .unwrap();

    let warning = entered
        .omitted_secrets_warning()
        .expect("omitted secret warning");
    assert!(
        warning.contains("API_KEY, GH_TOKEN"),
        "keys not sorted: {warning}"
    );
    assert!(
        !warning.contains("api_secret"),
        "warning leaked API value: {warning}"
    );
    assert!(
        !warning.contains("gh_secret"),
        "warning leaked GH value: {warning}"
    );
}

#[test]
fn systemd_cleanup_property_rejects_systemd_expansion_and_quoting_chars() {
    let path = std::path::Path::new("/state/thegn/sandbox-env/name-1.env");
    assert_eq!(
        systemd_env_cleanup_prop("/nix/store/coreutils/bin/rm", path).as_deref(),
        Some(
            "ExecStopPost=-+/nix/store/coreutils/bin/rm -f -- /state/thegn/sandbox-env/name-1.env"
        )
    );
    for unsafe_value in [
        "/bin/rm x",
        "/bin/rm%f",
        "/bin/rm$HOME",
        "/bin/rm;true",
        "/bin/rm\\x",
        "/bin/rm\"x",
        "/bin/rm'x",
    ] {
        assert!(systemd_env_cleanup_prop(unsafe_value, path).is_none());
    }
    assert!(systemd_env_cleanup_prop("bin/rm", path).is_none());
    assert!(systemd_env_cleanup_prop("/bin/rm", std::path::Path::new("state/name.env")).is_none());
    assert_eq!(
        systemd_env_cleanup_prop("/bin/rm", std::path::Path::new("/state/a b.env")).as_deref(),
        Some("ExecStopPost=-+/bin/rm -f -- \"/state/a b.env\"")
    );
}

#[test]
fn systemd_cleanup_property_quotes_and_escapes_paths() {
    for (path, expected) in [
        (
            "/state/it's.env",
            "ExecStopPost=-+/bin/rm -f -- \"/state/it's.env\"",
        ),
        (
            "/state/q\"x.env",
            "ExecStopPost=-+/bin/rm -f -- \"/state/q\\\"x.env\"",
        ),
        (
            "/state/b\\x.env",
            "ExecStopPost=-+/bin/rm -f -- \"/state/b\\\\x.env\"",
        ),
    ] {
        assert_eq!(
            systemd_env_cleanup_prop("/bin/rm", std::path::Path::new(path)).as_deref(),
            Some(expected),
            "path: {path}"
        );
    }
    for path in [
        "/state/$HOME.env",
        "/state/100%.env",
        "/state/a;b.env",
        "/state/line\nbreak.env",
    ] {
        assert!(
            systemd_env_cleanup_prop("/bin/rm", std::path::Path::new(path)).is_none(),
            "unsafe path accepted: {path:?}"
        );
    }
}

#[test]
fn systemd_cleanup_refusal_removes_env_file() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("st%ate");
    std::fs::create_dir(&state).unwrap();
    let state_home = state.to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("sk_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Systemd);
    s.image = None;
    s.name = "thegn-test-envfile-systemd-refusal".into();
    s.env = vec![("API_KEY".into(), "sk_secret".into())];

    assert!(matches!(
        backend_enter_argv(&s, "exec true"),
        Err(EnterError::Refused(r)) if r.class == RefusalClass::SystemdMountPath
    ));
    let env_dir = state.join("thegn/sandbox-env");
    let entries = std::fs::read_dir(env_dir)
        .unwrap()
        .collect::<std::io::Result<Vec<_>>>()
        .unwrap();
    assert!(
        entries.iter().all(|entry| entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "env")),
        "refused launch left a secret env file behind"
    );
}

#[test]
fn secret_env_guard_drop_unlinks_unspawned_systemd_env_file() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("sk_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Systemd);
    s.image = None;
    s.name = "thegn-test-envfile-guard-drop".into();
    s.env = vec![("API_KEY".into(), "sk_secret".into())];

    let guarded = backend_enter_argv_guarded(&s, "exec true").unwrap();
    let property = guarded
        .argv()
        .iter()
        .find(|arg| arg.starts_with("EnvironmentFile="))
        .expect("EnvironmentFile property")
        .clone();
    let path = std::path::PathBuf::from(property.trim_start_matches("EnvironmentFile="));
    assert!(path.is_file());
    drop(guarded);
    assert!(!path.exists());
}

#[test]
fn secret_env_guard_into_spawned_keeps_env_file() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("sk_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Systemd);
    s.image = None;
    s.name = "thegn-test-envfile-guard-spawned".into();
    s.env = vec![("API_KEY".into(), "sk_secret".into())];

    let guarded = backend_enter_argv_guarded(&s, "exec true").unwrap();
    let property = guarded
        .argv()
        .iter()
        .find(|arg| arg.starts_with("EnvironmentFile="))
        .expect("EnvironmentFile property")
        .clone();
    let path = std::path::PathBuf::from(property.trim_start_matches("EnvironmentFile="));
    assert!(path.is_file());
    let argv = guarded.into_spawned();
    assert!(argv.iter().any(|arg| arg == &property));
    assert!(path.is_file());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn secret_env_guard_never_unlinks_outside_the_env_dir() {
    let parent = tempfile::tempdir().unwrap();
    let other_parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("sandbox-env");
    std::fs::create_dir(&dir).unwrap();
    let outside = other_parent.path().join("outside.env");
    let inside_non_env = dir.join("keep.txt");
    std::fs::write(&outside, b"secret").unwrap();
    std::fs::write(&inside_non_env, b"secret").unwrap();

    drop(SecretEnvFile::adopt(dir.clone(), outside.clone()));
    drop(SecretEnvFile::adopt(dir, inside_non_env.clone()));
    assert!(outside.is_file());
    assert!(inside_non_env.is_file());
}

#[test]
fn secret_env_guard_absent_without_host_secrets() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("host-secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Systemd);
    s.image = None;
    s.name = "thegn-test-envfile-guard-absent".into();
    s.env = vec![("API_KEY".into(), "synthetic-value".into())];

    let guarded = backend_enter_argv_guarded(&s, "exec true").unwrap();
    let (argv, guard) = guarded.into_parts();
    assert!(guard.is_none());
    assert!(argv.iter().all(|arg| !arg.starts_with("EnvironmentFile=")));
}

#[test]
fn stale_env_file_sweep_removes_only_old_env_entries_without_following_links() {
    use std::time::{Duration, SystemTime};
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("sandbox-env");
    std::fs::create_dir(&dir).unwrap();
    let old = dir.join("old.env");
    let fresh = dir.join("fresh.env");
    let other = dir.join("keep.txt");
    let nested = dir.join("x.env");
    std::fs::write(&old, b"old").unwrap();
    std::fs::write(&fresh, b"fresh").unwrap();
    std::fs::write(&other, b"other").unwrap();
    std::fs::create_dir(&nested).unwrap();
    let now = SystemTime::now();
    std::fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(now - Duration::from_secs(120))
        .unwrap();
    assert_eq!(
        sweep_stale_env_files_in(
            parent.path(),
            std::path::Path::new("sandbox-env"),
            now,
            Duration::from_secs(60)
        ),
        1
    );
    assert!(!old.exists());
    assert!(fresh.exists());
    assert!(other.exists());
    assert!(nested.is_dir());
}

#[cfg(unix)]
#[test]
fn stale_env_file_sweep_unlinks_symlink_but_preserves_target() {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("sandbox-env");
    std::fs::create_dir(&dir).unwrap();
    let victim = parent.path().join("victim");
    let link = dir.join("hostile.env");
    std::fs::write(&victim, b"canary").unwrap();
    symlink(&victim, &link).unwrap();
    assert_eq!(
        sweep_stale_env_files_in(
            parent.path(),
            std::path::Path::new("sandbox-env"),
            SystemTime::now(),
            Duration::from_secs(1)
        ),
        1
    );
    assert!(!link.exists());
    assert_eq!(std::fs::read(victim).unwrap(), b"canary");
}

#[cfg(unix)]
#[test]
fn stale_env_file_sweep_does_not_traverse_symlinked_directory() {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};
    let parent = tempfile::tempdir().unwrap();
    let outside = parent.path().join("outside");
    let link = parent.path().join("sandbox-env");
    std::fs::create_dir(&outside).unwrap();
    let old = outside.join("old.env");
    std::fs::write(&old, b"keep").unwrap();
    symlink(&outside, &link).unwrap();
    assert_eq!(
        sweep_stale_env_files_in(
            parent.path(),
            std::path::Path::new("sandbox-env"),
            SystemTime::now(),
            Duration::ZERO
        ),
        0
    );
    assert!(old.exists());
}

#[test]
fn remote_oci_sends_host_secrets_on_stdin_and_keeps_synthetic_env_inline() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("GH_TOKEN", Some("ghp_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Podman);
    s.env = vec![
        ("GH_TOKEN".into(), "ghp_secret".into()),
        ("THEGN_SANDBOX".into(), "1".into()),
    ];
    s.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let secret_env = remote_stdin_oci_secret_env(&s);
    let opts = oci_create_opts_with_remote_stdin_secrets(&s, &secret_env).unwrap();
    let (argv, input) = oci_create_argv_and_stdin(&s, opts, &secret_env).unwrap();
    let j = argv.join(" ");
    assert!(!j.contains("ghp_secret"), "remote argv leaked secret: {j}");
    assert!(j.contains("--env-file /dev/stdin"), "stdin env-file: {j}");
    assert_eq!(
        j.matches("--env-file").count(),
        1,
        "one remote env-file: {j}"
    );
    assert!(
        j.contains("-e THEGN_SANDBOX=1"),
        "synthetic env inline: {j}"
    );
    let input = String::from_utf8(input.expect("SSH host secret stdin")).unwrap();
    assert_eq!(input, "GH_TOKEN=ghp_secret\n");
    let env_dir = state.path().join("thegn/sandbox-env");
    if env_dir.exists() {
        assert!(
            std::fs::read_dir(env_dir).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".env")),
            "remote secrets must not create a local env-file"
        );
    }
    assert!(
        !redact_stderr("remote rejected ghp_secret".into(), &["ghp_secret".into()])
            .contains("ghp_secret")
    );
}

#[test]
fn k8s_oci_sends_host_secrets_on_stdin_and_keeps_synthetic_env_inline() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("GH_TOKEN", Some("ghp_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Podman);
    s.env = vec![
        ("GH_TOKEN".into(), "ghp_secret".into()),
        ("THEGN_SANDBOX".into(), "1".into()),
    ];
    s.placement = Placement::K8s(crate::placement::K8sPlacement {
        kubectl: "kubectl".into(),
        context: None,
        namespace: None,
        pod: "pod".into(),
        container: Some("dev".into()),
        pod_template: None,
        image: None,
    });
    let secret_env = remote_stdin_oci_secret_env(&s);
    let opts = oci_create_opts_with_remote_stdin_secrets(&s, &secret_env).unwrap();
    let (argv, input) = oci_create_argv_and_stdin(&s, opts, &secret_env).unwrap();
    let joined = argv.join(" ");
    assert!(
        !joined.contains("ghp_secret"),
        "K8s argv leaked secret: {joined}"
    );
    assert!(
        joined.contains("--env-file /dev/stdin"),
        "stdin env-file: {joined}"
    );
    assert_eq!(
        joined.matches("--env-file").count(),
        1,
        "one env-file: {joined}"
    );
    assert!(
        joined.contains("-e THEGN_SANDBOX=1"),
        "synthetic env inline: {joined}"
    );
    assert_eq!(
        String::from_utf8(input.expect("K8s host secret stdin")).unwrap(),
        "GH_TOKEN=ghp_secret\n"
    );
}

#[test]
fn provider_oci_omits_host_secrets_without_stdin() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("GH_TOKEN", Some("ghp_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Podman);
    s.env = vec![
        ("GH_TOKEN".into(), "ghp_secret".into()),
        ("THEGN_SANDBOX".into(), "1".into()),
    ];
    s.placement = Placement::Provider(crate::placement::ProviderPlacement {
        provider: "test".into(),
        id: "sandbox".into(),
        interactive_prefix: vec![],
        control_prefix: vec!["provider".into(), "exec".into(), "--".into()],
        up_command: vec![],
        down_command: vec![],
    });

    // A provider transport has no stdin carrier, so the secret must be omitted
    // rather than inlined into the create argv.
    let secret_env = remote_stdin_oci_secret_env(&s);
    assert!(secret_env.is_empty(), "provider must not claim stdin");
    let opts = oci_create_opts_with_remote_stdin_secrets(&s, &secret_env).unwrap();
    let (argv, input) = oci_create_argv_and_stdin(&s, opts, &secret_env).unwrap();
    let joined = argv.join(" ");
    assert!(
        !joined.contains("ghp_secret"),
        "provider argv leaked secret: {joined}"
    );
    assert!(
        !joined.contains("--env-file"),
        "provider has no env-file carrier: {joined}"
    );
    assert!(
        joined.contains("-e THEGN_SANDBOX=1"),
        "synthetic env inline: {joined}"
    );
    assert!(input.is_none(), "provider must not receive a stdin payload");
    let env_dir = state.path().join("thegn/sandbox-env");
    assert!(
        !env_dir.exists(),
        "a remote provider must not create a local env-file"
    );
}

#[test]
fn remote_systemd_omits_host_secret_without_argv_or_local_file() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("API_KEY", Some("sk_secret")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut sandbox = spec(Backend::Systemd);
    sandbox.image = None;
    sandbox.name = "thegn-test-remote-systemd".into();
    sandbox.env = vec![
        ("API_KEY".into(), "sk_secret".into()),
        ("THEGN_SANDBOX".into(), "1".into()),
    ];
    sandbox.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ));

    let entered =
        backend_enter_argv_guarded_with_env(&sandbox, "exec true", SandboxArgvEnv::default())
            .unwrap();
    let argv = entered.argv().join(" ");
    assert!(!argv.contains("sk_secret"), "secret leaked in argv: {argv}");
    assert!(
        !argv.contains("EnvironmentFile="),
        "remote systemd must not reach for a host env-file: {argv}"
    );
    assert!(
        argv.contains("--setenv THEGN_SANDBOX=1"),
        "synthetic env inline: {argv}"
    );
    let warning = entered
        .omitted_secrets_warning()
        .expect("omitted secret warning");
    assert!(
        warning.contains("API_KEY"),
        "warning omitted key: {warning}"
    );
    assert!(
        !warning.contains("sk_secret"),
        "warning leaked value: {warning}"
    );
    let env_dir = state.path().join("thegn/sandbox-env");
    assert!(
        !env_dir.exists(),
        "a remote systemd pane must not create a local env-file"
    );
}

#[test]
fn remote_oci_secret_env_payload_rejects_invalid_records_without_echoing_values() {
    let key = "GOOD".to_string();
    let value = "value".to_string();
    let good = [(&key, &value)];
    assert_eq!(
        remote_oci_secret_env_payload(&good).unwrap(),
        Some(b"GOOD=value\n".to_vec())
    );

    let bad_key = "BAD-KEY".to_string();
    let err = remote_oci_secret_env_payload(&[(&bad_key, &value)])
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid record"), "{err}");
    assert!(!err.contains("value"), "error leaked a value: {err}");

    let multiline = "line1\nline2".to_string();
    let err = remote_oci_secret_env_payload(&[(&key, &multiline)])
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid record"), "{err}");
    assert!(!err.contains("line1"), "error leaked a value: {err}");
}

#[test]
fn remote_oci_secret_env_payload_enforces_variable_and_byte_limits() {
    let value = "v".to_string();
    let keys: Vec<String> = (0..=MAX_REMOTE_OCI_SECRET_ENV_VARS)
        .map(|i| format!("KEY_{i}"))
        .collect();
    let pairs: Vec<(&String, &String)> = keys.iter().map(|key| (key, &value)).collect();
    let err = remote_oci_secret_env_payload(&pairs)
        .unwrap_err()
        .to_string();
    assert!(err.contains("variable limit"), "{err}");

    let key = "BIG".to_string();
    let huge = "x".repeat(MAX_REMOTE_OCI_SECRET_ENV_BYTES);
    let err = remote_oci_secret_env_payload(&[(&key, &huge)])
        .unwrap_err()
        .to_string();
    assert!(err.contains("byte limit"), "{err}");
}

#[test]
fn oci_create_argv_and_stdin_error_does_not_echo_secret_value() {
    let mut s = spec(Backend::Podman);
    s.name = "thegn-test-argv-stdin-error".into();
    let key = "GOOD".to_string();
    let bad_value = "canary-value\n".to_string();
    let err = oci_create_argv_and_stdin(&s, Vec::new(), &[(&key, &bad_value)])
        .unwrap_err()
        .to_string();
    assert!(err.contains("invalid record"), "{err}");
    assert!(!err.contains("canary-value"), "error leaked value: {err}");
}

#[cfg(unix)]
#[test]
fn stderr_with_timeout_stdin_redacts_payload_and_reports_child_failure() {
    let argv = vec!["sh".into(), "-c".into(), "cat >&2; exit 3".into()];
    let result = stderr_with_timeout_stdin(&argv, Duration::from_secs(2), b"K=canary\n".to_vec());
    let (success, stderr) = result.expect("child exits and stderr is retained");
    assert!(!success, "exit status 3 must be reported as failure");
    assert!(stderr.contains("[REDACTED]"), "redacted stderr: {stderr}");
    assert!(
        !stderr.contains("canary"),
        "secret leaked in stderr: {stderr}"
    );
}

/// Spawn a live process whose command line carries the REAL remote OCI create
/// argv (as `sh` positional parameters) while the real secret payload goes to
/// its stdin, then read that command line back from `ps`: the canary must be
/// absent from the live argv, and the argv must still be what `ps` printed.
#[cfg(unix)]
#[test]
fn remote_oci_create_keeps_canary_out_of_live_process_argv() {
    let state = tempfile::tempdir().unwrap();
    let state_home = state.path().to_string_lossy().into_owned();
    let _env = crate::testenv::EnvGuard::mutate_pairs(&[
        ("GH_TOKEN", Some("live-process-canary")),
        ("THEGN_SANDBOX", None),
        ("XDG_STATE_HOME", Some(&state_home)),
    ]);
    let mut s = spec(Backend::Podman);
    s.env = vec![("GH_TOKEN".into(), "live-process-canary".into())];
    s.placement = Placement::Ssh(SshPlacement::plain(
        "box".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let secret_env = remote_stdin_oci_secret_env(&s);
    let opts = oci_create_opts_with_remote_stdin_secrets(&s, &secret_env).unwrap();
    let (create, input) = oci_create_argv_and_stdin(&s, opts, &secret_env).unwrap();
    let input = input.expect("SSH host secret stdin");
    assert!(
        String::from_utf8_lossy(&input).contains("live-process-canary"),
        "precondition: the payload carries the canary"
    );
    // `; :` keeps `sh` from exec-ing `ps` in place, so `$$` still names the
    // process whose argv holds the create command.
    let mut argv: Vec<String> = vec![
        "sh".into(),
        "-c".into(),
        "ps -ww -o command= -p $$ >&2; :".into(),
        "sh".into(),
    ];
    argv.extend(create);
    let (success, stderr) = stderr_with_timeout_stdin(&argv, Duration::from_secs(5), input)
        .expect("process listing completes");

    assert!(success, "process listing failed: {stderr}");
    assert!(
        stderr.contains("/dev/stdin"),
        "ps must print the live create argv: {stderr}"
    );
    assert!(
        !stderr.contains("live-process-canary") && !stderr.contains("[REDACTED]"),
        "secret appeared in live process argv: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn stderr_with_timeout_stdin_keeps_stderr_when_feeder_hits_epipe() {
    let argv = vec![
        "sh".into(),
        "-c".into(),
        "printf 'remote diagnostic' >&2; exit 3".into(),
    ];
    let input = vec![b'x'; 1024 * 1024];
    let (success, stderr) = stderr_with_timeout_stdin(&argv, Duration::from_secs(2), input)
        .expect("child exit diagnostics survive a broken stdin pipe");
    assert!(!success, "a child that rejects input reports failure");
    assert!(
        stderr.contains("remote diagnostic"),
        "stderr was lost: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn stderr_with_timeout_stdin_kills_and_reaps_on_timeout() {
    let argv = vec!["sh".into(), "-c".into(), "sleep 5".into()];
    assert!(
        stderr_with_timeout_stdin(&argv, Duration::from_millis(100), b"K=canary\n".to_vec())
            .is_none(),
        "a command exceeding its deadline returns None"
    );
}

#[cfg(unix)]
#[test]
fn stderr_with_timeout_stdin_deadline_kills_descendant_holding_stdin() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let pid_file = dir.path().join("stdin-descendant-pid");
    let script = format!(
        "exec 3<&0; sleep 30 & echo $! > '{}'; exit 0",
        pid_file.display()
    );
    let argv = vec!["sh".into(), "-c".into(), script];
    let started = Instant::now();
    let (success, _) =
        stderr_with_timeout_stdin(&argv, Duration::from_millis(300), vec![b'x'; 1024 * 1024])
            .expect("leader exit and captured stderr are returned after bounded feeder wait");

    assert!(!success, "stdin left in a descendant makes delivery fail");
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_millis(500);
    while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
        assert!(
            Instant::now() < deadline,
            "stdin-holding descendant {pid} survived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stderr_redaction_skips_short_common_values() {
    let stderr = redact_stderr(
        "CI=1 TERM=xterm token=long-secret-value".into(),
        &["1".into(), "xterm".into(), "long-secret-value".into()],
    );
    assert_eq!(stderr, "CI=1 TERM=xterm token=[REDACTED]");
}

#[cfg(unix)]
#[test]
fn secret_env_file_replaces_a_planted_symlink_without_touching_its_target() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("sandbox-env");
    std::fs::create_dir(&dir).unwrap();
    let victim = parent.path().join("victim");
    std::fs::write(&victim, b"sentinel bytes").unwrap();
    std::os::unix::fs::symlink(&victim, dir.join("launch.env")).unwrap();
    let key = String::from("GH_TOKEN");
    let value = String::from("canary-secret");

    let path = write_secret_env_file_in(
        parent.path(),
        std::path::Path::new("sandbox-env"),
        "launch",
        &[(&key, &value)],
        false,
    )
    .expect("atomic env-file write");
    assert_eq!(std::fs::read(&victim).unwrap(), b"sentinel bytes");
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_file()
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "GH_TOKEN=canary-secret\n"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(dir.join("launch.env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn oci_secret_env_generations_are_unique_and_keep_independent_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let key = String::from("GH_TOKEN");
    let first_value = String::from("first-secret");
    let second_value = String::from("second-secret");
    let first = write_secret_env_file_in(
        temp.path(),
        std::path::Path::new("sandbox-env"),
        "launch",
        &[(&key, &first_value)],
        true,
    )
    .expect("first generated env-file");
    let second = write_secret_env_file_in(
        temp.path(),
        std::path::Path::new("sandbox-env"),
        "launch",
        &[(&key, &second_value)],
        true,
    )
    .expect("second generated env-file");
    assert_ne!(first, second);
    assert_eq!(
        std::fs::read_to_string(first).unwrap(),
        "GH_TOKEN=first-secret\n"
    );
    assert_eq!(
        std::fs::read_to_string(second).unwrap(),
        "GH_TOKEN=second-secret\n"
    );
}

#[test]
fn oci_env_cleanup_removes_only_env_files_inside_the_sandbox_env_directory() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("sandbox-env");
    let key = String::from("GH_TOKEN");
    let value = String::from("canary-secret");
    let generated = write_secret_env_file_in(
        temp.path(),
        std::path::Path::new("sandbox-env"),
        "launch",
        &[(&key, &value)],
        true,
    )
    .expect("generated env-file");
    let outside = temp.path().join("outside.env");
    std::fs::write(&outside, b"outside sentinel").unwrap();

    assert!(remove_oci_env_file_in(&dir, &generated).unwrap());
    assert!(!generated.exists());
    assert!(!remove_oci_env_file_in(&dir, std::path::Path::new("/dev/stdin")).unwrap());
    assert!(!remove_oci_env_file_in(&dir, &outside).unwrap());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside sentinel");
}

#[cfg(unix)]
#[test]
fn secret_env_file_rejects_a_symlinked_directory() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let dir = parent.path().join("sandbox-env");
    std::os::unix::fs::symlink(&target, &dir).unwrap();
    let key = String::from("GH_TOKEN");
    let value = String::from("canary-secret");

    assert!(
        write_secret_env_file_in(
            parent.path(),
            std::path::Path::new("sandbox-env"),
            "launch",
            &[(&key, &value)],
            false,
        )
        .is_none()
    );
    assert!(!target.join("launch.env").exists());
}

#[test]
fn secret_env_file_rejects_a_regular_file_in_place_of_directory() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("sandbox-env");
    std::fs::write(&dir, b"not a directory").unwrap();
    let key = String::from("GH_TOKEN");
    let value = String::from("canary-secret");

    assert!(
        write_secret_env_file_in(
            parent.path(),
            std::path::Path::new("sandbox-env"),
            "launch",
            &[(&key, &value)],
            false,
        )
        .is_none()
    );
    assert_eq!(std::fs::read(&dir).unwrap(), b"not a directory");
}

#[cfg(unix)]
#[test]
fn secret_env_file_rejects_a_symlinked_intermediate_component() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(&target, parent.path().join("thegn")).unwrap();
    let key = String::from("GH_TOKEN");
    let value = String::from("canary-secret");

    assert!(
        write_secret_env_file_in(
            parent.path(),
            std::path::Path::new("thegn/sandbox-env"),
            "launch",
            &[(&key, &value)],
            false,
        )
        .is_none()
    );
    assert!(!target.join("sandbox-env").exists());
}

#[cfg(unix)]
#[test]
fn secret_env_file_tightens_owned_group_writable_state_directory() {
    let parent = tempfile::tempdir().unwrap();
    let intermediate = parent.path().join("thegn");
    std::fs::create_dir(&intermediate).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&intermediate, std::fs::Permissions::from_mode(0o775)).unwrap();
    let key = String::from("GH_TOKEN");
    let value = String::from("canary-secret");

    let path = write_secret_env_file_in(
        parent.path(),
        std::path::Path::new("thegn/sandbox-env"),
        "launch",
        &[(&key, &value)],
        false,
    )
    .expect("owned group-writable state directory is tightened");
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "GH_TOKEN=canary-secret\n"
    );
    assert_eq!(
        std::fs::metadata(intermediate)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}

#[test]
fn apple_inspect_json_is_parsed_where_go_templates_do_not_exist() {
    // Verified against Apple `container` 1.2.2 on macOS 26: `container container
    // inspect` is not a command ("Plugin 'container-container' not found") and
    // `--format` is an unknown option. The docker-shaped probe therefore always
    // answered "not running", so a container thegn had just successfully created
    // was declared a failure — the backend fell out of the chain while leaving
    // the container running. This is the real JSON shape.
    let json = r#"[{
      "id": "thegn-x",
      "status": { "state": "running", "startedDate": "2026-08-24T01:55:50Z" },
      "configuration": { "mounts": [
        { "type": {"virtiofs":{}}, "source": "/wt/feat", "destination": "/wt/feat" },
        { "type": {"virtiofs":{}}, "source": "/repo/.git", "destination": "/repo/.git" }
      ] }
    }]"#;
    assert_eq!(parse_apple_inspect(json, &["/wt/feat"]), (true, true));
    assert_eq!(
        parse_apple_inspect(json, &["/wt/feat", "/repo/.git"]),
        (true, true)
    );
    // A required mount that isn't there ⇒ running, but stale: force a recreate.
    assert_eq!(
        parse_apple_inspect(json, &["/wt/feat", "/gone"]),
        (true, false)
    );

    // Anything but "running" is unusable — a created-but-not-started container
    // passes inspect yet cannot accept exec, the same trap the podman path guards.
    let stopped = json.replace("\"running\"", "\"stopped\"");
    assert_eq!(parse_apple_inspect(&stopped, &[]), (false, false));

    // Unparseable / empty output must read as "not running", never as healthy.
    assert_eq!(parse_apple_inspect("", &[]), (false, false));
    assert_eq!(parse_apple_inspect("[]", &[]), (false, false));
    assert_eq!(parse_apple_inspect("not json", &[]), (false, false));
}

#[test]
fn host_toolchain_mounts_are_withheld_when_the_guest_abi_differs() {
    // An OCI guest is always Linux. `host_toolchain_mounts` hands it the HOST's
    // /usr, /bin and /nix/store — correct on a Linux host, catastrophic on a Mac,
    // where they are Mach-O and shadow the guest's own binaries. Verified against
    // Apple `container`: `-v /usr:/usr:ro` gives "failed to find target
    // executable sleep" and `-v /bin:/bin:ro` gives "Exec format error".
    let s = spec(Backend::Apple);
    let joined = oci_create_opts(&s).expect("valid volume names").join(" ");
    if crate::sandbox_backend::host_os() == crate::sandbox_backend::HostOs::Linux {
        // Same-ABI host: the injection is the whole point, so it must stay.
        return;
    }
    for shadowing in [
        "/usr:/usr",
        "/bin:/bin",
        "/lib:/lib",
        "/nix/store:/nix/store",
    ] {
        assert!(
            !joined.contains(shadowing),
            "a Linux guest must not receive the host's {shadowing} on a non-Linux host: {joined}"
        );
    }
    // The worktree itself is still mounted — this narrows the toolchain
    // substrate, not the sandbox's reason for existing.
    assert!(joined.contains("-v /wt/feat:/wt/feat"), "{joined}");
}

#[test]
fn guest_shares_host_abi_is_true_for_everything_but_a_foreign_oci_guest() {
    use crate::sandbox::guest_shares_host_abi;
    use crate::sandbox_backend::HostOs;

    // Only OCI backends can have a foreign guest: bwrap, systemd and `none` run
    // on this host's own kernel, so host and guest are the same system whatever
    // the OS. Getting this backwards would withhold the Nix daemon socket from a
    // plain `backend = none` pane on a Mac, where it is exactly what makes
    // `nix develop` work.
    for b in [Backend::Bwrap, Backend::Systemd, Backend::None] {
        for os in [HostOs::Linux, HostOs::MacOs, HostOs::Windows] {
            assert!(guest_shares_host_abi(b, os), "{b:?} on {os:?}");
        }
    }
    // OCI: same ABI on Linux (the case the injection was built for), foreign
    // everywhere else — its container is always a Linux one, reached through a VM.
    for b in [
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Apple,
    ] {
        assert!(guest_shares_host_abi(b, HostOs::Linux), "{b:?} on Linux");
        assert!(!guest_shares_host_abi(b, HostOs::MacOs), "{b:?} on macOS");
        assert!(
            !guest_shares_host_abi(b, HostOs::Windows),
            "{b:?} on Windows"
        );
    }
}

#[test]
fn the_nix_daemon_socket_is_withheld_from_a_foreign_guest() {
    // The gap that made podman unusable on a Nix-managed Mac: the ABI gate above
    // covered `host_toolchain_mounts`, but the Tier-B daemon socket is injected
    // separately and bound `/nix/var/nix/daemon-socket` unconditionally. podman
    // machine shares only /Users, /private and /var/folders, so `run` exited 125
    // with `statfs /nix: no such file or directory` and NO container was created
    // — the backend failed every launch, reported as "could not start podman
    // container '<name>'".
    //
    // Asserted through `oci_create_opts` (the argv the runtime actually gets)
    // rather than the gate, so it fails if the mount returns by another route.
    if crate::sandbox_backend::host_os() == crate::sandbox_backend::HostOs::Linux {
        return; // same ABI: the socket is the point, and it must stay
    }
    let mut s = spec(Backend::Podman);
    s.mounts.push(Mount {
        host: "/nix/var/nix/daemon-socket".into(),
        dest: "/nix/var/nix/daemon-socket".into(),
        ro: true,
        cache: false,
    });
    s.devenv = true;
    s.devenv_path = Some("/nix/store/abc-devenv/bin/devenv".into());
    let joined = oci_create_opts(&s).expect("valid volume names").join(" ");
    // The devenv `/nix` bind is gated at emit time, so it must be absent here
    // even though the spec asks for it.
    assert!(
        !joined.contains("-v /nix:/nix"),
        "a Linux guest on a non-Linux host must not receive the host's /nix: {joined}"
    );
}

// --- THE-156: the default containment image and its entrypoint -------------

#[test]
fn default_oci_image_is_digest_pinned() {
    // A tag is a mutable pointer the registry owner can retarget; the default
    // containment boundary must be content-addressed.
    assert!(
        crate::sandbox::image_is_digest_pinned(DEFAULT_OCI_IMAGE),
        "default image is not digest-pinned: {DEFAULT_OCI_IMAGE}"
    );
    // The tag rides along for readability, so the reference still says what it
    // is rather than being an opaque hash.
    let parsed = crate::image::ImageRef::parse(DEFAULT_OCI_IMAGE).expect("parses");
    assert_eq!(parsed.tag, "stable");
    assert_eq!(parsed.name, "docker.io/library/debian");
}

#[test]
fn image_is_digest_pinned_rejects_mutable_tags() {
    assert!(!crate::sandbox::image_is_digest_pinned(
        "docker.io/library/debian:stable"
    ));
    assert!(!crate::sandbox::image_is_digest_pinned("alpine"));
    // A registry port is not a digest.
    assert!(!crate::sandbox::image_is_digest_pinned("reg:5000/img:v1"));
    // Truncated / malformed digests are not pins.
    assert!(!crate::sandbox::image_is_digest_pinned("img@sha256:abc"));
    assert!(crate::sandbox::image_is_digest_pinned(&format!(
        "img@sha256:{}",
        "a".repeat(64)
    )));
}

#[test]
fn entrypoint_override_precedes_the_image_and_names_the_workload() {
    // The ordering is the whole point: `--entrypoint` must be a CREATE option,
    // before the image, or the runtime treats it as a container argument.
    let ep = crate::sandbox::entrypoint_override();
    assert_eq!(ep, ["--entrypoint".to_string(), "sleep".to_string()]);

    // Reconstruct the argv shape both run sites build, and assert an image
    // ENTRYPOINT can no longer wrap the workload.
    let mut argv: Vec<String> = vec!["run".into(), "-d".into()];
    argv.extend(ep);
    argv.push(DEFAULT_OCI_IMAGE.to_string());
    argv.push("infinity".into());

    let image_at = argv
        .iter()
        .position(|a| a == DEFAULT_OCI_IMAGE)
        .expect("image present");
    let flag_at = argv
        .iter()
        .position(|a| a == "--entrypoint")
        .expect("entrypoint flag present");
    assert!(
        flag_at < image_at,
        "--entrypoint must precede the image: {argv:?}"
    );
    // Exactly one workload is named, and it is ours.
    assert_eq!(argv.last().map(String::as_str), Some("infinity"));
    assert_eq!(
        argv.iter().filter(|a| *a == "sleep").count(),
        1,
        "sleep is the entrypoint, not also a trailing command: {argv:?}"
    );
}

#[test]
fn sealed_container_reuse_requires_exact_mounts_and_a_matching_seal_label() {
    let required: std::collections::HashSet<&str> = ["/wt", "/nix/store"].into_iter().collect();
    // Probe format: RUNNING, seal label, policy fingerprint, then bind sources.
    let exact = "RUNNING\n1\nfp\n/wt\n/nix/store\n";
    assert_eq!(
        judge_inspect(exact, &required, true, false, "fp"),
        (true, true)
    );
    assert_eq!(
        judge_inspect(
            "RUNNING\n1\nfp\n/wt\n/nix/store\n/dev/null\n",
            &required,
            true,
            false,
            "fp"
        ),
        (true, true)
    );
    // (a) a superset (a hardened container that also binds ~/.gnupg) is refused
    // for a sealed spec, though it is fine for a non-sealed one.
    let superset = "RUNNING\n1\nfp\n/wt\n/nix/store\n/home/u/.gnupg\n";
    assert_eq!(
        judge_inspect(superset, &required, true, false, "fp"),
        (true, false)
    );
    assert_eq!(
        judge_inspect(
            "RUNNING\n\nfp\n/wt\n/nix/store\n/home/u/.gnupg\n",
            &required,
            false,
            false,
            "fp"
        ),
        (true, true)
    );
    // devenv's own `/nix` bind is not an extra, but only while the current spec
    // would still emit it: with devenv off the leftover bind forces a recreate.
    let with_nix = "RUNNING\n1\nfp\n/wt\n/nix/store\n/nix\n";
    assert_eq!(
        judge_inspect(with_nix, &required, true, true, "fp"),
        (true, true)
    );
    assert_eq!(
        judge_inspect(with_nix, &required, true, false, "fp"),
        (true, false)
    );
    // (b) label mismatch forces recreate, both directions; a missing label on a
    // sealed spec is a pre-label container and is recreated.
    assert_eq!(
        judge_inspect(
            "RUNNING\n0\nfp\n/wt\n/nix/store\n",
            &required,
            true,
            false,
            "fp"
        ),
        (true, false)
    );
    assert_eq!(
        judge_inspect(
            "RUNNING\n\nfp\n/wt\n/nix/store\n",
            &required,
            true,
            false,
            "fp"
        ),
        (true, false)
    );
    assert_eq!(
        judge_inspect(
            "RUNNING\n1\nfp\n/wt\n/nix/store\n",
            &required,
            false,
            false,
            "fp"
        ),
        (true, false)
    );
    assert_eq!(
        judge_inspect(
            "RUNNING\n0\nfp\n/wt\n/nix/store\n",
            &required,
            false,
            false,
            "fp"
        ),
        (true, true)
    );
    assert_eq!(
        judge_inspect("EXITED\n1\nfp\n/wt\n", &required, true, false, "fp"),
        (false, false)
    );
    // Non-sealed reuse never cared about extras, nix included.
    assert_eq!(
        judge_inspect(
            "RUNNING\n\nfp\n/wt\n/nix/store\n/nix\n",
            &required,
            false,
            false,
            "fp"
        ),
        (true, true)
    );
}

#[test]
fn judge_inspect_rejects_policy_mismatch() {
    let required: std::collections::HashSet<&str> = ["/wt"].into_iter().collect();
    assert_eq!(
        judge_inspect(
            "RUNNING\n0\nold-fp\n/wt\n",
            &required,
            false,
            false,
            "new-fp"
        ),
        (true, false)
    );
}

#[test]
fn judge_inspect_rejects_missing_policy_label() {
    let required: std::collections::HashSet<&str> = ["/wt"].into_iter().collect();
    assert_eq!(
        judge_inspect("RUNNING\n0\n/wt\n", &required, false, false, "new-fp"),
        (true, false)
    );
    assert_eq!(
        judge_inspect("RUNNING\n0\n\n/wt\n", &required, false, false, "new-fp"),
        (true, false)
    );
}

#[test]
fn judge_inspect_accepts_exact_policy() {
    let required: std::collections::HashSet<&str> = ["/wt"].into_iter().collect();
    assert_eq!(
        judge_inspect(
            "RUNNING\n0\nnew-fp\n/wt\n",
            &required,
            false,
            false,
            "new-fp"
        ),
        (true, true)
    );
}

#[test]
fn image_id_normalises_docker_and_podman_shapes() {
    let podman = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(
        normalize_image_id(&format!("sha256:{podman}")),
        Some(podman.into())
    );
    assert_eq!(normalize_image_id(podman), Some(podman.into()));
    assert_eq!(
        normalize_image_id("ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789"),
        Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into())
    );
    assert_eq!(normalize_image_id(""), None);
    assert_eq!(normalize_image_id("<no value>"), None);
    assert_eq!(normalize_image_id(&podman[..63]), None);
    assert_eq!(normalize_image_id("debian:stable"), None);
}

#[test]
fn image_reuse_requires_equal_ids() {
    let id = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let same = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let other = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    assert!(image_reusable(Some(id), Some(same)));
    assert!(!image_reusable(Some(id), Some(other)));
    assert!(!image_reusable(None, Some(same)));
    assert!(!image_reusable(Some(id), None));
}

#[test]
fn split_image_line_keeps_judge_inspect_input_unchanged() {
    let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let stdout = format!("RUNNING\n0\nnew-fp\n/wt\nthegn-image:x\nthegn-image:sha256:{id}\n");
    let (inspect, image) = split_image_line(&stdout);
    assert_eq!(inspect, "RUNNING\n0\nnew-fp\n/wt\nthegn-image:x\n");
    let expected_image = format!("sha256:{id}");
    assert_eq!(image.as_deref(), Some(expected_image.as_str()));
    let required: std::collections::HashSet<&str> = ["/wt"].into_iter().collect();
    assert_eq!(
        judge_inspect(&inspect, &required, false, false, "new-fp"),
        (true, true)
    );
}

/// Observed on macOS (Docker 29.5.2 via colima): `docker container inspect
/// --format` appends its own newline after the template's trailing `\n`, so the
/// image line was no longer final and every fresh container read as drifted.
#[test]
fn split_image_line_survives_docker_double_trailing_newline() {
    let id = "2bf225230f05881b6a36d20f484088f408c9a1309edc730c0bf01f15e635d411";
    let stdout =
        format!("RUNNING\n0\nfp\n/Users/u/repo\n/Users/u/repo/.git\nthegn-image:sha256:{id}\n\n");
    let (inspect, image) = split_image_line(&stdout);
    assert_eq!(
        inspect,
        "RUNNING\n0\nfp\n/Users/u/repo\n/Users/u/repo/.git\n"
    );
    let expected_image = format!("sha256:{id}");
    assert_eq!(image.as_deref(), Some(expected_image.as_str()));
    assert!(image_reusable(
        image.as_deref(),
        Some(&format!("sha256:{id}\n"))
    ));
    // A missing image line still yields no id, never a guessed one.
    let (_, none) = split_image_line("RUNNING\n0\nfp\n/wt\n\n");
    assert_eq!(none, None);
}

#[test]
fn expected_policy_is_stable_across_sealed_view() {
    let mut s = spec(Backend::Podman);
    s.seal_home = Some("/home/u".into());
    s.mounts.push(Mount {
        host: "/home/u/.gnupg".into(),
        dest: "/home/u/.gnupg".into(),
        ro: false,
        cache: false,
    });
    assert_eq!(expected_policy(&s), expected_policy(&sealed_view(&s)));
}

#[test]
fn create_opts_stamp_the_expected_policy() {
    let s = spec(Backend::Podman);
    let argv = oci_create_opts(&s).unwrap().join(" ");
    assert!(argv.contains(&format!("--label {POLICY_LABEL}={}", expected_policy(&s))));
    assert!(argv.contains(crate::sandbox_manage::OWNED_LABEL));
    assert!(argv.contains("--label thegn.seal_home=0"));

    let apple = SandboxSpec {
        backend: Backend::Apple,
        ..s
    };
    assert!(
        !oci_create_opts(&apple)
            .unwrap()
            .join(" ")
            .contains(POLICY_LABEL)
    );
}

#[test]
fn sealed_apple_reuse_refuses_a_missing_mounts_array() {
    let missing = r#"[{"status":{"state":"running"},"configuration":{}}]"#;
    let empty = r#"[{"status":{"state":"running"},"configuration":{"mounts":[]}}]"#;
    // Sealed, nothing required: present-and-empty is verified, missing is not.
    assert_eq!(
        parse_apple_inspect_sealed(empty, &[], true, false),
        (true, true)
    );
    assert_eq!(
        parse_apple_inspect_sealed(missing, &[], true, false),
        (true, false)
    );
    // Non-sealed behaviour is unchanged.
    assert_eq!(
        parse_apple_inspect_sealed(missing, &[], false, false),
        (true, true)
    );
    // An extra bind on a sealed spec is still refused; `/nix` only if expected.
    let nix = r#"[{"status":{"state":"running"},"configuration":{"mounts":[{"source":"/nix"}]}}]"#;
    assert_eq!(
        parse_apple_inspect_sealed(nix, &[], true, false),
        (true, false)
    );
    assert_eq!(
        parse_apple_inspect_sealed(nix, &[], true, true),
        (true, true)
    );
}

#[test]
fn oci_create_stamps_the_seal_label() {
    let mut s = spec(Backend::Podman);
    assert!(
        oci_create_opts(&s)
            .unwrap()
            .join(" ")
            .contains("--label thegn.seal_home=0")
    );
    s.seal_home = Some("/home/u".into());
    assert!(
        oci_create_opts(&s)
            .unwrap()
            .join(" ")
            .contains("--label thegn.seal_home=1")
    );
}

#[test]
fn bwrap_resolver_bind_is_skipped_for_no_network_and_covered_dirs() {
    let dir = std::path::Path::new("/run/systemd/resolve");
    let mut s = spec(Backend::Bwrap);
    s.network = Network::Nat;
    assert_eq!(
        resolver_bind_filter(&s, &[], dir).as_deref(),
        Some("/run/systemd/resolve")
    );
    // network = none: fail closed, no resolver mount.
    s.network = Network::None;
    assert_eq!(resolver_bind_filter(&s, &[], dir), None);
    // The user's `/run/systemd/resolve:ro` workaround (or a parent): no duplicate.
    s.network = Network::Nat;
    s.mounts.push(Mount {
        host: "/run/systemd/resolve".into(),
        dest: "/run/systemd/resolve".into(),
        ro: true,
        cache: false,
    });
    assert_eq!(resolver_bind_filter(&s, &[], dir), None);
    // Already under a hardcoded parent.
    let s2 = spec(Backend::Bwrap);
    assert_eq!(resolver_bind_filter(&s2, &["/run/systemd"], dir), None);
}

#[test]
fn bwrap_resolver_bind_is_covered_for_nested_user_mounts() {
    let dir = std::path::Path::new("/run/systemd/resolve");
    let nested = Mount {
        host: "/host/netif".into(),
        dest: "/run/systemd/resolve/netif".into(),
        ro: false,
        cache: false,
    };
    let mut s = spec(Backend::Bwrap);
    s.mounts.push(nested.clone());
    assert_eq!(
        resolver_bind_filter(&s, &[], dir).as_deref(),
        Some("/run/systemd/resolve")
    );
    assert!(bwrap_mount_covered(&nested, &["/run/systemd/resolve"]));
    let at_dir = Mount {
        dest: "/run/systemd/resolve".into(),
        ..nested.clone()
    };
    assert!(!bwrap_mount_covered(&at_dir, &["/run/systemd/resolve"]));
}

#[test]
fn bwrap_mount_under_private_tmp_is_bound_after_the_tmpfs() {
    let report_dir = "/tmp/thegn-report-test";
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: report_dir.into(),
        dest: report_dir.into(),
        ro: false,
        cache: false,
    });

    let argv = backend_enter_argv(&sandbox, "true").unwrap();
    let tmpfs = argv
        .windows(2)
        .position(|window| window == ["--tmpfs", "/tmp"])
        .expect("bwrap must create a private /tmp");
    let bind = argv
        .windows(3)
        .position(|window| window == ["--bind", report_dir, report_dir])
        .expect("the report mount must not be dropped");
    assert!(tmpfs < bind, "private /tmp must precede the report bind");
}

#[test]
fn bwrap_network_none_argv_has_no_resolver_bind() {
    let resolver_dir = std::path::Path::new("/run/systemd/resolve");
    let resolver_dirs = [
        "/run/systemd/resolve",
        "/run/NetworkManager",
        "/run/resolvconf",
        "/run/connman",
    ];
    let contains_resolver_dir = |argv: &[String]| {
        argv.iter().any(|arg| {
            resolver_dirs
                .iter()
                .any(|dir| arg.as_str() == *dir || arg.starts_with(*dir))
        })
    };

    let mut nat = spec(Backend::Bwrap);
    nat.network = Network::Nat;
    let nat_argv = backend_enter_argv_with_resolver_dir(&nat, "true", Some(resolver_dir)).unwrap();
    assert!(
        nat_argv
            .windows(3)
            .any(|w| { w[0] == "--ro-bind" && w[1] == "/run/systemd/resolve" && w[2] == w[1] })
    );

    let mut none = spec(Backend::Bwrap);
    none.network = Network::None;
    let argv = backend_enter_argv_with_resolver_dir(&none, "true", Some(resolver_dir)).unwrap();
    assert!(
        !contains_resolver_dir(&argv),
        "unexpected resolver bind in argv: {}",
        argv.join(" ")
    );
    assert!(argv.iter().any(|arg| arg == "--unshare-net"));
}

#[test]
fn systemd_launch_refuses_mount_paths_systemd_would_misparse() {
    // Open profiles emit no mount properties, so a host and destination that
    // would be unsafe if emitted are irrelevant to this backend's argv.
    let mut open = spec(Backend::Systemd);
    open.mounts.push(Mount {
        host: "/host/My Code/repo".into(),
        dest: "/sandbox/My Code/repo".into(),
        ro: false,
        cache: false,
    });
    assert!(launchable(&open).is_ok());
    assert!(systemd_mount_props(&open).unwrap().is_empty());

    // The existing .git/config exception is emitted even for an open profile,
    // so its destination must still pass the same path validation.
    let mut git_config = spec(Backend::Systemd);
    git_config.mounts.push(Mount {
        host: "/repo/.git/config".into(),
        dest: "/repo/.git/config".into(),
        ro: true,
        cache: false,
    });
    let props = systemd_mount_props(&git_config).unwrap();
    assert_eq!(props.len(), 1);
    assert_eq!(props[0], "ReadOnlyPaths=/repo/.git/config");
    git_config.mounts.last_mut().unwrap().dest = "/repo/My Code/config".into();
    assert!(matches!(
        enter_argv(&git_config, "true"),
        Err(EnterError::Refused(Refusal {
            class: RefusalClass::SystemdMountPath,
            path: Some(_),
            ..
        }))
    ));

    // Hardened with no sealed HOME emits no property for a read-only mount.
    let mut hardened_ro = spec(Backend::Systemd);
    hardened_ro.read_only_root = true;
    hardened_ro.mounts.push(Mount {
        host: "/host/Some Dir".into(),
        dest: "/sandbox/Some Dir".into(),
        ro: true,
        cache: false,
    });
    assert!(launchable(&hardened_ro).is_ok());
    assert!(
        systemd_mount_props(&hardened_ro)
            .unwrap()
            .iter()
            .all(|p| !p.contains("Some Dir"))
    );

    // A writable destination is emitted as ReadWritePaths and must be refused.
    let mut emitted = spec(Backend::Systemd);
    emitted.read_only_root = true;
    emitted.mounts.push(Mount {
        host: "/host/repo".into(),
        dest: "/sandbox/My Code/repo".into(),
        ro: false,
        cache: false,
    });
    assert!(matches!(
        enter_argv(&emitted, "true"),
        Err(EnterError::Refused(Refusal {
            class: RefusalClass::SystemdMountPath,
            path: Some(_),
            ..
        }))
    ));
    let direct = backend_enter_argv(&emitted, "true");
    assert!(matches!(
        &direct,
        Err(EnterError::Refused(Refusal {
            class: RefusalClass::SystemdMountPath,
            path: Some(_),
            ..
        }))
    ));
    assert!(!matches!(
        direct,
        Ok(argv) if argv.iter().any(|arg| arg.contains("ThegnRefused"))
    ));

    // A host path is emitted only for binds beneath a sealed HOME.
    let mut sealed = spec(Backend::Systemd);
    sealed.read_only_root = true;
    sealed.seal_home = Some("/home/test user".into());
    sealed.mounts.push(Mount {
        host: "/host/My Code/repo".into(),
        dest: "/home/test user/repo".into(),
        ro: false,
        cache: false,
    });
    let refusal = systemd_mount_props(&sealed).unwrap_err();
    assert!(refusal.contains("/host/My Code/repo"), "{refusal}");

    // The default writable carve-outs are encoded by the same checked path
    // property builder, including a HOME that systemd would split/expand.
    let synthetic_carveout = Mount {
        host: "/host/home/tmp".into(),
        dest: "/home/user name/tmp".into(),
        ro: false,
        cache: false,
    };
    let mut carveout_spec = spec(Backend::Systemd);
    carveout_spec.read_only_root = true;
    carveout_spec.mounts.clear();
    assert!(
        systemd_mount_props_with_carveouts(&carveout_spec, &[synthetic_carveout])
            .unwrap_err()
            .contains("user name")
    );
    assert!(launchable(&spec(Backend::Systemd)).is_ok());
    let mut protected = spec(Backend::Systemd);
    protected.read_only_root = true;
    let protected_argv = backend_enter_argv_with_env(
        &protected,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user/9".into()),
            control_dirs: Vec::new(),
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        protected_argv
            .windows(2)
            .any(|pair| { pair[0] == "-p" && pair[1] == "PrivateTmp=yes" })
    );
    assert!(
        protected_argv
            .windows(2)
            .any(|pair| { pair[0] == "-p" && pair[1] == "InaccessiblePaths=-/run/user/9/thegn" })
    );
    let open_argv = backend_enter_argv_with_env(
        &spec(Backend::Systemd),
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user/9".into()),
            control_dirs: Vec::new(),
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        !open_argv
            .iter()
            .any(|arg| arg.starts_with("InaccessiblePaths="))
    );
}

#[test]
fn universal_block_lowers_to_network_none() {
    assert_eq!(
        lower_universal_deny(Network::Nat, &["*".into()]),
        Network::None
    );
    assert_eq!(
        lower_universal_deny(Network::Nat, &[" * ".into()]),
        Network::None
    );
    assert_eq!(
        lower_universal_deny(Network::Host, &["x.test".into()]),
        Network::Host
    );

    let cfg = crate::config::SandboxConfig {
        enabled: true,
        backend: crate::config::SandboxBackend::Bwrap,
        network_block: vec!["*".into()],
        isolation_floor: Some(crate::config::IsolationFloor::Off),
        on_floor_miss: crate::config::OnFloorMiss::Degrade,
        ..Default::default()
    };
    let loc = crate::remote::GitLoc::from_db("/wt/x", None);
    if let Some(resolved) = resolve(&cfg, &loc, "test") {
        assert_eq!(resolved.network, Network::None);
    }
}

#[test]
fn hostname_rules_note_names_backend_on_every_local_backend() {
    for backend in [
        Backend::Bwrap,
        Backend::Systemd,
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Apple,
    ] {
        let mut s = spec(backend);
        s.network_allow = vec!["example.com".into()];
        let note = hostname_rules_note(&s).expect("active allow-list should be reported");
        assert!(note.contains(backend.label()), "{note}");
        assert!(note.contains("are advisory"), "{note}");
        assert!(!note.contains("DNS-only"), "{note}");

        s.network = Network::Host;
        s.network_allow.clear();
        s.network_block = vec!["example.com".into()];
        assert!(hostname_rules_note(&s).is_some());

        s.network = Network::None;
        assert!(hostname_rules_note(&s).is_none());
        s.network = Network::Nat;
        s.network_block.clear();
        assert!(hostname_rules_note(&s).is_none());
    }

    let mut provider = spec(Backend::Podman);
    provider.network_block = vec!["example.com".into()];
    provider.placement = Placement::Provider(crate::placement::ProviderPlacement {
        provider: "test".into(),
        id: "sandbox".into(),
        interactive_prefix: vec![],
        control_prefix: vec![],
        up_command: vec![],
        down_command: vec![],
    });
    // An unknown provider is the `custom` kind: no egress policy API.
    let note = hostname_rules_note(&provider).expect("custom provider lists are unenforced");
    assert!(note.contains("NOT enforced"), "{note}");
}

fn provider_spec_with_lists(provider: &str) -> SandboxSpec {
    let mut s = spec(Backend::Podman);
    s.network_allow = vec!["github.com".into()];
    s.placement = Placement::Provider(crate::placement::ProviderPlacement {
        provider: provider.into(),
        id: "sandbox".into(),
        interactive_prefix: vec![],
        control_prefix: vec![],
        up_command: vec![],
        down_command: vec![],
    });
    s
}

#[test]
fn note_for_daytona_provider_warns() {
    let note = hostname_rules_note(&provider_spec_with_lists("daytona"))
        .expect("daytona has no egress policy API");
    assert!(note.contains("env provider 'daytona'"), "{note}");
    assert!(note.contains("NOT enforced"), "{note}");
    assert!(note.contains("network = \"none\""), "{note}");
}

#[test]
fn note_for_custom_provider_warns() {
    let note = hostname_rules_note(&provider_spec_with_lists("custom"))
        .expect("custom has no egress policy API");
    assert!(note.contains("NOT enforced"), "{note}");
}

#[test]
fn note_for_sprites_provider_is_none() {
    assert!(hostname_rules_note(&provider_spec_with_lists("sprites")).is_none());
}

#[test]
fn local_note_says_advisory_not_dns() {
    let mut s = spec(Backend::Bwrap);
    s.network_block = vec!["example.com".into()];
    let note = hostname_rules_note(&s).expect("local lists are advisory");
    assert!(note.contains("advisory"), "{note}");
    assert!(!note.contains("DNS"), "{note}");
}

#[test]
fn policy_refusal_skips_only_egress_api_providers() {
    use crate::config::EnvProviderKind;
    use crate::seam::Kind as _;
    for kind in EnvProviderKind::ALL.iter().copied() {
        let s = provider_spec_with_lists(kind.as_str());
        let refused = hostname_policy_refusal_with(HostnameRulesPolicy::Refuse, &s);
        assert_eq!(refused.is_none(), kind.egress_api(), "{kind:?}");
        assert_eq!(
            hostname_rules_note(&s).is_none(),
            kind.egress_api(),
            "{kind:?}"
        );
        assert!(hostname_policy_refusal_with(HostnameRulesPolicy::WarnUnenforced, &s).is_none());
    }
}

#[test]
fn no_dns_flag_in_oci_create_opts() {
    let mut s = spec(Backend::Podman);
    s.network = Network::Nat;
    s.network_block = vec!["x.test".into()];
    let opts = oci_create_opts(&s).expect("valid volume names");
    assert!(!opts.iter().any(|arg| arg == "--dns"), "{opts:?}");
}

#[test]
fn default_hostname_policy_warns_not_refuses() {
    assert_eq!(HOSTNAME_RULES, HostnameRulesPolicy::WarnUnenforced);
    let mut s = spec(Backend::Bwrap);
    s.network_allow = vec!["example.com".into()];
    assert!(!matches!(
        enter_argv(&s, "sh"),
        Err(EnterError::Refused(Refusal {
            class: RefusalClass::HostnamePolicy,
            ..
        }))
    ));
}

#[test]
fn refuse_policy_refuses_hostname_rules() {
    for backend in [
        Backend::Bwrap,
        Backend::Systemd,
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Apple,
    ] {
        let mut s = spec(backend);
        s.network_block = vec!["example.com".into()];
        let message = hostname_policy_refusal_with(HostnameRulesPolicy::Refuse, &s)
            .expect("active local hostname rules should refuse");
        assert!(message.contains("network = \"none\""), "{message}");
        s.network = Network::Host;
        assert!(hostname_policy_refusal_with(HostnameRulesPolicy::Refuse, &s).is_some());
        s.network = Network::None;
        assert!(hostname_policy_refusal_with(HostnameRulesPolicy::Refuse, &s).is_none());
        s.network = Network::Nat;
        assert!(hostname_policy_refusal_with(HostnameRulesPolicy::WarnUnenforced, &s).is_none());
    }

    let mut remote = spec(Backend::Bwrap);
    remote.network_allow = vec!["example.com".into()];
    remote.placement = Placement::Ssh(SshPlacement::plain(
        "host".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    assert!(hostname_policy_refusal_with(HostnameRulesPolicy::Refuse, &remote).is_some());

    let mut placement_spec = spec(Backend::Podman);
    placement_spec.network_allow = vec!["example.com".into()];
    placement_spec.placement = Placement::Provider(crate::placement::ProviderPlacement {
        provider: "test".into(),
        id: "sandbox".into(),
        interactive_prefix: vec![],
        control_prefix: vec![],
        up_command: vec![],
        down_command: vec![],
    });
    // A provider without an egress policy API (`test` is the custom kind)
    // refuses under the refuse policy; only an egress-API provider is exempt.
    assert!(hostname_policy_refusal_with(HostnameRulesPolicy::Refuse, &placement_spec).is_some());
    placement_spec.placement = Placement::K8s(crate::placement::K8sPlacement {
        kubectl: "kubectl".into(),
        context: None,
        namespace: None,
        pod: "test".into(),
        container: None,
        pod_template: None,
        image: None,
    });
    assert!(hostname_policy_refusal_with(HostnameRulesPolicy::Refuse, &placement_spec).is_none());
}

#[test]
fn universal_block_suppresses_vpn_sidecar() {
    let cfg = crate::config::SandboxConfig {
        enabled: true,
        backend: crate::config::SandboxBackend::None,
        network: Network::Nat,
        network_block: vec!["*".into()],
        vpn: {
            let mut vpn = vpn_cfg(VpnProviderKind::Tailscale);
            vpn.mode = VpnMode::Proxy;
            vpn
        },
        isolation_floor: Some(crate::config::IsolationFloor::Off),
        on_floor_miss: crate::config::OnFloorMiss::Degrade,
        ..Default::default()
    };
    let loc = crate::remote::GitLoc::from_db("/wt/x", None);
    let resolved = resolve_placed_exact(
        &cfg,
        &loc,
        "test",
        SandboxProfile::Hardened,
        Placement::Ssh(SshPlacement::plain(
            "host".into(),
            22,
            false,
            TransportKind::Ssh,
        )),
    )
    .expect("explicit remote host backend should resolve without probing");
    assert_eq!(resolved.network, Network::None);
    assert!(
        resolved.vpn.is_none(),
        "universal deny must suppress VPN egress"
    );

    let mut oci = spec(Backend::Podman);
    oci.network = resolved.network;
    oci.vpn = resolved.vpn;
    let opts = oci_create_opts(&oci).expect("valid volume names");
    assert!(
        opts.windows(2)
            .any(|pair| pair[0] == "--network" && pair[1] == "none"),
        "{opts:?}"
    );
    assert!(
        !opts.iter().any(|arg| arg.starts_with("container:")),
        "{opts:?}"
    );
}

#[cfg(unix)]
#[test]
fn vpn_staging_sweep_removes_old_unlisted_and_gc_removed_sidecars() {
    use std::time::{Duration, SystemTime};
    let parent = private_tempdir();
    let vpn = parent.path().join("thegn/vpn");
    std::fs::create_dir_all(&vpn).unwrap();
    let now = SystemTime::now();
    let age = Duration::from_secs(60);
    let old_unlisted = vpn.join("a-tgvpn");
    let fresh_unlisted = vpn.join("b-tgvpn");
    let old_listed = vpn.join("c-tgvpn");
    let fresh_removed = vpn.join("d-tgvpn");
    let other = vpn.join("e");
    let nested_residue = vpn.join("f-tgvpn");
    for dir in [
        &old_unlisted,
        &fresh_unlisted,
        &old_listed,
        &fresh_removed,
        &other,
        &nested_residue,
    ] {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("secret"), b"staged").unwrap();
    }
    std::fs::create_dir(nested_residue.join("nested")).unwrap();
    std::fs::File::open(&old_unlisted)
        .unwrap()
        .set_modified(now - Duration::from_secs(120))
        .unwrap();
    std::fs::File::open(&old_listed)
        .unwrap()
        .set_modified(now - Duration::from_secs(120))
        .unwrap();
    std::fs::File::open(&fresh_removed)
        .unwrap()
        .set_modified(now)
        .unwrap();
    std::fs::File::open(&nested_residue)
        .unwrap()
        .set_modified(now - Duration::from_secs(120))
        .unwrap();
    let listed = std::collections::HashSet::from(["c-tgvpn".to_string()]);
    assert_eq!(
        sweep_orphan_vpn_staging_in(
            parent.path(),
            std::path::Path::new("thegn/vpn"),
            Some(&listed),
            &["d-tgvpn".to_string()],
            now,
            age,
        ),
        2
    );
    assert!(!old_unlisted.exists());
    assert!(fresh_unlisted.is_dir());
    assert!(old_listed.is_dir());
    assert!(!fresh_removed.exists());
    assert!(other.is_dir());
    assert!(!nested_residue.join("secret").exists());
    assert!(nested_residue.is_dir());
    assert!(nested_residue.join("nested").is_dir());
}

#[cfg(unix)]
#[test]
fn vpn_staging_sweep_removes_nothing_when_no_runtime_was_listed() {
    use std::time::{Duration, SystemTime};
    let parent = private_tempdir();
    let vpn = parent.path().join("thegn/vpn");
    std::fs::create_dir_all(&vpn).unwrap();
    let old_unlisted = vpn.join("old-tgvpn");
    let removed = vpn.join("removed-tgvpn");
    for dir in [&old_unlisted, &removed] {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("secret"), b"staged").unwrap();
        std::fs::File::open(dir)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
    }
    assert_eq!(
        sweep_orphan_vpn_staging_in(
            parent.path(),
            std::path::Path::new("thegn/vpn"),
            None,
            &["removed-tgvpn".to_string()],
            SystemTime::now(),
            Duration::from_secs(60),
        ),
        1
    );
    assert!(old_unlisted.is_dir());
    assert!(!removed.exists());
}

#[cfg(unix)]
#[test]
fn vpn_staging_sweep_never_follows_a_symlinked_entry() {
    use std::os::unix::fs::symlink;
    use std::time::{Duration, SystemTime};
    let parent = private_tempdir();
    let vpn = parent.path().join("thegn/vpn");
    let outside = parent.path().join("outside");
    std::fs::create_dir_all(&vpn).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let canary = outside.join("secret");
    std::fs::write(&canary, b"canary").unwrap();
    symlink(&outside, vpn.join("c-tgvpn")).unwrap();
    sweep_orphan_vpn_staging_in(
        parent.path(),
        std::path::Path::new("thegn/vpn"),
        None,
        &["c-tgvpn".to_string()],
        SystemTime::now(),
        Duration::from_secs(60),
    );
    assert_eq!(std::fs::read(canary).unwrap(), b"canary");
}

#[cfg(unix)]
#[test]
fn secret_env_file_write_failure_returns_none_and_leaves_nothing() {
    let parent = private_tempdir();
    let key = String::from("GH_TOKEN");
    let value = String::from("canary-secret");

    let path =
        crate::fsperm::with_private_write_fault(crate::fsperm::PrivateWriteFault::Write, || {
            write_secret_env_file_in(
                parent.path(),
                std::path::Path::new("sandbox-env"),
                "launch",
                &[(&key, &value)],
                true,
            )
        });

    assert!(path.is_none());
    let env_dir = parent.path().join("sandbox-env");
    for entry in std::fs::read_dir(&env_dir).unwrap() {
        let entry = entry.unwrap();
        let bytes = std::fs::read(entry.path()).unwrap();
        assert!(
            !bytes
                .windows(value.len())
                .any(|window| window == value.as_bytes())
        );
        assert!(
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".secret.tmp-")
        );
    }
}

#[test]
fn compose_project_adoption_record_is_owner_checked_and_rejects_symlinks() {
    let state_home = tempfile::tempdir().unwrap();
    let mut sandbox = spec(Backend::Docker);
    sandbox.name = "thegn-collision".into();
    sandbox.worktree = PathBuf::from("/w/my.repo");
    let hashed = crate::sandbox_compose::hashed_project_name(&sandbox.name, &sandbox.worktree);
    let record_dir = state_home.path().join("thegn/compose-projects");
    std::fs::create_dir_all(&record_dir).unwrap();
    let record = record_dir.join(format!("{hashed}.adopted"));

    assert_eq!(
        crate::sandbox_compose::project_for_in(state_home.path(), &sandbox),
        hashed
    );
    std::fs::write(&record, "thegn-collision\n").unwrap();
    assert_eq!(
        crate::sandbox_compose::project_for_in(state_home.path(), &sandbox),
        "thegn-collision"
    );
    std::fs::write(&record, "another-project\n").unwrap();
    assert_eq!(
        crate::sandbox_compose::project_for_in(state_home.path(), &sandbox),
        hashed
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let target = record_dir.join("target");
        std::fs::write(&target, "thegn-collision").unwrap();
        std::fs::remove_file(&record).unwrap();
        symlink(target, &record).unwrap();
        assert_eq!(
            crate::sandbox_compose::project_for_in(state_home.path(), &sandbox),
            hashed
        );
    }
}

#[test]
fn compose_legacy_project_probe_adopts_only_component_owned_projects() {
    let state_home = tempfile::tempdir().unwrap();
    let mut sandbox = spec(Backend::Docker);
    sandbox.name = "thegn-collision".into();
    sandbox.worktree = PathBuf::from("/w/my.repo");
    let target = crate::sandbox_compose::ComposeTarget::for_spec(&sandbox).unwrap();
    let expected_ps = vec![
        "docker".to_string(),
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        "label=com.docker.compose.project=thegn-collision".into(),
        "--format".into(),
        "{{.Names}}".into(),
    ];
    let observed = std::sync::Mutex::new(Vec::new());
    let project = crate::sandbox_compose::resolve_project_with(
        state_home.path(),
        &sandbox,
        &target,
        |argv| {
            observed.lock().unwrap().push(argv.to_vec());
            if argv == expected_ps.as_slice() {
                Some((true, "legacy-web-1\n".into()))
            } else {
                Some((true, "/w/my.repo/.devcontainer\n".into()))
            }
        },
    );
    assert_eq!(project, sandbox.name);
    let observed = observed.lock().unwrap();
    assert_eq!(observed.len(), 2);
    assert_eq!(observed[0], expected_ps);
    assert!(observed[1].windows(2).any(|args| {
        args[0] == "--format"
            && args[1] == "{{index .Config.Labels \"com.docker.compose.project.working_dir\"}}"
    }));
    assert!(observed[1].contains(&"legacy-web-1".to_string()));
    drop(observed);
    assert_eq!(
        crate::sandbox_compose::project_for_in(state_home.path(), &sandbox),
        sandbox.name
    );

    let foreign_state = tempfile::tempdir().unwrap();
    let foreign = crate::sandbox_compose::resolve_project_with(
        foreign_state.path(),
        &sandbox,
        &target,
        |argv| {
            if argv == expected_ps.as_slice() {
                Some((true, "legacy-web-1\n".into()))
            } else {
                Some((true, "/w/my-repo/.devcontainer\n".into()))
            }
        },
    );
    assert_eq!(
        foreign,
        crate::sandbox_compose::hashed_project_name(&sandbox.name, &sandbox.worktree)
    );
    assert_eq!(
        crate::sandbox_compose::project_for_in(foreign_state.path(), &sandbox),
        foreign
    );

    let failed_state = tempfile::tempdir().unwrap();
    let failed = crate::sandbox_compose::resolve_project_with(
        failed_state.path(),
        &sandbox,
        &target,
        |_| None,
    );
    assert_eq!(failed, foreign);
    assert_eq!(
        crate::sandbox_compose::project_for_in(failed_state.path(), &sandbox),
        failed
    );

    let empty_state = tempfile::tempdir().unwrap();
    let empty =
        crate::sandbox_compose::resolve_project_with(empty_state.path(), &sandbox, &target, |_| {
            Some((true, String::new()))
        });
    assert_eq!(empty, failed);

    let inspect_failed_state = tempfile::tempdir().unwrap();
    let inspect_calls = std::cell::Cell::new(0);
    let inspect_failed = crate::sandbox_compose::resolve_project_with(
        inspect_failed_state.path(),
        &sandbox,
        &target,
        |_| {
            let call = inspect_calls.get();
            inspect_calls.set(call + 1);
            (call == 0).then(|| (true, "legacy-web-1\n".into()))
        },
    );
    assert_eq!(inspect_failed, failed);
    assert_eq!(inspect_calls.get(), 2);

    let malformed_state = tempfile::tempdir().unwrap();
    let malformed_dir = malformed_state.path().join("thegn/compose-projects");
    std::fs::create_dir_all(&malformed_dir).unwrap();
    let malformed_record = malformed_dir.join(format!(
        "{}.adopted",
        crate::sandbox_compose::hashed_project_name(&sandbox.name, &sandbox.worktree)
    ));
    std::fs::write(&malformed_record, "other-project").unwrap();
    let malformed_calls = std::cell::Cell::new(0);
    let malformed = crate::sandbox_compose::resolve_project_with(
        malformed_state.path(),
        &sandbox,
        &target,
        |_| {
            malformed_calls.set(malformed_calls.get() + 1);
            None
        },
    );
    assert_eq!(malformed, failed);
    assert_eq!(malformed_calls.get(), 0);

    let write_failed_state = tempfile::tempdir().unwrap();
    std::fs::write(
        write_failed_state.path().join("thegn"),
        "blocking directory",
    )
    .unwrap();
    let write_failed = crate::sandbox_compose::resolve_project_with(
        write_failed_state.path(),
        &sandbox,
        &target,
        |argv| {
            if argv == expected_ps.as_slice() {
                Some((true, "legacy-web-1\n".into()))
            } else {
                Some((true, "/w/my.repo/.devcontainer\n".into()))
            }
        },
    );
    assert_eq!(write_failed, failed);

    let adopted =
        crate::sandbox_compose::resolve_project_with(state_home.path(), &sandbox, &target, |_| {
            panic!("adoption record should suppress repeat probes")
        });
    assert_eq!(adopted, sandbox.name);
}

#[test]
fn compose_teardown_projects_add_legacy_only_for_adopted_worktree() {
    let state_home = tempfile::tempdir().unwrap();
    let worktree = PathBuf::from("/w/my.repo");
    let legacy = container_name(&worktree.to_string_lossy());
    let hashed = crate::sandbox_compose::hashed_project_name(&legacy, &worktree);
    assert_eq!(
        crate::sandbox_compose::teardown_projects_in(state_home.path(), &worktree),
        vec![hashed.clone()]
    );
    let directory = crate::fsperm::ensure_private_dir_beneath(
        state_home.path(),
        std::path::Path::new("thegn/compose-projects"),
    )
    .unwrap();
    directory
        .write_owner_only_atomic(&format!("{hashed}.adopted"), legacy.as_bytes())
        .unwrap();
    assert_eq!(
        crate::sandbox_compose::teardown_projects_in(state_home.path(), &worktree),
        vec![hashed, legacy]
    );
}

#[test]
fn remote_teardown_downs_the_hashed_project_through_the_placement() {
    let state_home = tempfile::tempdir().unwrap();
    let loc = crate::remote::GitLoc::Remote {
        ssh: crate::remote::SshTarget::plain("remote.example".into(), 22, false),
        path: "/remote/wt".into(),
    };
    let name = container_name("/host/wt");
    let projects = teardown_projects_for_loc(state_home.path(), &loc, &name, true);
    let loc_project =
        crate::sandbox_compose::hashed_project_name(&name, std::path::Path::new("/remote/wt"));
    let mounted_project =
        crate::sandbox_compose::hashed_project_name(&name, std::path::Path::new("/workspace"));
    assert_eq!(projects, vec![loc_project.clone(), mounted_project.clone()]);

    let placement = Placement::Ssh(SshPlacement::plain(
        "remote.example".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let commands = teardown_argvs(Backend::Docker, None, &name, &projects);
    let routed_commands: Vec<_> = commands
        .iter()
        .map(|argv| placement.control_argv(argv))
        .collect();
    assert!(
        routed_commands
            .iter()
            .all(|argv| argv.first().is_some_and(|arg| arg == "ssh"))
    );
    let down_commands: Vec<_> = commands
        .iter()
        .filter(|argv| argv.iter().any(|arg| arg == "down"))
        .collect();
    assert_eq!(down_commands.len(), 2);
    let has_project = |project: &str| {
        down_commands.iter().any(|argv| {
            argv.windows(2)
                .any(|pair| pair[0] == "-p" && pair[1] == project)
        })
    };
    assert!(has_project(&loc_project));
    assert!(has_project(&mounted_project));
    assert!(!has_project(&name));
}

const PODMAN5_STATS: &str = r#"[
  {"id":"a1b2c3d4e5f6","name":"gamehost-mailpit-1","cpu_time":"0.00s","cpu_percent":"0.02%","avg_cpu":"0.02%","mem_usage":"10.19MB / 100.5GB","mem_percent":"0.01%","net_io":"22.22kB / 7.348kB","block_io":"0B / 0B","pids":"3"},
  {"id":"f6e5d4c3b2a1","name":"gamehost-db-1","cpu_time":"0.01s","cpu_percent":"0.80%","avg_cpu":"0.80%","mem_usage":"51.13MB / 100.5GB","mem_percent":"0.05%","net_io":"21.7kB / 7.348kB","block_io":"0B / 0B","pids":"12"}
]"#;

#[test]
fn parse_stats_rows_reads_podman5_json() {
    let rows = parse_stats_rows(PODMAN5_STATS);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        (
            "gamehost-mailpit-1".into(),
            ContainerStat {
                cpu: "0.02%".into(),
                mem: "10.19MB".into(),
                net: "22.22kB / 7.348kB".into(),
            }
        )
    );
    assert_eq!(
        rows[1],
        (
            "gamehost-db-1".into(),
            ContainerStat {
                cpu: "0.80%".into(),
                mem: "51.13MB".into(),
                net: "21.7kB / 7.348kB".into(),
            }
        )
    );
}

#[test]
fn parse_stats_rows_reads_docker_ndjson() {
    let rows = parse_stats_rows(
        r#"{"BlockIO":"4kB / 0B","CPUPerc":"1.25%","Container":"abc","ID":"abc","MemPerc":"0.1%","MemUsage":"15.5MiB / 7.6GiB","Name":"web","NetIO":"1.2kB / 648B","PIDs":"2"}
{"BlockIO":"0B / 0B","CPUPerc":"0.50%","Container":"def","ID":"def","MemPerc":"0.2%","MemUsage":"20MiB / 7.6GiB","Name":"worker","NetIO":"2kB / 1kB","PIDs":"1"}"#,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "web");
    assert_eq!(rows[0].1.cpu, "1.25%");
    assert_eq!(rows[0].1.mem, "15.5MiB");
    assert_eq!(rows[0].1.net, "1.2kB / 648B");
}

#[test]
fn container_listing_with_stats_joins_podman5_stats_by_name() {
    let containers = running_containers_with(
        true,
        |backend| match backend {
            Backend::Podman => RuntimeProbe::Present,
            _ => RuntimeProbe::Absent,
        },
        |backend, _| {
            assert_eq!(backend, Backend::Podman);
            Some(r#"[{"Names":["gamehost-db-1"],"Image":"postgres","Status":"Up"}]"#.into())
        },
        |_| parse_stats_rows(PODMAN5_STATS).into_iter().collect(),
    );
    assert_eq!(containers.len(), 1);
    assert_eq!(containers[0].name, "gamehost-db-1");
    assert_eq!(containers[0].cpu, "0.80%");
    assert_eq!(containers[0].mem, "51.13MB");
}

#[test]
fn parse_stats_rows_drops_rows_without_a_name() {
    assert!(parse_stats_rows(r#"[{"cpu_percent":"0.80%","mem_usage":"1MB / 2GB"}]"#).is_empty());
}

#[test]
fn placed_teardown_routes_every_command_through_the_ssh_placement() {
    let state = crate::testenv::IsolatedState::new();
    let loc = crate::remote::GitLoc::Remote {
        ssh: crate::remote::SshTarget::plain("remote.example".into(), 22, false),
        path: "/remote/wt".into(),
    };
    let placement = Placement::Ssh(SshPlacement::plain(
        "remote.example".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let name = container_name("/host/wt");
    let expected_projects = [
        crate::sandbox_compose::hashed_project_name(&name, std::path::Path::new("/remote/wt")),
        crate::sandbox_compose::hashed_project_name(&name, std::path::Path::new("/workspace")),
    ];
    let mut calls = Vec::new();
    let result = teardown_placed_with(
        &SandboxConfig {
            enabled: true,
            ..SandboxConfig::default()
        },
        &placement,
        &loc,
        &name,
        &state.state_home(),
        |backend| {
            if backend == Backend::Docker {
                RuntimeProbe::Present
            } else {
                RuntimeProbe::Absent
            }
        },
        |called_placement, argv, timeout| {
            calls.push((called_placement.clone(), argv.to_vec(), timeout));
            Some(true)
        },
    );
    assert!(result.is_ok());
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|(called_placement, argv, _)| {
        matches!(called_placement, Placement::Ssh(_))
            && called_placement
                .control_argv(argv)
                .first()
                .is_some_and(|arg| arg == "ssh")
    }));
    let down_commands: Vec<_> = calls
        .iter()
        .filter(|(_, argv, _)| argv.iter().any(|arg| arg == "down"))
        .collect();
    assert!(!down_commands.is_empty());
    assert!(down_commands.iter().all(|(_, argv, _)| {
        argv.windows(2).any(|pair| {
            pair[0] == "-p"
                && expected_projects
                    .iter()
                    .any(|project| pair[1] == project.as_str())
                && pair[1] != name.as_str()
        })
    }));
}

#[test]
fn placed_teardown_reports_an_unreachable_remote_host() {
    let state = crate::testenv::IsolatedState::new();
    let loc = crate::remote::GitLoc::Remote {
        ssh: crate::remote::SshTarget::plain("remote.example".into(), 22, false),
        path: "/remote/wt".into(),
    };
    let placement = Placement::Ssh(SshPlacement::plain(
        "remote.example".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    let cfg = SandboxConfig {
        enabled: true,
        ..SandboxConfig::default()
    };
    let name = container_name("/host/wt");
    let absent = teardown_placed_with(
        &cfg,
        &placement,
        &loc,
        &name,
        &state.state_home(),
        |_| RuntimeProbe::Absent,
        |_, _, _| Some(true),
    );
    assert!(matches!(absent, Err(PlacedTeardownError::NoRuntime)));

    let unreachable = teardown_placed_with(
        &cfg,
        &placement,
        &loc,
        &name,
        &state.state_home(),
        |_| RuntimeProbe::Present,
        |_, _, _| None,
    );
    assert!(matches!(
        unreachable,
        Err(PlacedTeardownError::Unreachable { commands }) if commands > 0
    ));

    let missing_container = teardown_placed_with(
        &cfg,
        &placement,
        &loc,
        &name,
        &state.state_home(),
        |_| RuntimeProbe::Present,
        |_, _, _| Some(false),
    );
    assert!(missing_container.is_ok());

    let disabled = teardown_placed_with(
        &SandboxConfig {
            enabled: false,
            ..SandboxConfig::default()
        },
        &placement,
        &loc,
        &name,
        &state.state_home(),
        |_| panic!("disabled sandbox must not probe runtimes"),
        |_, _, _| panic!("disabled sandbox must not run teardown commands"),
    );
    assert!(disabled.is_ok());
}

#[test]
fn sealed_systemd_enter_is_refused_strict() {
    let mut sealed = spec(Backend::Systemd);
    sealed.seal_home = Some("/home/u".into());
    assert!(matches!(
        enter_argv(&sealed, "sh"),
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::StrictUnsupported
    ));
}

#[test]
fn control_socket_bwrap_masks_a_visible_control_dir_after_the_binds() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mount = temp.path().to_path_buf();
    let control_dir = mount.join("run/thegn");
    std::fs::create_dir_all(&control_dir).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: mount.display().to_string(),
        dest: mount.display().to_string(),
        ro: true,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: None,
            control_dirs: vec![control_dir.clone()],
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    let mask = argv
        .windows(2)
        .position(|pair| pair[0] == "--tmpfs" && pair[1] == control_dir.display().to_string())
        .unwrap();
    let last_bind = argv
        .iter()
        .enumerate()
        .filter(|(_, arg)| arg.as_str() == "--bind" || arg.as_str() == "--ro-bind")
        .map(|(index, _)| index)
        .max()
        .unwrap();
    let command = argv.iter().position(|arg| arg == "--").unwrap();
    assert!(mask > last_bind, "control tmpfs follows every bind");
    assert!(
        mask < command,
        "control tmpfs precedes the command separator"
    );
}

#[test]
fn control_socket_bwrap_refuses_a_mask_that_would_hide_a_bind() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mount = temp.path().to_path_buf();
    let control_dir = mount.join("run");
    std::fs::create_dir(&control_dir).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: mount.display().to_string(),
        dest: mount.display().to_string(),
        ro: true,
        cache: false,
    });
    sandbox.mounts.push(Mount {
        host: mount.join("source").display().to_string(),
        dest: mount.join("run/child").display().to_string(),
        ro: true,
        cache: false,
    });
    assert!(matches!(
        backend_enter_argv_with_env(
            &sandbox,
            "true",
            SandboxArgvEnv {
                resolver_dir: None,
                runtime_dir: None,
                control_dirs: vec![control_dir],
                protected_dirs: Vec::new(),
            },
        ),
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::ControlSocketReachable
    ));
}

#[test]
fn control_socket_bwrap_translates_a_host_dir_to_a_renamed_mount() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let control_dir = temp.path().join("run/thegn");
    std::fs::create_dir_all(&control_dir).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: temp.path().display().to_string(),
        dest: "/data".into(),
        ro: true,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: None,
            control_dirs: vec![control_dir],
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| pair[0] == "--tmpfs" && pair[1] == "/data/run/thegn")
    );
}

#[test]
fn control_socket_systemd_adds_inaccessible_paths_for_published_dirs() {
    let mut sandbox = spec(Backend::Systemd);
    sandbox.read_only_root = true;
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: Some("/run/user/42".into()),
            control_dirs: vec!["/srv/ctl".into()],
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        argv.iter()
            .any(|arg| arg == "InaccessiblePaths=-/run/user/42/thegn")
    );
    assert!(argv.iter().any(|arg| arg == "InaccessiblePaths=-/srv/ctl"));
}

#[test]
fn control_socket_systemd_refuses_a_dir_that_hides_the_worktree() {
    let mut sandbox = spec(Backend::Systemd);
    sandbox.read_only_root = true;
    sandbox.worktree = "/wt/feat".into();
    assert!(matches!(
        backend_enter_argv_with_env(
            &sandbox,
            "true",
            SandboxArgvEnv {
                resolver_dir: None,
                runtime_dir: None,
                control_dirs: vec!["/wt".into()],
                protected_dirs: Vec::new(),
            },
        ),
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::ControlSocketReachable
    ));
}

#[test]
fn control_socket_systemd_leaves_private_tmp_unmasked() {
    let mut sandbox = spec(Backend::Systemd);
    sandbox.read_only_root = true;
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: None,
            control_dirs: vec!["/tmp".into()],
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(!argv.iter().any(|arg| arg == "InaccessiblePaths=-/tmp"));
}

#[test]
fn control_socket_label_and_remediation_are_stable() {
    assert_eq!(
        RefusalClass::ControlSocketReachable.label(),
        "control-socket-reachable"
    );
    assert_eq!(
        RefusalClass::ControlSocketReachable.remediation(),
        "move [daemon] socket and $XDG_RUNTIME_DIR out of $HOME, the worktree and every sandbox mount (the default /run/user/<uid> is never mounted)"
    );
}

#[test]
fn control_socket_oci_mount_covering_a_control_dir_is_refused() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let control_dir = temp.path().join("run/thegn");
    std::fs::create_dir_all(&control_dir).unwrap();
    for backend in [Backend::Podman, Backend::Docker] {
        let mut sandbox = spec(backend);
        sandbox.mounts.push(Mount {
            host: temp.path().display().to_string(),
            dest: temp.path().display().to_string(),
            ro: true,
            cache: false,
        });
        let result = launchable_with(&sandbox, std::slice::from_ref(&control_dir));
        let expected_path = control_dir.display().to_string();
        assert!(matches!(
            result,
            Err(EnterError::Refused(refusal))
                if refusal.class == RefusalClass::ControlSocketReachable
                    && refusal.backend == sandbox.backend.label()
                    && refusal.path.as_deref() == Some(expected_path.as_str())
        ));
    }
}

#[test]
fn control_socket_compose_spec_mount_covering_a_control_dir_is_refused() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let control_dir = temp.path().join("run/thegn");
    std::fs::create_dir_all(&control_dir).unwrap();
    let mut sandbox = spec(Backend::Docker);
    sandbox.mounts.push(Mount {
        host: temp.path().display().to_string(),
        dest: temp.path().display().to_string(),
        ro: true,
        cache: false,
    });
    sandbox.compose = Some(
        crate::sandbox_compose::ComposeSpec {
            files: vec!["compose.yml".into()],
            service: Some("app".into()),
            run_services: Vec::new(),
        }
        .encode(),
    );
    let result = launchable_with(&sandbox, std::slice::from_ref(&control_dir));
    let expected_path = control_dir.display().to_string();
    assert!(matches!(
        result,
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::ControlSocketReachable
                && refusal.backend == sandbox.backend.label()
                && refusal.path.as_deref() == Some(expected_path.as_str())
    ));
}

#[test]
fn control_socket_oci_mount_inside_a_control_dir_is_refused() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let control_dir = temp.path().join("run/thegn");
    let source = control_dir.join("key");
    std::fs::create_dir_all(&source).unwrap();
    let mut sandbox = spec(Backend::Podman);
    sandbox.mounts.push(Mount {
        host: source.display().to_string(),
        dest: "/data".into(),
        ro: true,
        cache: false,
    });
    let result = launchable_with(&sandbox, std::slice::from_ref(&control_dir));
    let expected_path = control_dir.display().to_string();
    assert!(matches!(
        result,
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::ControlSocketReachable
                && refusal.path.as_deref() == Some(expected_path.as_str())
    ));
}

#[test]
fn control_socket_bwrap_refuses_a_bind_sourced_inside_a_control_dir() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let control_dir = temp.path().join("run/thegn");
    let source = control_dir.join("key");
    std::fs::create_dir_all(&source).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: source.display().to_string(),
        dest: "/data".into(),
        ro: true,
        cache: false,
    });
    assert!(matches!(
        backend_enter_argv_with_env(
            &sandbox,
            "true",
            SandboxArgvEnv {
                resolver_dir: None,
                runtime_dir: None,
                control_dirs: vec![control_dir],
                protected_dirs: Vec::new(),
            },
        ),
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::ControlSocketReachable
    ));
}

#[test]
fn control_socket_oci_file_access_all_and_host_are_exempt() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let control_dir = temp.path().join("run/thegn");
    std::fs::create_dir_all(&control_dir).unwrap();
    for file_access in [FileAccess::All, FileAccess::Host] {
        let mut sandbox = spec(Backend::Podman);
        sandbox.file_access = file_access;
        sandbox.mounts.push(Mount {
            host: temp.path().display().to_string(),
            dest: temp.path().display().to_string(),
            ro: true,
            cache: false,
        });
        assert!(!matches!(
            launchable_with(&sandbox, std::slice::from_ref(&control_dir)),
            Err(EnterError::Refused(refusal))
                if refusal.class == RefusalClass::ControlSocketReachable
        ));
    }
}

// Positive control: unrelated control directories do not block OCI launches.
#[test]
fn control_socket_oci_control_dir_outside_every_mount_is_clear() {
    let mount = tempfile::tempdir_in("/tmp").unwrap();
    let control = tempfile::tempdir_in("/tmp").unwrap();
    let control_dir = control.path().join("run/thegn");
    std::fs::create_dir_all(&control_dir).unwrap();
    let mut sandbox = spec(Backend::Docker);
    sandbox.mounts.push(Mount {
        host: mount.path().display().to_string(),
        dest: "/data".into(),
        ro: true,
        cache: false,
    });
    assert!(!matches!(
        launchable_with(&sandbox, std::slice::from_ref(&control_dir)),
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::ControlSocketReachable
    ));
}

#[cfg(unix)]
#[test]
fn control_socket_oci_refuses_a_control_dir_symlinked_into_a_mount() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mount = temp.path().join("mnt");
    let mounted_control = mount.join("ctl");
    let runtime = temp.path().join("rt");
    let control_dir = runtime.join("thegn");
    std::fs::create_dir_all(&mounted_control).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    symlink(&mounted_control, &control_dir).unwrap();

    for backend in [Backend::Podman, Backend::Docker] {
        let mut sandbox = spec(backend);
        sandbox.mounts.push(Mount {
            host: mount.display().to_string(),
            dest: mount.display().to_string(),
            ro: true,
            cache: false,
        });
        assert!(matches!(
            launchable_with(&sandbox, std::slice::from_ref(&control_dir)),
            Err(EnterError::Refused(refusal))
                if refusal.class == RefusalClass::ControlSocketReachable
        ));
    }
}

#[cfg(unix)]
#[test]
fn control_socket_bwrap_masks_a_control_dir_seen_through_a_symlinked_mount_host() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let real = temp.path().join("real");
    let control_dir = real.join("run/thegn");
    let mount_host = temp.path().join("hostlink");
    std::fs::create_dir_all(&control_dir).unwrap();
    symlink(&real, &mount_host).unwrap();

    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: mount_host.display().to_string(),
        dest: "/data".into(),
        ro: true,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            resolver_dir: None,
            runtime_dir: None,
            control_dirs: vec![control_dir],
            protected_dirs: Vec::new(),
        },
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| { pair[0] == "--tmpfs" && pair[1] == "/data/run/thegn" })
    );
}

// Compile/wiring check only: in this test process the resolved list may be empty,
// so this does not prove that each configured control dir is added to the guard.
#[test]
fn control_socket_host_compose_guard_wiring_compile_check() {
    let guard = host_compose_guard();
    assert!(
        crate::sandbox_control_guard::control_dirs()
            .iter()
            .all(|control_dir| guard
                .protected
                .iter()
                .any(|protected| { control_dir.starts_with(protected) }))
    );
}

#[test]
fn control_socket_oci_read_only_mount_covering_the_run_dir_is_admitted() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let run = temp.path().join("state/thegn/run");
    std::fs::create_dir_all(run.parent().unwrap()).unwrap();
    std::fs::create_dir(&run).unwrap();
    for backend in [Backend::Podman, Backend::Docker] {
        let mut sandbox = spec(backend);
        sandbox.mounts.push(Mount {
            host: temp.path().display().to_string(),
            dest: temp.path().display().to_string(),
            ro: true,
            cache: false,
        });
        assert!(launchable_guarded(&sandbox, &[], std::slice::from_ref(&run)).is_ok());
    }
}

#[test]
fn control_socket_oci_writable_mount_covering_the_run_dir_is_refused() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let run = temp.path().join("state/thegn/run");
    std::fs::create_dir_all(run.parent().unwrap()).unwrap();
    std::fs::create_dir(&run).unwrap();
    for backend in [Backend::Podman, Backend::Docker] {
        let mut sandbox = spec(backend);
        sandbox.mounts.push(Mount {
            host: temp.path().display().to_string(),
            dest: temp.path().display().to_string(),
            ro: false,
            cache: false,
        });
        let result = launchable_guarded(&sandbox, &[], std::slice::from_ref(&run));
        let expected = run.display().to_string();
        assert!(matches!(
            result,
            Err(EnterError::Refused(refusal))
                if refusal.class == RefusalClass::ControlSocketReachable
                    && refusal.path.as_deref() == Some(expected.as_str())
        ));
    }
}

#[test]
fn control_socket_bwrap_masks_a_prepared_run_dir_under_a_writable_root() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let run = temp.path().join("state/thegn/run");
    std::fs::create_dir_all(&run).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: temp.path().display().to_string(),
        dest: temp.path().display().to_string(),
        ro: false,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            protected_dirs: vec![run.clone()],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|pair| { pair[0] == "--tmpfs" && pair[1] == run.display().to_string() })
    );
}

#[test]
fn control_socket_bwrap_masks_exactly_the_protected_run_dir() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let run = temp.path().join("state/thegn/run");
    std::fs::create_dir_all(&run).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: temp.path().display().to_string(),
        dest: temp.path().display().to_string(),
        ro: true,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            protected_dirs: vec![run.clone()],
            ..Default::default()
        },
    )
    .unwrap();
    let mask = argv
        .windows(2)
        .position(|pair| pair[0] == "--tmpfs" && pair[1] == run.display().to_string())
        .unwrap();
    let last_bind = argv
        .iter()
        .enumerate()
        .filter(|(_, arg)| arg.as_str() == "--bind" || arg.as_str() == "--ro-bind")
        .map(|(index, _)| index)
        .max()
        .unwrap();
    let command = argv.iter().position(|arg| arg == "--").unwrap();
    assert!(mask > last_bind, "protected tmpfs follows every bind");
    assert!(
        mask < command,
        "protected tmpfs precedes the command separator"
    );
    for ancestor in [temp.path().join("state/thegn"), temp.path().join("state")] {
        assert!(
            !argv
                .windows(2)
                .any(|pair| { pair[0] == "--tmpfs" && pair[1] == ancestor.display().to_string() })
        );
    }
}

#[test]
fn control_socket_bwrap_leaves_an_absent_protected_dir_under_a_read_only_bind_unmasked() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let run = temp.path().join("state/thegn/run");
    std::fs::create_dir_all(run.parent().unwrap()).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: temp.path().display().to_string(),
        dest: temp.path().display().to_string(),
        ro: true,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            protected_dirs: vec![run],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!argv.windows(2).any(|pair| {
        pair[0] == "--tmpfs" && std::path::Path::new(&pair[1]).starts_with(temp.path())
    }));
}

#[test]
fn control_socket_bwrap_refuses_an_absent_protected_dir_under_a_writable_bind() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let run = temp.path().join("state/thegn/run");
    std::fs::create_dir_all(run.parent().unwrap()).unwrap();
    let mut sandbox = spec(Backend::Bwrap);
    sandbox.mounts.push(Mount {
        host: temp.path().display().to_string(),
        dest: temp.path().display().to_string(),
        ro: false,
        cache: false,
    });
    assert!(matches!(
        backend_enter_argv_with_env(
            &sandbox,
            "true",
            SandboxArgvEnv {
                protected_dirs: vec![run],
                ..Default::default()
            },
        ),
        Err(EnterError::Refused(refusal))
            if refusal.class == RefusalClass::ControlSocketReachable
    ));
}

#[test]
fn control_socket_systemd_makes_the_protected_dir_inaccessible() {
    let mut sandbox = spec(Backend::Systemd);
    sandbox.read_only_root = true;
    let argv = backend_enter_argv_with_env(
        &sandbox,
        "true",
        SandboxArgvEnv {
            protected_dirs: vec!["/srv/state/thegn/run".into()],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        argv.iter()
            .any(|arg| arg == "InaccessiblePaths=-/srv/state/thegn/run")
    );
}

#[path = "sandbox_evidence_tests.rs"]
mod evidence;

fn common_dir_mount_hosts(dir: &std::path::Path, init: bool) -> (Vec<String>, std::path::PathBuf) {
    if init {
        let ok = crate::util::git_cmd(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["init", "--quiet"])
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    let loc = crate::remote::GitLoc::Local(dir.to_path_buf());
    let want = loc
        .git_out_path(&["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .unwrap_or_default();
    let cfg = crate::config::SandboxConfig {
        enabled: true,
        backend: crate::config::SandboxBackend::None,
        isolation_floor: Some(crate::config::IsolationFloor::Off),
        on_floor_miss: crate::config::OnFloorMiss::Degrade,
        ..Default::default()
    };
    let spec = resolve_placed_exact(
        &cfg,
        &loc,
        "test",
        SandboxProfile::Hardened,
        Placement::Ssh(SshPlacement::plain(
            "host".into(),
            22,
            false,
            TransportKind::Ssh,
        )),
    )
    .expect("explicit remote host backend should resolve without probing");
    (spec.mounts.into_iter().map(|m| m.host).collect(), want)
}

#[test]
fn git_common_mount_uses_the_lossless_common_dir() {
    let dir = tempfile::tempdir().unwrap();
    let (hosts, want) = common_dir_mount_hosts(dir.path(), true);
    assert!(!want.as_os_str().is_empty());
    assert!(
        hosts.iter().any(|h| std::path::Path::new(h) == want),
        "{hosts:?} lacks {want:?}"
    );
}

#[test]
fn git_common_mount_is_omitted_outside_a_repo() {
    let dir = tempfile::tempdir().unwrap();
    let (hosts, want) = common_dir_mount_hosts(dir.path(), false);
    assert!(want.as_os_str().is_empty());
    assert!(!hosts.iter().any(|h| h.ends_with("/.git")), "{hosts:?}");
}

/// A fake linked worktree on disk for `git_policy_mounts`: `<td>/repo/.git`
/// (common dir with objects, refs, logs, config and `worktrees/lane`) and
/// `<td>/wt` (the worktree, with its `.git` gitfile).
struct GitFixture {
    _td: tempfile::TempDir,
    common: std::path::PathBuf,
    admin: std::path::PathBuf,
    wt: std::path::PathBuf,
}

fn git_fixture() -> GitFixture {
    let td = tempfile::tempdir().unwrap();
    let common = td.path().join("repo/.git");
    let admin = common.join("worktrees/lane");
    let wt = td.path().join("wt");
    for dir in [
        common.join("objects"),
        common.join("refs/heads"),
        common.join("hooks"),
        admin.clone(),
        wt.clone(),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(common.join("config"), "[core]\n").unwrap();
    std::fs::write(common.join("refs/heads/main"), "0\n").unwrap();
    std::fs::write(admin.join("commondir"), "../..\n").unwrap();
    std::fs::write(admin.join("gitdir"), "x\n").unwrap();
    std::fs::write(wt.join(".git"), "gitdir: x\n").unwrap();
    GitFixture {
        _td: td,
        common,
        admin,
        wt,
    }
}

fn git_probe(fx: &GitFixture, branch: &str, target: Option<&str>) -> GitProbe {
    GitProbe {
        common: fx.common.clone(),
        admin: Some(fx.admin.clone()),
        branch: Some(branch.to_string()),
        target: target.map(str::to_string),
        default: Some("main".to_string()),
        identity: Default::default(),
        ..Default::default()
    }
}

fn git_cfg(mode: crate::config::SandboxGitMode) -> crate::config::SandboxGitConfig {
    crate::config::SandboxGitConfig {
        mode,
        ..Default::default()
    }
}

fn bwrap_git_argv(fx: &GitFixture, mounts: Vec<Mount>) -> Vec<String> {
    let mut s = spec(Backend::Bwrap);
    s.worktree = fx.wt.clone();
    s.mounts = mounts;
    backend_enter_argv_with_env(&s, "true", SandboxArgvEnv::default()).unwrap()
}

fn triple_at(argv: &[String], flag: &str, path: &std::path::Path) -> Option<usize> {
    let p = path.display().to_string();
    argv.windows(3)
        .position(|t| t == [flag, p.as_str(), p.as_str()])
}

#[test]
fn git_policy_commit_only_binds_common_ro_then_lane_carve_outs() {
    use crate::config::SandboxGitMode;
    let fx = git_fixture();
    let (mounts, warning) = git_policy_mounts(
        &git_cfg(SandboxGitMode::CommitOnly),
        &fx.wt,
        &git_probe(&fx, "tg/lane", Some("main")),
    );
    assert_eq!(warning, None);
    // The plan's mkdirs ran on the host.
    assert!(fx.common.join("refs/heads/tg").is_dir());
    assert!(fx.common.join("logs/refs/heads/tg").is_dir());
    // Each carve-out holds the keep directory, so a ref prune cannot remove
    // the directory under a running sandbox's bind.
    assert!(fx.common.join("refs/heads/tg/.thegn-keep").is_dir());
    assert!(fx.common.join("logs/refs/heads/tg/.thegn-keep").is_dir());
    let argv = bwrap_git_argv(&fx, mounts.clone());
    let wt = triple_at(&argv, "--bind", &fx.wt).expect("worktree rw");
    let common_ro = triple_at(&argv, "--ro-bind", &fx.common).expect("common dir ro");
    let objects = triple_at(&argv, "--bind", &fx.common.join("objects")).expect("objects rw");
    let admin = triple_at(&argv, "--bind", &fx.admin).expect("admin dir rw");
    let heads = triple_at(&argv, "--bind", &fx.common.join("refs/heads/tg")).expect("lane refs rw");
    let config = triple_at(&argv, "--ro-bind", &fx.common.join("config")).expect("config pin");
    let gitfile = triple_at(&argv, "--ro-bind", &fx.wt.join(".git")).expect("gitfile pin");
    let commondir =
        triple_at(&argv, "--ro-bind", &fx.admin.join("commondir")).expect("commondir pin");
    assert!(wt < common_ro && common_ro < objects, "{argv:?}");
    assert!(
        objects < config && admin < config && heads < config,
        "{argv:?}"
    );
    assert!(config < gitfile && admin < commondir, "{argv:?}");
    // The missing config.worktree was created so its re-pin applies.
    assert!(fx.admin.join("config.worktree").exists());
    assert!(triple_at(&argv, "--bind", &fx.common).is_none(), "{argv:?}");
    // Nothing writable covers main's ref, hooks or the shared config.
    for protected in [
        "refs/heads/main",
        "hooks/pre-commit",
        "config",
        "packed-refs",
    ] {
        let path = fx.common.join(protected);
        assert!(
            !mounts
                .iter()
                .any(|m| !m.ro && path.starts_with(std::path::Path::new(&m.dest))),
            "{protected} is under a rw bind: {mounts:?}"
        );
    }
}

#[test]
fn alternates_pinned_ro() {
    use crate::config::SandboxGitMode;
    let fx = git_fixture();
    // No `objects/info/`: the pin must create it rather than skip the bind.
    assert!(!fx.common.join("objects/info").exists());
    for mode in [
        SandboxGitMode::CommitOnly,
        SandboxGitMode::PushLane,
        SandboxGitMode::Full,
    ] {
        let (mounts, _) = git_policy_mounts(
            &git_cfg(mode),
            &fx.wt,
            &git_probe(&fx, "tg/lane", Some("main")),
        );
        let argv = bwrap_git_argv(&fx, mounts.clone());
        let objects = triple_at(&argv, "--bind", &fx.common.join("objects")).expect("objects rw");
        for name in ["alternates", "http-alternates"] {
            let path = fx.common.join("objects/info").join(name);
            // Created empty so the read-only bind has a target.
            assert!(path.exists(), "{name}");
            let pin = triple_at(&argv, "--ro-bind", &path).unwrap_or_else(|| panic!("{name}"));
            assert!(
                objects < pin,
                "{name} pinned after the rw objects bind: {argv:?}"
            );
            // The mount list (lowered unchanged by OCI `:ro` and systemd
            // `ReadOnlyPaths`/`BindReadOnlyPaths`) carries it read-only.
            assert!(
                mounts
                    .iter()
                    .any(|m| m.ro && std::path::Path::new(&m.dest) == path),
                "{name}: {mounts:?}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn probe_object_store_reads_alternates_symlinks_and_commondir() {
    let fx = git_fixture();
    let clean = probe_object_store(&fx.common, Some(&fx.admin));
    assert_eq!(clean, (vec![], false, false));
    std::fs::create_dir_all(fx.common.join("objects/info")).unwrap();
    std::fs::write(
        fx.common.join("objects/info/alternates"),
        "# comment\n\n/shared/objects\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("/tmp", fx.common.join("objects/aa")).unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(
        fx.admin.join("commondir"),
        format!("{}\n", other.path().display()),
    )
    .unwrap();
    let (alts, symlinks, outside) = probe_object_store(&fx.common, Some(&fx.admin));
    assert_eq!(alts, vec!["/shared/objects".to_string()]);
    assert!(symlinks && outside);
    // git resolves `--git-common-dir` FROM `commondir`, so the launch path
    // probes with the planted dir as `common`; the admin dir's own location
    // still exposes the redirect.
    let (_, _, outside) = probe_object_store(other.path(), Some(&fx.admin));
    assert!(
        outside,
        "a redirected commondir is outside the admin dir's repo"
    );
}

#[cfg(unix)]
#[test]
fn probe_object_store_flags_a_symlinked_objects_dir() {
    let fx = git_fixture();
    let real = tempfile::tempdir().unwrap();
    std::fs::remove_dir_all(fx.common.join("objects")).unwrap();
    std::os::unix::fs::symlink(real.path(), fx.common.join("objects")).unwrap();
    let (alts, symlinks, outside) = probe_object_store(&fx.common, Some(&fx.admin));
    assert!(alts.is_empty() && symlinks && !outside);
}

#[test]
fn git_policy_read_only_makes_the_worktree_ro() {
    use crate::config::SandboxGitMode;
    let fx = git_fixture();
    let (mounts, warning) = git_policy_mounts(
        &git_cfg(SandboxGitMode::ReadOnly),
        &fx.wt,
        &git_probe(&fx, "tg/lane", Some("main")),
    );
    assert_eq!(warning, None);
    assert!(mounts.iter().all(|m| m.ro), "{mounts:?}");
    let argv = bwrap_git_argv(&fx, mounts);
    assert!(triple_at(&argv, "--ro-bind", &fx.wt).is_some(), "{argv:?}");
    assert!(
        triple_at(&argv, "--ro-bind", &fx.common).is_some(),
        "{argv:?}"
    );
    // Planting pin targets in the main git dir would break host git (an
    // empty `commondir` is fatal); a missing `info` must not become a file.
    for planted in ["commondir", "gitdir", "config.worktree", "info"] {
        assert!(!fx.common.join(planted).exists(), "{planted} was created");
    }
    assert!(
        triple_at(&argv, "--ro-bind", &fx.common.join("hooks")).is_some(),
        "{argv:?}"
    );
}

#[test]
fn git_policy_top_level_branch_falls_back_to_read_only_git_with_a_warning() {
    use crate::config::SandboxGitMode;
    let fx = git_fixture();
    let (mounts, warning) = git_policy_mounts(
        &git_cfg(SandboxGitMode::CommitOnly),
        &fx.wt,
        &git_probe(&fx, "feature", Some("main")),
    );
    let warning = warning.expect("fallback warns");
    assert!(warning.contains("top-level"), "{warning}");
    assert!(warning.contains("tg/<name>"), "{warning}");
    // The worktree stays writable; every git bind is read-only.
    assert_eq!(mounts[0].dest, fx.wt.to_string_lossy());
    assert!(!mounts[0].ro);
    assert!(mounts[1..].iter().all(|m| m.ro), "{mounts:?}");
    assert!(
        mounts
            .iter()
            .any(|m| m.ro && std::path::Path::new(&m.dest) == fx.common)
    );

    // mode = "full" keeps today's read-write common dir for the same branch.
    let (full, warning) = git_policy_mounts(
        &git_cfg(SandboxGitMode::Full),
        &fx.wt,
        &git_probe(&fx, "feature", Some("main")),
    );
    assert!(warning.is_some());
    assert!(
        full.iter()
            .any(|m| !m.ro && std::path::Path::new(&m.dest) == fx.common),
        "{full:?}"
    );
}

#[test]
fn git_policy_unknown_target_fails_closed_even_for_full() {
    use crate::config::SandboxGitMode;
    let fx = git_fixture();
    for mode in [SandboxGitMode::CommitOnly, SandboxGitMode::Full] {
        let (mounts, warning) =
            git_policy_mounts(&git_cfg(mode), &fx.wt, &git_probe(&fx, "tg/lane", None));
        assert!(warning.is_some(), "{mode}");
        assert!(!mounts[0].ro, "{mode}: worktree stays writable");
        assert!(mounts[1..].iter().all(|m| m.ro), "{mode}: {mounts:?}");
    }
}

#[test]
fn git_policy_main_checkout_gets_a_read_only_git_dir() {
    use crate::config::SandboxGitMode;
    let fx = git_fixture();
    let main = fx.common.parent().unwrap().to_path_buf();
    let probe = GitProbe {
        common: fx.common.clone(),
        admin: None,
        branch: Some("main".into()),
        target: Some("main".into()),
        default: Some("main".into()),
        identity: Default::default(),
        ..Default::default()
    };
    let (mounts, warning) = git_policy_mounts(&git_cfg(SandboxGitMode::CommitOnly), &main, &probe);
    assert!(warning.expect("warns").contains("main checkout"));
    assert!(!mounts[0].ro);
    assert!(
        mounts
            .iter()
            .any(|m| m.ro && std::path::Path::new(&m.dest) == fx.common),
        "{mounts:?}"
    );
    assert!(mounts[1..].iter().all(|m| m.ro), "{mounts:?}");
}

#[test]
fn bwrap_bind_dest_table() {
    // A fake filesystem: `/home/u/link` -> `/home/u/real`, `/alias` -> `/srv`
    // (an ancestor symlink), `/dangling` unresolvable.
    let resolve = |p: &std::path::Path| -> Option<PathBuf> {
        let s = p.to_string_lossy();
        if s == "/dangling" {
            return None;
        }
        for (alias, target) in [("/home/u/link", "/home/u/real"), ("/alias", "/srv")] {
            if let Ok(rest) = p.strip_prefix(alias) {
                return Some(std::path::Path::new(target).join(rest));
            }
        }
        Some(p.to_path_buf())
    };
    let roots = [
        PathBuf::from("/home/u"),
        PathBuf::from("/srv"),
        PathBuf::from("/alias"),
    ];
    let p = std::path::Path::new;
    // Plain dir: unchanged.
    assert_eq!(
        bwrap_bind_dest(p("/home/u/plain"), &roots, &resolve),
        DestFix::Keep
    );
    // Symlinked dest under a visible root: dropped, never followed.
    assert_eq!(
        bwrap_bind_dest(p("/home/u/link"), &roots, &resolve),
        DestFix::Drop
    );
    // Symlinked ancestor below the root: dropped too.
    assert_eq!(
        bwrap_bind_dest(p("/home/u/link/dir/leaf"), &roots, &resolve),
        DestFix::Drop
    );
    // The covering root is itself an alias (`/alias -> /srv`, like
    // `/home -> /var/home`), nothing below it is a link: kept.
    assert_eq!(
        bwrap_bind_dest(p("/alias/dir/leaf"), &roots, &resolve),
        DestFix::Keep
    );
    // Unresolvable: unchanged (the launch reports it as before).
    assert_eq!(
        bwrap_bind_dest(p("/dangling"), &roots, &resolve),
        DestFix::Keep
    );
    // Not under any visible root: bwrap creates it, alias kept.
    assert_eq!(
        bwrap_bind_dest(p("/uncovered/link"), &roots, &|_: &std::path::Path| {
            Some(PathBuf::from("/elsewhere"))
        }),
        DestFix::Keep
    );
}

#[test]
fn fix_bind_dests_keeps_uncovered_and_drops_covered() {
    // A mount's own (aliased) destination is not a root for itself.
    let mut s = spec(Backend::Bwrap);
    s.mounts = vec![Mount {
        host: "/src".into(),
        dest: "/link".into(),
        ro: false,
        cache: false,
    }];
    let resolve = |p: &std::path::Path| -> Option<PathBuf> {
        if p == std::path::Path::new("/link") {
            Some(PathBuf::from("/link/../target"))
        } else {
            Some(p.to_path_buf())
        }
    };
    fix_bind_dests(&mut s, &resolve);
    assert_eq!(s.mounts.len(), 1);
    assert_eq!(s.mounts[0].dest, "/link");

    // Under the bwrap substrate and symlinked: dropped, read-only or not.
    for ro in [true, false] {
        let mut s = spec(Backend::Bwrap);
        s.mounts = vec![Mount {
            host: "/src".into(),
            dest: "/etc/link".into(),
            ro,
            cache: false,
        }];
        let into_etc = |p: &std::path::Path| -> Option<PathBuf> {
            if p == std::path::Path::new("/etc/link") {
                Some(PathBuf::from("/etc/target"))
            } else {
                Some(p.to_path_buf())
            }
        };
        fix_bind_dests(&mut s, &into_etc);
        assert!(s.mounts.is_empty(), "{:?}", s.mounts);
    }
}

#[cfg(unix)]
#[test]
fn symlinked_writable_dest_is_skipped_not_followed() {
    use std::os::unix::fs::symlink;

    let td = private_tempdir();
    let base = td.path().canonicalize().unwrap();
    let source = base.join("source");
    let real = base.join("real");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&real).unwrap();
    let alias = base.join("alias");
    symlink(&real, &alias).unwrap();
    let base_s = base.to_string_lossy().into_owned();
    let source_s = source.to_string_lossy().into_owned();
    let real_s = real.to_string_lossy().into_owned();
    let alias_s = alias.to_string_lossy().into_owned();
    let mut s = spec(Backend::Bwrap);
    s.mounts = vec![
        Mount {
            host: base_s.clone(),
            dest: base_s.clone(),
            ro: true,
            cache: false,
        },
        Mount {
            host: source_s.clone(),
            dest: alias_s.clone(),
            ro: false,
            cache: false,
        },
    ];

    let argv = enter_argv(&s, "true").expect("a symlinked destination is skipped, not refused");
    assert!(!argv.contains(&alias_s), "{argv:?}");
    assert!(!argv.contains(&real_s), "{argv:?}");
    assert!(
        !argv
            .windows(2)
            .any(|w| w[0] == "--bind" && w[1] == source_s),
        "{argv:?}"
    );
}

#[cfg(unix)]
#[test]
fn aliased_home_keeps_worktree_bind_and_drops_link_below_it() {
    // `$HOME` reached through a system symlink (`/home -> /var/home`): the
    // read-only home mount and a writable worktree under it keep their path;
    // a link planted BELOW the home still drops its bind.
    use std::os::unix::fs::symlink;

    let td = private_tempdir();
    let base = td.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("real/home/wt")).unwrap();
    std::fs::create_dir_all(base.join("secret")).unwrap();
    symlink(base.join("real"), base.join("alias")).unwrap();
    let home = base.join("alias/home");
    symlink(base.join("secret"), home.join("tmp")).unwrap();
    let home_s = home.to_string_lossy().into_owned();
    let wt_s = home.join("wt").to_string_lossy().into_owned();
    let tmp_s = home.join("tmp").to_string_lossy().into_owned();
    let mut s = spec(Backend::Bwrap);
    s.mounts = vec![
        Mount {
            host: home_s.clone(),
            dest: home_s.clone(),
            ro: true,
            cache: false,
        },
        Mount {
            host: wt_s.clone(),
            dest: wt_s.clone(),
            ro: false,
            cache: false,
        },
        Mount {
            host: base.join("real/home/wt").to_string_lossy().into_owned(),
            dest: tmp_s.clone(),
            ro: false,
            cache: false,
        },
    ];

    let argv = enter_argv(&s, "true").expect("aliased home launches");
    assert!(argv.contains(&home_s), "{argv:?}");
    assert!(
        argv.windows(3).any(|w| w[0] == "--bind" && w[2] == wt_s),
        "worktree under the aliased home stays writable: {argv:?}"
    );
    assert!(!argv.contains(&tmp_s), "{argv:?}");
}

#[cfg(unix)]
#[test]
fn oci_symlinked_dest_is_not_rewritten() {
    use std::os::unix::fs::symlink;

    let td = private_tempdir();
    let base = td.path().canonicalize().unwrap();
    let real = base.join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = base.join("alias");
    symlink(&real, &alias).unwrap();
    let alias_s = alias.to_string_lossy().into_owned();
    let mut s = spec(Backend::Docker);
    s.mounts.push(Mount {
        host: real.to_string_lossy().into_owned(),
        dest: alias_s.clone(),
        ro: false,
        cache: false,
    });
    let planned = launchable(&s).unwrap();
    assert!(planned.mounts.iter().any(|m| m.dest == alias_s));
}

#[test]
fn local_bwrap_argv_carries_the_git_identity() {
    // The value is unique, so it never equals the test process's own env (the
    // bwrap emitter omits a pair the launcher already exports).
    let id = crate::git_identity::GitIdentity {
        name: Some("Thegn Identity Probe 7f3c".into()),
        email: Some("probe-7f3c@example.invalid".into()),
    };
    let mut s = spec(Backend::Bwrap);
    let extra = crate::git_identity::identity_env(&id, &s.env, &|_| None);
    s.env.extend(extra);
    let argv = enter_argv(&s, "true").unwrap();
    for (k, v) in [
        ("GIT_AUTHOR_NAME", "Thegn Identity Probe 7f3c"),
        ("GIT_COMMITTER_NAME", "Thegn Identity Probe 7f3c"),
        ("GIT_AUTHOR_EMAIL", "probe-7f3c@example.invalid"),
        ("GIT_COMMITTER_EMAIL", "probe-7f3c@example.invalid"),
    ] {
        assert!(
            argv.windows(3)
                .any(|w| w[0] == "--setenv" && w[1] == k && w[2] == v),
            "{k} missing from {argv:?}"
        );
    }
}

#[test]
fn cached_git_identity_reuses_a_fresh_entry() {
    let calls = std::sync::atomic::AtomicUsize::new(0);
    let read = |args: &[&str]| -> Option<String> {
        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        match args {
            ["config", "--get", "user.name"] => Some("Cache Probe".into()),
            _ => None,
        }
    };
    let root = std::path::Path::new("/thegn-test/identity-cache-repo");
    let first = cached_git_identity(root, &read);
    let second = cached_git_identity(root, &read);
    assert_eq!(first.name.as_deref(), Some("Cache Probe"));
    assert_eq!(first, second);
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
}

#[test]
fn default_spec_is_inert() {
    let spec = SandboxSpec::default();
    assert_eq!(spec.backend, Backend::None);
    assert!(spec.placement.is_local());
    assert!(spec.image.is_none());
    assert!(spec.worktree.as_os_str().is_empty());
    assert!(spec.mounts.is_empty());
    assert!(spec.env.is_empty());
    assert!(spec.env_overrides.is_empty());
    assert!(spec.env_block.is_empty());
    assert_eq!(spec.network, Network::default());
    assert!(spec.network_allow.is_empty());
    assert!(spec.network_block.is_empty());
    assert!(!spec.read_only_root);
    assert!(!spec.no_new_privileges);
    assert!(spec.pids_limit.is_none());
    assert!(spec.drop_capabilities.is_empty());
    assert!(spec.add_capabilities.is_empty(), "no capability is added");
    assert_eq!(spec.file_access, FileAccess::default());
    assert!(spec.ports.is_empty());
    assert!(spec.gpu.is_none());
    assert_eq!(spec.limits, SandboxLimits::default());
    assert!(spec.volumes.is_empty());
    assert!(spec.compose.is_none());
    assert!(spec.build.is_none());
    assert!(spec.init_script.is_none());
    assert!(!spec.devenv);
    assert!(spec.devenv_path.is_none());
    assert!(spec.name.is_empty());
    assert!(spec.vpn.is_none());
    assert!(spec.oci_host.is_none());
    assert!(spec.oci_runtime.is_none());
    assert!(!spec.daemon_persistent);
    assert!(spec.seal_home.is_none());
    assert!(spec.volume_owners.is_empty());
    assert!(spec.egress.is_none(), "no egress plan until WS04-P2-08");
}

/// Every `SandboxSpec { … }` struct-literal body in `src` (function
/// signatures, the struct definition and `impl` headers are skipped).
fn spec_literal_bodies(src: &str) -> Vec<&str> {
    let mut bodies = Vec::new();
    let mut from = 0;
    while let Some(found) = src[from..].find("SandboxSpec {") {
        let head = from + found;
        let open = head + "SandboxSpec ".len();
        from = open + 1;
        let before = src[..head]
            .trim_end_matches(|c: char| c.is_alphanumeric() || c == '_' || c == ':')
            .trim_end();
        if ["->", "struct", "for", "impl"]
            .iter()
            .any(|kw| before.ends_with(kw))
        {
            continue;
        }
        let mut depth = 0usize;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        bodies.push(&src[open + 1..open + i]);
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    bodies
}

/// WS00-P1-02: production resolution never builds a spec from
/// `SandboxSpec::default()` or struct-update syntax, so every new security
/// field forces an explicit decision at each production constructor.
#[test]
fn default_spec_not_used_in_resolution() {
    fn production(source: &'static str) -> &'static str {
        source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .map(|(production, _)| production)
            .unwrap_or(source)
    }
    // `sandbox.rs` keeps its tests in `sandbox_tests.rs`, so its whole source
    // is production code apart from small `#[cfg(test)]` helpers.
    let sandbox = include_str!("sandbox.rs");
    assert!(
        !spec_literal_bodies(sandbox).is_empty(),
        "the scan must see sandbox.rs's resolution literal"
    );
    let sources = [
        ("sandbox.rs", sandbox),
        ("host.rs", production(include_str!("host.rs"))),
        (
            "sandbox_floor.rs",
            production(include_str!("sandbox_floor.rs")),
        ),
        (
            "sandbox_preflight.rs",
            production(include_str!("sandbox_preflight.rs")),
        ),
        (
            "sandbox_tmp_hint.rs",
            production(include_str!("sandbox_tmp_hint.rs")),
        ),
        (
            "sandbox_volume_owner.rs",
            production(include_str!("sandbox_volume_owner.rs")),
        ),
    ];
    for (name, source) in sources {
        assert!(
            !source.contains("SandboxSpec::default()"),
            "{name}: production code must not build a default SandboxSpec"
        );
        for body in spec_literal_bodies(source) {
            assert!(
                !body.lines().any(|line| line.trim_start().starts_with("..")),
                "{name}: a production SandboxSpec literal uses struct-update syntax"
            );
        }
    }
}

/// THE-2695: a temp home bound read-only with a credential dir, a token file
/// and a sibling repository listed as hidden.
fn hidden_paths_fixture(backend: Backend) -> (tempfile::TempDir, PathBuf, SandboxSpec) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let home = std::fs::canonicalize(temp.path()).unwrap().join("home");
    std::fs::create_dir_all(home.join(".aws")).unwrap();
    std::fs::create_dir_all(home.join("code/other/.git")).unwrap();
    std::fs::write(home.join(".netrc"), b"machine x").unwrap();
    let mut sandbox = spec(backend);
    let home_s = home.display().to_string();
    sandbox.mounts.insert(
        0,
        Mount {
            host: home_s.clone(),
            dest: home_s,
            ro: true,
            cache: false,
        },
    );
    sandbox.hidden_paths = [".aws", ".netrc", "code/other"]
        .iter()
        .map(|rel| home.join(rel).display().to_string())
        .collect();
    (temp, home, sandbox)
}

#[test]
fn hidden_paths_bwrap_masks_credentials_and_siblings_after_the_home_bind() {
    let (_temp, home, sandbox) = hidden_paths_fixture(Backend::Bwrap);
    let argv = backend_enter_argv_with_env(&sandbox, "true", SandboxArgvEnv::default()).unwrap();
    let h = home.display().to_string();
    let bind = argv
        .windows(3)
        .position(|triple| triple == ["--ro-bind", h.as_str(), h.as_str()])
        .expect("home is bound read-only");
    let aws = format!("{h}/.aws");
    let other = format!("{h}/code/other");
    let netrc = format!("{h}/.netrc");
    let aws_mask = argv
        .windows(2)
        .position(|pair| pair == ["--tmpfs", aws.as_str()])
        .expect("~/.aws is masked with a tmpfs");
    let other_mask = argv
        .windows(2)
        .position(|pair| pair == ["--tmpfs", other.as_str()])
        .expect("the sibling repository is masked with a tmpfs");
    let netrc_mask = argv
        .windows(3)
        .position(|triple| triple == ["--ro-bind", "/dev/null", netrc.as_str()])
        .expect("~/.netrc is masked with /dev/null");
    let command = argv.iter().position(|arg| arg == "--").unwrap();
    for mask in [aws_mask, other_mask, netrc_mask] {
        assert!(bind < mask && mask < command, "{argv:?}");
    }
}

#[test]
fn hidden_paths_bwrap_keep_an_explicitly_mounted_path() {
    let (_temp, home, mut sandbox) = hidden_paths_fixture(Backend::Bwrap);
    let aws = home.join(".aws").display().to_string();
    sandbox.mounts.push(Mount {
        host: aws.clone(),
        dest: aws.clone(),
        ro: true,
        cache: false,
    });
    let argv = backend_enter_argv_with_env(&sandbox, "true", SandboxArgvEnv::default()).unwrap();
    assert!(
        !argv
            .windows(2)
            .any(|pair| pair == ["--tmpfs", aws.as_str()]),
        "{argv:?}"
    );
}

#[test]
fn hidden_paths_oci_masks_local_podman_only() {
    let (_temp, home, sandbox) = hidden_paths_fixture(Backend::Podman);
    let argv = oci_create_opts_in(&sandbox, &[], None).unwrap();
    let h = home.display().to_string();
    let aws = format!("{h}/.aws");
    let netrc = format!("/dev/null:{h}/.netrc:ro");
    assert!(
        argv.windows(2)
            .any(|pair| pair == ["--tmpfs", aws.as_str()]),
        "{argv:?}"
    );
    assert!(
        argv.windows(2).any(|pair| pair == ["-v", netrc.as_str()]),
        "{argv:?}"
    );
    let (_temp2, _home2, mut remote) = hidden_paths_fixture(Backend::Podman);
    remote.placement = Placement::Ssh(SshPlacement::plain(
        "host".into(),
        22,
        false,
        TransportKind::Ssh,
    ));
    assert!(oci_hidden_path_args(&remote).is_empty());
}

#[test]
fn hidden_paths_systemd_are_inaccessible_unless_they_hold_the_worktree() {
    let (_temp, home, mut sandbox) = hidden_paths_fixture(Backend::Systemd);
    sandbox.read_only_root = true;
    sandbox.hidden_paths.push("/wt".into());
    let argv = backend_enter_argv_with_env(&sandbox, "true", SandboxArgvEnv::default()).unwrap();
    let aws = format!("InaccessiblePaths=-{}/.aws", home.display());
    assert!(
        argv.windows(2).any(|pair| pair == ["-p", aws.as_str()]),
        "{argv:?}"
    );
    assert!(
        !argv.iter().any(|arg| arg == "InaccessiblePaths=-/wt"),
        "{argv:?}"
    );
}

/// The seven control pins of `fx` as a linked worktree, in every backend argv.
fn seven_pins(fx: &GitFixture) -> Vec<std::path::PathBuf> {
    vec![
        fx.common.join("hooks"),
        fx.common.join("config"),
        fx.common.join("info"),
        fx.admin.join("config.worktree"),
        fx.admin.join("commondir"),
        fx.admin.join("gitdir"),
        fx.wt.join(".git"),
    ]
}

fn pin_fixture_files(fx: &GitFixture) {
    std::fs::create_dir_all(fx.common.join("info")).unwrap();
    std::fs::write(fx.admin.join("config.worktree"), "").unwrap();
}

fn argv_for(backend: Backend, fx: &GitFixture, mounts: Vec<Mount>) -> Vec<String> {
    let mut s = spec(backend);
    s.worktree = fx.wt.clone();
    s.mounts = mounts;
    backend_enter_argv_with_env(&s, "true", SandboxArgvEnv::default())
        .unwrap_or_else(|e| panic!("{backend:?}: {e:?}"))
}

#[test]
fn every_git_pin_is_read_only_on_every_backend() {
    use crate::config::SandboxGitMode;
    for mode in [
        SandboxGitMode::ReadOnly,
        SandboxGitMode::CommitOnly,
        SandboxGitMode::PushLane,
        SandboxGitMode::Full,
    ] {
        let fx = git_fixture();
        pin_fixture_files(&fx);
        let (mounts, _) = git_policy_mounts(
            &git_cfg(mode),
            &fx.wt,
            &git_probe(&fx, "tg/lane", Some("main")),
        );
        let legacy = legacy_git_mounts(&fx.wt, Some(&fx.common), Some(&fx.admin), true);
        for (label, mounts) in [("policy", mounts), ("legacy", legacy)] {
            let bwrap = argv_for(Backend::Bwrap, &fx, mounts.clone());
            let oci = {
                let mut s = spec(Backend::Podman);
                s.worktree = fx.wt.clone();
                s.mounts = mounts.clone();
                oci_create_opts(&s).expect("oci opts")
            };
            let props = {
                let mut s = spec(Backend::Systemd);
                s.worktree = fx.wt.clone();
                s.mounts = mounts;
                systemd_mount_props(&s).unwrap()
            };
            for pin in seven_pins(&fx) {
                // `read-only` has no per-worktree admin dir (the plan treats the
                // common dir as its own admin dir).
                if mode == SandboxGitMode::ReadOnly
                    && label == "policy"
                    && pin.starts_with(&fx.admin)
                {
                    continue;
                }
                let p = pin.display().to_string();
                assert!(
                    triple_at(&bwrap, "--ro-bind", &pin).is_some(),
                    "{mode:?}/{label}: bwrap {p}"
                );
                assert!(
                    oci.iter().any(|a| a == &format!("{p}:{p}:ro")),
                    "{mode:?}/{label}: oci {p}: {oci:?}"
                );
                assert!(
                    props.contains(&format!("ReadOnlyPaths={p}")),
                    "{mode:?}/{label}: systemd {p}: {props:?}"
                );
            }
        }
    }
}

#[test]
fn systemd_common_outside_home_is_readonly() {
    use crate::config::SandboxGitMode;
    let fx = git_fixture();
    pin_fixture_files(&fx);
    let (mounts, _) = git_policy_mounts(
        &git_cfg(SandboxGitMode::CommitOnly),
        &fx.wt,
        &git_probe(&fx, "tg/lane", Some("main")),
    );
    let mut s = spec(Backend::Systemd);
    s.worktree = fx.wt.clone();
    s.mounts = mounts;
    s.read_only_root = false;
    let props = systemd_mount_props(&s).unwrap();
    let common = fx.common.display().to_string();
    assert!(
        props.contains(&format!("ReadOnlyPaths={common}")),
        "{props:?}"
    );
    for rw in [
        "objects",
        "worktrees/lane",
        "refs/heads/tg",
        "logs/refs/heads/tg",
    ] {
        assert!(
            props.contains(&format!("ReadWritePaths={common}/{rw}")),
            "{rw}: {props:?}"
        );
    }
}

#[test]
fn host_exec_files_are_pinned_after_the_worktree_for_standard_not_open() {
    let fx = git_fixture();
    std::fs::create_dir_all(fx.wt.join(".claude")).unwrap();
    std::fs::write(fx.wt.join(".envrc"), "").unwrap();
    std::fs::write(fx.wt.join(".mcp.json"), "{}").unwrap();
    assert!(host_exec_mounts(&fx.wt, SandboxProfile::Open).is_empty());
    let pins = host_exec_mounts(&fx.wt, SandboxProfile::Hardened);
    assert!(pins.iter().all(|m| m.ro));
    let mut mounts = vec![path_mount(&fx.wt, false)];
    mounts.extend(pins);
    let argv = argv_for(Backend::Bwrap, &fx, mounts.clone());
    let wt = triple_at(&argv, "--bind", &fx.wt).expect("worktree");
    for name in [".claude", ".envrc", ".mcp.json"] {
        let at =
            triple_at(&argv, "--ro-bind", &fx.wt.join(name)).unwrap_or_else(|| panic!("{name}"));
        assert!(wt < at, "{name} after worktree: {argv:?}");
    }
    assert!(
        triple_at(&argv, "--ro-bind", &fx.wt.join(".vscode/tasks.json")).is_none(),
        "missing entries are not pinned"
    );
    let mut s = spec(Backend::Systemd);
    s.worktree = fx.wt.clone();
    s.mounts = mounts;
    let props = systemd_mount_props(&s).unwrap();
    let claude = fx.wt.join(".claude").display().to_string();
    assert!(
        props.contains(&format!("ReadOnlyPaths={claude}")),
        "{props:?}"
    );
}

#[test]
fn legacy_remote_placement_pins_without_stat() {
    let common = std::path::Path::new("/remote/repo/.git");
    let admin = common.join("worktrees/lane");
    let wt = std::path::Path::new("/remote/wt");
    let mounts = legacy_git_mounts(wt, Some(common), Some(&admin), false);
    for pin in ["hooks", "config", "info"] {
        let dest = common.join(pin).display().to_string();
        assert!(mounts.iter().any(|m| m.ro && m.dest == dest), "{pin}");
    }
    assert!(mounts.iter().any(|m| m.ro && m.dest == "/remote/wt/.git"));
}

#[cfg(unix)]
#[test]
fn legacy_symlinked_hooks_fail_closed_to_a_read_only_common_dir() {
    let fx = git_fixture();
    pin_fixture_files(&fx);
    std::fs::remove_dir_all(fx.common.join("hooks")).unwrap();
    std::os::unix::fs::symlink(&fx.wt, fx.common.join("hooks")).unwrap();
    let mounts = legacy_git_mounts(&fx.wt, Some(&fx.common), Some(&fx.admin), true);
    let common = fx.common.display().to_string();
    assert!(
        mounts.iter().any(|m| m.dest == common && m.ro),
        "{mounts:?}"
    );
}
