//! THE-792 runtime-evidence regressions; see
//! `docs/audits/sandbox-security-2026-10-13/runtime-evidence.md`.

use super::*;
use crate::capabilities::IsolationClass;
use crate::config::{Config, IsolationFloor, Network, OnFloorMiss, SandboxConfig, SandboxProfile};
use crate::remote::GitLoc;
use crate::sandbox_floor::{FloorDecision, decide, floor_in_force, miss_policy_in_force};
use std::net::UdpSocket;
use std::time::Duration;

#[test]
fn canary_env_selection_repo_hint_cannot_select_a_weaker_named_env() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join(".thegn.toml"), "env = \"hostdev\"\n").unwrap();
    let config: Config = toml::from_str(
        r#"
            [sandbox]
            enabled = true
            backend = "auto"
            backend_chain = ["podman"]
            profile = "sealed"
            network = "none"
            isolation_floor = "shared-kernel"
            on_floor_miss = "fail"
            [env.hostdev]
            placement = "local"
            [env.hostdev.sandbox]
            backend = "none"
            profile = "open"
            network = "host"
            isolation_floor = "off"
            on_floor_miss = "degrade"
        "#,
    )
    .unwrap();
    let loc = GitLoc::Local(repo.path().to_path_buf());

    let resolved = config.resolve_env(repo.path(), &loc, repo.path(), None);
    assert_eq!(resolved.name, "default");
    assert_eq!(resolved.sandbox.profile, SandboxProfile::Sealed);
    assert_eq!(resolved.sandbox.network, Network::None);
    assert_eq!(
        resolved.sandbox.isolation_floor,
        Some(IsolationFloor::SharedKernel)
    );
    assert_eq!(resolved.sandbox.on_floor_miss, OnFloorMiss::Fail);

    let explicitly_selected = config.resolve_env(repo.path(), &loc, repo.path(), Some("hostdev"));
    assert_eq!(explicitly_selected.name, "hostdev");
    assert_eq!(explicitly_selected.sandbox.profile, SandboxProfile::Open);
    assert_eq!(explicitly_selected.sandbox.network, Network::Host);
    assert_eq!(
        explicitly_selected.sandbox.isolation_floor,
        Some(IsolationFloor::Off)
    );
    assert_eq!(
        explicitly_selected.sandbox.on_floor_miss,
        OnFloorMiss::Degrade
    );
}

#[test]
fn canary_terminal_fallback_default_floor_refuses_host_process() {
    let default = SandboxConfig {
        enabled: true,
        ..SandboxConfig::default()
    };
    assert_eq!(
        floor_in_force(&default, false),
        IsolationFloor::SharedKernel
    );
    assert_eq!(miss_policy_in_force(&default), OnFloorMiss::Fail);
    let refusal = match decide(
        floor_in_force(&default, false),
        miss_policy_in_force(&default),
        IsolationClass::HostProcess,
        IsolationClass::HostProcess,
    ) {
        FloorDecision::Fail(miss) => miss.refusal(),
        other => panic!("default floor must refuse a host process, got {other:?}"),
    };
    assert!(refusal.contains("isolation_floor = \"off\""));

    let explicit_degrade = SandboxConfig {
        isolation_floor: Some(IsolationFloor::SharedKernel),
        on_floor_miss: OnFloorMiss::Degrade,
        ..SandboxConfig::default()
    };
    assert!(matches!(
        decide(
            floor_in_force(&explicit_degrade, false),
            miss_policy_in_force(&explicit_degrade),
            IsolationClass::HostProcess,
            IsolationClass::HostProcess,
        ),
        FloorDecision::Degrade(_)
    ));

    let explicit_off = SandboxConfig {
        isolation_floor: Some(IsolationFloor::Off),
        ..SandboxConfig::default()
    };
    assert!(matches!(
        decide(
            floor_in_force(&explicit_off, false),
            miss_policy_in_force(&explicit_off),
            IsolationClass::HostProcess,
            IsolationClass::HostProcess,
        ),
        FloorDecision::Ok
    ));
}

#[test]
fn canary_reuse_narrowing_network_change_forces_recreate() {
    let nat = spec(Backend::Podman);
    let mut none = nat.clone();
    none.network = Network::None;
    let nat_policy = expected_policy(&nat);
    let none_policy = expected_policy(&none);
    assert_ne!(nat_policy, none_policy);

    let required: std::collections::HashSet<&str> =
        ["/wt/feat", "/repo/.git"].into_iter().collect();
    let probe = format!("RUNNING\n0\n{nat_policy}\n/wt/feat\n/repo/.git\n");
    assert_eq!(
        judge_inspect(&probe, &required, false, false, &none_policy),
        (true, false),
        "a running NAT container cannot satisfy network=none"
    );
    assert_eq!(
        judge_inspect(&probe, &required, false, false, &nat_policy),
        (true, true),
        "the same container remains reusable for its original policy"
    );
}

#[test]
fn canary_dns_singleton_second_policy_gets_its_own_filter() {
    let upstream = UdpSocket::bind("127.0.0.1:0").unwrap();
    upstream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let upstream_addr = upstream.local_addr().unwrap();

    let first_port = crate::dns_filter::get_or_start(crate::dns_filter::DnsPolicy {
        allow: vec!["allowed.test".into()],
        block: Vec::new(),
        upstream: Some(upstream_addr),
    })
    .expect("first DNS policy starts");
    let second_port = crate::dns_filter::get_or_start(crate::dns_filter::DnsPolicy {
        allow: Vec::new(),
        block: vec!["*".into()],
        upstream: Some(upstream_addr),
    })
    .expect("second DNS policy starts");
    assert_ne!(first_port, second_port);

    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut query = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in "allowed.test".split('.') {
        query.push(u8::try_from(label.len()).unwrap());
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);
    query.extend_from_slice(&[0, 1, 0, 1]);

    client.send_to(&query, ("127.0.0.1", first_port)).unwrap();
    let mut forwarded = [0_u8; 512];
    let (forwarded_len, filter_addr) = upstream.recv_from(&mut forwarded).unwrap();
    forwarded[2] = 0x81;
    forwarded[3] = 0x80;
    upstream
        .send_to(&forwarded[..forwarded_len], filter_addr)
        .unwrap();
    let mut response = [0_u8; 512];
    let (allowed_len, _) = client.recv_from(&mut response).unwrap();
    assert!(allowed_len >= 4);
    assert_eq!(&response[..2], &[0x12, 0x34]);
    assert_eq!(response[3] & 0x0f, 0, "the first policy allows the name");

    query[..2].copy_from_slice(&[0x56, 0x78]);
    client.send_to(&query, ("127.0.0.1", second_port)).unwrap();
    let (blocked_len, _) = client.recv_from(&mut response).unwrap();
    assert!(blocked_len >= 4);
    assert_eq!(&response[..2], &[0x56, 0x78]);
    assert_eq!(response[3] & 0x0f, 3, "the second policy blocks the name");

    assert!(
        matches!(
            upstream.recv_from(&mut forwarded),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                )
        ),
        "a blocked query must not reach the configured upstream"
    );
}
