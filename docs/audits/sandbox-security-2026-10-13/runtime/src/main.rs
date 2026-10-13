//! Audit-only executable: calls the checked-out core without changing its API.
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use thegn_core::capabilities::IsolationClass;
use thegn_core::config::{Config, IsolationFloor, Network, OnFloorMiss, SandboxConfig};
use thegn_core::dns_filter::{DnsPolicy, drain_events, get_or_start};
use thegn_core::remote::GitLoc;
use thegn_core::sandbox::{ensure, enter_argv, resolve};

fn query(addr: SocketAddr, name: &str) -> u8 {
    let mut packet = vec![0xAB, 0x12, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        packet.push(label.len() as u8);
        packet.extend(label.as_bytes());
    }
    packet.extend([0, 0, 1, 0, 1]);
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    sock.send_to(&packet, addr).unwrap();
    let mut response = [0; 512];
    let n = sock.recv(&mut response).unwrap();
    assert!(n >= 12);
    response[3] & 15
}

fn dns() {
    let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut buf = [0; 512];
        while let Ok((n, peer)) = upstream.recv_from(&mut buf) {
            let mut resp = buf[..n].to_vec();
            resp[2] = 0x81;
            resp[3] = 0x80;
            resp[7] = 1;
            resp.extend([0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 127, 0, 0, 1]);
            upstream.send_to(&resp, peer).unwrap();
        }
    });
    let p1 = get_or_start(DnsPolicy {
        allow: vec!["allowed.test".into()],
        block: vec![],
        upstream: Some(upstream_addr),
    })
    .unwrap();
    let filter = SocketAddr::from(([127, 0, 0, 1], p1));
    println!("first_allowed_rcode={}", query(filter, "allowed.test"));
    println!("first_blocked_rcode={}", query(filter, "blocked.test"));
    let p2 = get_or_start(DnsPolicy {
        allow: vec![],
        block: vec!["*".into()],
        upstream: Some(upstream_addr),
    })
    .unwrap();
    println!("second_policy_same_port={}", p1 == p2);
    println!(
        "second_deny_all_allowed_test_rcode={}",
        query(filter, "allowed.test")
    );
    println!(
        "alternate_dns_blocked_test_rcode={}",
        query(upstream_addr, "blocked.test")
    );
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    println!(
        "direct_ip_tcp_connect={}",
        TcpStream::connect_timeout(&tcp.local_addr().unwrap(), Duration::from_secs(1)).is_ok()
    );
    for e in drain_events() {
        println!("event={} allowed={}", e.name, e.allowed);
    }
}

fn env_selection(root: &Path) {
    let cfg: Config = toml::from_str(
        r#"
[sandbox]
profile="sealed"
network="none"
isolation_floor="shared-kernel"
on_floor_miss="fail"
[env.hostdev.sandbox]
backend="none"
profile="open"
network="host"
isolation_floor="off"
on_floor_miss="degrade"
"#,
    )
    .unwrap();
    let before = cfg.resolve_env(root, &GitLoc::Local(root.to_path_buf()), root, None);
    println!(
        "before={} backend={} profile={} network={} floor={} miss={}",
        before.name,
        before.sandbox.backend,
        before.sandbox.profile,
        before.sandbox.network,
        before.sandbox.isolation_floor,
        before.sandbox.on_floor_miss
    );
    std::fs::write(
        root.join(".thegn.toml"),
        "env=\"hostdev\"\n[sandbox]\nprofile=\"sealed\"\nnetwork=\"none\"\n",
    )
    .unwrap();
    let after = cfg.resolve_env(root, &GitLoc::Local(root.to_path_buf()), root, None);
    println!(
        "after={} backend={} profile={} network={} floor={} miss={}",
        after.name,
        after.sandbox.backend,
        after.sandbox.profile,
        after.sandbox.network,
        after.sandbox.isolation_floor,
        after.sandbox.on_floor_miss
    );
}

