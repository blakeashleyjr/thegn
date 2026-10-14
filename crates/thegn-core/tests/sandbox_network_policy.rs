//! Opt-in Podman checks for hostname policy reporting and universal deny lowering.

use std::path::PathBuf;

use thegn_core::config::{
    FileAccess, IsolationFloor, Network, OnFloorMiss, SandboxBackend, SandboxConfig, SandboxProfile,
};
use thegn_core::placement::Placement;
use thegn_core::remote::GitLoc;
use thegn_core::sandbox::{
    Backend, HOSTNAME_RULES, HostnameRulesPolicy, SandboxLimits, SandboxSpec, ensure, enter_argv,
    hostname_rules_note, resolve_placed_exact,
};

fn skip() -> bool {
    !thegn_core::util::have("podman")
        || std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
}

#[expect(
    clippy::disallowed_methods,
    reason = "opt-in Podman e2e removes its test container"
)]
fn force_rm(name: &str) {
    // best-effort: test scratch: best-effort by design
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", name])
        .output();
}

fn spec_with_network_block(name: &str, block: Vec<String>) -> SandboxSpec {
    SandboxSpec {
        backend: Backend::Podman,
        placement: Placement::Local,
        image: Some("docker.io/library/alpine:latest".into()),
        worktree: PathBuf::from("/tmp/tg-e2e-net"),
        mounts: vec![],
        env: vec![],
        env_overrides: std::collections::HashMap::new(),
        env_block: vec![],
        network: Network::Nat,
        network_allow: vec![],
        network_block: block,
        read_only_root: false,
        no_new_privileges: false,
        pids_limit: None,
        drop_capabilities: vec![],
        add_capabilities: vec![],
        file_access: FileAccess::None,
        ports: vec![],
        gpu: None,
        limits: SandboxLimits::default(),
        volumes: vec![],
        compose: None,
        build: None,
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: name.into(),
        vpn: None,
        oci_host: None,
        oci_runtime: None,
        daemon_persistent: false,
        seal_home: None,
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "opt-in Podman e2e executes a command in its test container"
)]
fn run_in(spec: &SandboxSpec, cmd: &str) -> String {
    let argv = enter_argv(spec, cmd).expect("valid volume names");
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .unwrap_or_else(|_| panic!("exec failed: {argv:?}"));
    String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
}

// G1: configured hostname rules remain labelled; the unit test
// `no_dns_flag_in_oci_create_opts` covers the OCI create options directly.
#[test]
fn g1_hostname_rules_do_not_inject_dns_flag() {
    if skip() {
        return;
    }
    let name = "thegn-e2e-g1";
    force_rm(name);
    let mut spec = spec_with_network_block(name, vec!["blocked.internal".into()]);
    spec.network_allow = vec!["allowed.example".into()];
    assert_eq!(HOSTNAME_RULES, HostnameRulesPolicy::WarnDnsOnly);
    assert!(
        hostname_rules_note(&spec)
            .unwrap()
            .contains("DNS-name rules only")
    );
    ensure(&spec).expect("ensure failed");
    let out = run_in(&spec, "true");
    force_rm(name);
    assert!(out.is_empty(), "true should produce no output: {out}");
}

// G3: allow lists also remain warnings; the unit test
// `no_dns_flag_in_oci_create_opts` covers the OCI create options directly.
#[test]
fn g3_allow_list_does_not_inject_dns_flag() {
    if skip() {
        return;
    }
    let name = "thegn-e2e-g3";
    force_rm(name);
    let mut spec = spec_with_network_block(name, vec![]);
    spec.network_allow = vec!["api.example.com".into()];
    assert!(hostname_rules_note(&spec).is_some());
    ensure(&spec).expect("ensure failed");
    let out = run_in(&spec, "true");
    force_rm(name);
    assert!(out.is_empty(), "true should produce no output: {out}");
}

// G2: a resolved universal deny is the runtime's offline network boundary.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "opt-in Podman e2e checks its offline test container"
)]
fn g2_universal_block_runs_offline() {
    if skip() {
        return;
    }
    let name = "thegn-e2e-g2";
    force_rm(name);
    std::fs::create_dir_all("/tmp/tg-e2e-net").expect("create Podman worktree bind source");
    let cfg = SandboxConfig {
        enabled: true,
        backend: SandboxBackend::Podman,
        image: "docker.io/library/alpine:latest".into(),
        network: Network::Nat,
        network_block: vec!["*".into()],
        isolation_floor: IsolationFloor::Off,
        on_floor_miss: OnFloorMiss::Degrade,
        ..Default::default()
    };
    let loc = GitLoc::from_db("/tmp/tg-e2e-net", None);
    let spec = resolve_placed_exact(&cfg, &loc, name, SandboxProfile::Hardened, Placement::Local)
        .expect("Podman should resolve for the opt-in e2e");
    assert_eq!(spec.network, Network::None);
    ensure(&spec).expect("offline container ensure failed");
    let argv = enter_argv(&spec, "ip route").expect("valid volume names");
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .expect("ip route exec failed");
    force_rm(name);
    assert!(out.status.success(), "ip route failed: {out:?}");
    assert!(
        out.stdout.is_empty(),
        "offline container has a route: {out:?}"
    );
}

// G4: empty hostname rules still launch; the unit test
// `no_dns_flag_in_oci_create_opts` covers the OCI create options directly.
#[test]
fn g4_empty_rules_launch_without_dns_override() {
    if skip() {
        return;
    }
    let name = "thegn-e2e-g4";
    force_rm(name);
    let spec = spec_with_network_block(name, vec![]);
    ensure(&spec).expect("ensure failed");
    let out = run_in(&spec, "true");
    force_rm(name);
    assert!(out.is_empty(), "true should produce no output: {out}");
}
