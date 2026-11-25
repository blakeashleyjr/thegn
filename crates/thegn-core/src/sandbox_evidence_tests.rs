//! THE-792 runtime-evidence regressions: canaries for the sandbox admission
//! contract — env selection, the isolation floor, and container-reuse policy.
//! The evidence audit behind them is a private dated record; the public
//! contract these tests keep is documented in
//! `docs/help/sandbox-security-model.md`.

use super::*;
use crate::capabilities::IsolationClass;
use crate::config::{Config, IsolationFloor, Network, OnFloorMiss, SandboxConfig, SandboxProfile};
use crate::remote::GitLoc;
use crate::sandbox_floor::{FloorDecision, decide, floor_in_force, miss_policy_in_force};

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

    let resolved = config.resolve_env(
        repo.path(),
        &loc,
        repo.path(),
        &crate::sandbox_pin::EnvSelection::none(),
    );
    assert_eq!(resolved.name, "default");
    assert_eq!(resolved.sandbox.profile, SandboxProfile::Sealed);
    assert_eq!(resolved.sandbox.network, Network::None);
    assert_eq!(
        resolved.sandbox.isolation_floor,
        Some(IsolationFloor::SharedKernel)
    );
    assert_eq!(resolved.sandbox.on_floor_miss, OnFloorMiss::Fail);

    let explicitly_selected = config.resolve_env(
        repo.path(),
        &loc,
        repo.path(),
        &crate::sandbox_pin::EnvSelection::named("hostdev"),
    );
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