fn policy_overlay(root: &Path) {
    let restricted: Config = toml::from_str(
        r#"[sandbox]
network_allow=["allowed.test"]
network_block=["globalblocked.test"]
"#,
    )
    .unwrap();
    for (name, cfg, overlay) in [
        (
            "restricted_repo_adds_destination",
            restricted.clone(),
            "[sandbox]\nnetwork_allow=[\"broader.test\"]\n",
        ),
        (
            "restricted_repo_empty_allow",
            restricted.clone(),
            "[sandbox]\nnetwork_allow=[]\n",
        ),
        (
            "restricted_repo_adds_block",
            restricted,
            "[sandbox]\nnetwork_block=[\"repoblocked.test\"]\n",
        ),
        (
            "unrestricted_repo_narrows",
            Config::default(),
            "[sandbox]\nnetwork_allow=[\"allowed.test\"]\n",
        ),
    ] {
        std::fs::write(root.join(".thegn.toml"), overlay).unwrap();
        let env = cfg.resolve_env(root, &GitLoc::Local(root.to_path_buf()), root, None);
        println!(
            "case={name} allow={:?} block={:?}",
            env.sandbox.network_allow, env.sandbox.network_block
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "dns" => {
            dns();
            return;
        }
        "policy-overlay" => {
            policy_overlay(Path::new(&args[2]));
            return;
        }
        "env-selection" => {
            env_selection(Path::new(&args[2]));
            return;
        }
        "floor" => {
            for (floor, miss) in [
                (IsolationFloor::Off, OnFloorMiss::Degrade),
                (IsolationFloor::SharedKernel, OnFloorMiss::Degrade),
                (IsolationFloor::SharedKernel, OnFloorMiss::Fail),
            ] {
                println!(
                    "floor={} miss={} result={:?}",
                    floor,
                    miss,
                    thegn_core::sandbox_floor::decide(
                        floor,
                        miss,
                        IsolationClass::HostProcess,
                        IsolationClass::HostProcess
                    )
                );
            }
            return;
        }
        _ => {}
    }
    let cfg_text = format!(
        "backend={:?}\nprofile={:?}\nnetwork={:?}\n",
        args[3], args[4], args[5]
    );
    let mut cfg: SandboxConfig = toml::from_str(&cfg_text).unwrap();
    if let Ok(image) = std::env::var("AUDIT_IMAGE") {
        cfg.image = image;
    }
    if let Ok(host) = std::env::var("AUDIT_OCI_HOST") {
        cfg.oci_host = host;
    }
    cfg.inject_devshell = false;
    cfg.env_passthrough.push("HOME".into());
    if std::env::var_os("AUDIT_CORRECTED_MOUNTS").is_some() {
        cfg.mounts = vec!["~/.gitconfig:ro".into(), "~/.gnupg".into()];
    }
    if std::env::var_os("AUDIT_OCI_MINIMAL").is_some()
        && matches!(args[3].as_str(), "podman" | "docker")
    {
        cfg.auto_caches = false;
        let home = std::env::var("HOME").unwrap();
        cfg.mounts = vec!["/nix/store:ro".into(), format!("{home}/.cache/pip")];
        if !cfg.profile.hides_home() {
            cfg.mounts.push(format!("{home}:ro"));
            cfg.mounts.push(format!("{home}/.gnupg"));
        }
    }
    if std::env::var_os("AUDIT_EMPTY_CHAIN").is_some() {
        cfg.backend_chain.clear();
    }
    if let Ok(custom) = std::env::var("AUDIT_CUSTOM_MOUNT") {
        cfg.mounts.push(custom);
    }
    if args.get(6).is_some_and(|x| x == "deny-all") {
        cfg.network_block = vec!["*".into()];
    }
    println!(
        "request backend={} profile={} network={} floor={} miss={}",
        cfg.backend, cfg.profile, cfg.network, cfg.isolation_floor, cfg.on_floor_miss
    );
    let loc = GitLoc::Local(Path::new(&args[2]).to_path_buf());
    let Some(mut spec) = resolve(&cfg, &loc, "audit") else {
        println!("resolved=None (local host fallback or disabled)");
        return;
    };
    spec.name = std::env::var("AUDIT_CONTAINER_NAME").unwrap();
    println!(
        "resolved backend={} profile_home={:?} network={:?} name={}",
        spec.backend.label(),
        spec.seal_home,
        spec.network,
        spec.name
    );
    println!("mounts={}", serde_json::to_string(&spec.mounts.iter().map(|m| serde_json::json!({"host": m.host, "dest": m.dest, "ro": m.ro, "cache": m.cache})).collect::<Vec<_>>()).unwrap());
    println!(
        "forwarded_env_keys={:?}",
        spec.env.iter().map(|(k, _)| k).collect::<Vec<_>>()
    );
    let script = std::env::var("AUDIT_INNER").unwrap();
    let argv = match enter_argv(&spec, &script) {
        Ok(argv) => argv,
        Err(e) => {
            println!("enter_refused={e}");
            return;
        }
    };
    println!("argv={}", serde_json::to_string(&argv).unwrap());
    if args[1] == "plan" {
        return;
    }
    if spec.backend.is_oci() {
        // Refuse to even call ensure if this exact local image is absent. Never pull.
        let mut exists = Command::new("podman");
        if let Some(host) = &spec.oci_host {
            exists.args(["--url", host]);
        }
        assert!(
            exists
                .args(["image", "exists", spec.image.as_ref().unwrap()])
                .status()
                .unwrap()
                .success(),
            "audit requires a cached image"
        );
    }
    match ensure(&spec) {
        Ok(()) => println!("ensure=ok"),
        Err(e) => {
            println!("ensure=error {e}");
            return;
        }
    }
    let status = Command::new(&argv[0]).args(&argv[1..]).status().unwrap();
    println!("enter_exit={status}");
    if args[1] == "reuse" {
        spec.network = Network::None;
        println!("reuse_requested_network=None");
        match ensure(&spec) {
            Ok(()) => println!("reuse_ensure=ok"),
            Err(e) => {
                println!("reuse_ensure=error {e}");
                return;
            }
        }
        let after = enter_argv(&spec, &script).unwrap();
        println!("reuse_argv={}", serde_json::to_string(&after).unwrap());
        let status = Command::new(&after[0]).args(&after[1..]).status().unwrap();
        println!("reuse_enter_exit={status}");
        let mut inspect = Command::new("podman");
        if let Some(host) = &spec.oci_host {
            inspect.args(["--url", host]);
        }
        let out = inspect
            .args([
                "inspect",
                "--format",
                "{{.HostConfig.NetworkMode}} {{.Id}}",
                &spec.name,
            ])
            .output()
            .unwrap();
        println!(
            "reused_container_inspect={}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
