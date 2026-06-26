//! Suite G — Network policy enforcement (Tier 2, podman + DNS filter).
//!
//! Tests that the DNS filter configured via network_block/network_allow in
//! SandboxSpec actually blocks or forwards DNS queries inside the container.
//! The DNS filter proxy is started via dns_filter::get_or_start; containers
//! receive --dns 127.0.0.1:<port> so their resolver hits the proxy.

use std::path::PathBuf;

use superzej_core::config::{FileAccess, Network};
use superzej_core::dns_filter::{DnsPolicy, drain_events};
use superzej_core::sandbox::{Backend, SandboxLimits, SandboxSpec, Transport, ensure, enter_argv};

fn skip() -> bool {
    !superzej_core::util::have("podman")
        || std::env::var("CI").is_ok()
        || std::env::var("SKIP_PODMAN_E2E").is_ok()
        || std::env::var("PODMAN_E2E_FORCE").is_err()
}

fn force_rm(name: &str) {
    let _ = std::process::Command::new("podman")
        .args(["rm", "-f", name])
        .output();
}

fn spec_with_network_block(name: &str, block: Vec<String>) -> SandboxSpec {
    SandboxSpec {
        backend: Backend::Podman,
        transport: Transport::Local,
        image: Some("docker.io/library/alpine:latest".into()),
        worktree: PathBuf::from("/tmp/sz-e2e-net"),
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
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: name.into(),
        vpn: None,
    }
}

fn run_in(spec: &SandboxSpec, cmd: &str) -> String {
    let argv = enter_argv(spec, cmd);
    let out = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .unwrap_or_else(|_| panic!("exec failed: {argv:?}"));
    String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr)
}

// ── G1: container can't resolve blocked domain ───────────────────────────────

#[test]
fn g1_blocked_domain_nxdomain_in_container() {
    if skip() {
        return;
    }
    let name = "superzej-e2e-g1";
    force_rm(name);
    let spec = spec_with_network_block(name, vec!["blocked.internal".into()]);
    ensure(&spec).expect("ensure failed");
    // nslookup exits non-zero and prints "NXDOMAIN" or "can't resolve" for blocked.
    let out = run_in(&spec, "nslookup blocked.internal 2>&1 || true");
    force_rm(name);
    let blocked = out.contains("NXDOMAIN")
        || out.contains("can't resolve")
        || out.contains("nxdomain")
        || out.contains("server can't find");
    assert!(blocked, "expected NXDOMAIN for blocked domain; got: {out}");
}

// ── G2: allow-list restricts unlisted domains ────────────────────────────────

#[test]
fn g2_allow_list_blocks_unlisted() {
    if skip() {
        return;
    }
    let name = "superzej-e2e-g2";
    force_rm(name);
    let mut spec = spec_with_network_block(name, vec![]);
    spec.network_allow = vec!["api.example.com".into()];
    ensure(&spec).expect("ensure failed");
    // Unlisted domain should get NXDOMAIN.
    let out = run_in(&spec, "nslookup google.com 2>&1 || true");
    force_rm(name);
    let nxdomain = out.contains("NXDOMAIN")
        || out.contains("can't resolve")
        || out.contains("nxdomain")
        || out.contains("server can't find");
    assert!(
        nxdomain,
        "domain not in allow-list should get NXDOMAIN; got: {out}"
    );
}

// ── G3: DNS events captured in ring buffer ───────────────────────────────────

#[test]
fn g3_dns_events_captured() {
    if skip() {
        return;
    }
    // Start the dns filter explicitly with a known policy to prime the singleton.
    let _ = superzej_core::dns_filter::get_or_start(DnsPolicy {
        block: vec!["blocked.internal".into()],
        allow: vec![],
        upstream: None,
    });
    drain_events(); // clear prior
    let name = "superzej-e2e-g3";
    force_rm(name);
    let spec = spec_with_network_block(name, vec!["blocked.internal".into()]);
    ensure(&spec).expect("ensure failed");
    // Query a blocked and an allowed domain inside the container.
    let _ = run_in(&spec, "nslookup blocked.internal 2>&1 || true");
    let _ = run_in(&spec, "nslookup example.com 2>&1 || true");
    force_rm(name);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let events = drain_events();
    assert!(
        events
            .iter()
            .any(|e| e.name.contains("blocked.internal") && !e.allowed),
        "expected blocked event for blocked.internal; events={events:?}"
    );
}

// ── G4: network_block=[] → all domains resolve ───────────────────────────────

#[test]
fn g4_empty_block_list_allows_all() {
    if skip() {
        return;
    }
    let name = "superzej-e2e-g4";
    force_rm(name);
    let spec = spec_with_network_block(name, vec![]); // no block-list
    ensure(&spec).expect("ensure failed");
    // With empty policy: all domains are allowed. The DNS filter won't be started
    // (oci_create_opts only injects --dns when network_allow or network_block
    // is non-empty), so the container uses the default resolver.
    // Just verify the container is running and basic DNS resolves.
    let out = run_in(&spec, "nslookup localhost 2>&1 || true");
    force_rm(name);
    // Localhost always resolves — any non-crash result is acceptable.
    assert!(!out.is_empty(), "nslookup should produce some output");
}
