use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thegn_core::config::{VpnMode, VpnOnError, VpnProviderKind};
use thegn_svc::vpn::{Attachment, DnsConfig, OciRuntime, Requirements, SidecarPlan, VpnProvider};

#[derive(Clone, Default)]
struct Calls {
    ups: Arc<Mutex<Vec<String>>>,
    readies: Arc<Mutex<Vec<String>>>,
    downs: Arc<Mutex<Vec<String>>>,
    removals: Arc<Mutex<Vec<String>>>,
}

struct FakeProvider {
    calls: Calls,
    ready_fails: bool,
    up_fails: bool,
}

impl VpnProvider for FakeProvider {
    fn kind(&self) -> &'static str {
        "test"
    }

    fn requirements(&self) -> Requirements {
        Requirements::default()
    }

    fn dns(&self) -> DnsConfig {
        DnsConfig::default()
    }

    fn sidecar_plan(&self, _container: &str) -> anyhow::Result<SidecarPlan> {
        anyhow::bail!("not used by the fake provider")
    }

    fn up(&self, _rt: &OciRuntime, container: &str) -> anyhow::Result<Attachment> {
        self.calls.ups.lock().unwrap().push(container.to_string());
        if self.up_fails {
            anyhow::bail!("partial bring-up failure")
        }
        Ok(Attachment::default())
    }

    fn ready(&self, _rt: &OciRuntime, container: &str, _timeout: Duration) -> anyhow::Result<()> {
        self.calls
            .readies
            .lock()
            .unwrap()
            .push(container.to_string());
        if self.ready_fails {
            anyhow::bail!("readiness timeout")
        }
        Ok(())
    }

    fn down(&self, _rt: &OciRuntime, container: &str) -> anyhow::Result<()> {
        self.calls.downs.lock().unwrap().push(container.to_string());
        Ok(())
    }
}

fn vpn_spec(on_error: VpnOnError) -> thegn_core::sandbox::SandboxSpec {
    let vpn_config = thegn_core::config::VpnConfig {
        provider: VpnProviderKind::Tailscale,
        mode: VpnMode::Sidecar,
        on_error,
        ..Default::default()
    };
    let vpn = thegn_core::sandbox::build_vpn_spec(
        &vpn_config,
        "tg-test-wt",
        thegn_core::config::SandboxProfile::Hardened,
    )
    .expect("enabled VPN config resolves");
    thegn_core::sandbox::SandboxSpec {
        backend: thegn_core::sandbox::Backend::Podman,
        placement: thegn_core::placement::Placement::Local,
        image: Some("test:latest".into()),
        worktree: std::path::PathBuf::from("/repo/wt"),
        mounts: Vec::new(),
        env: Vec::new(),
        env_overrides: Default::default(),
        env_block: Vec::new(),
        network: thegn_core::config::Network::Nat,
        network_allow: Vec::new(),
        network_block: Vec::new(),
        read_only_root: false,
        no_new_privileges: false,
        pids_limit: None,
        drop_capabilities: Vec::new(),
        add_capabilities: Vec::new(),
        file_access: thegn_core::config::FileAccess::Worktree,
        ports: Vec::new(),
        gpu: None,
        limits: thegn_core::sandbox::SandboxLimits::default(),
        volumes: Vec::new(),
        compose: None,
        build: None,
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: "tg-test-wt".into(),
        vpn: Some(vpn),
        oci_host: None,
        oci_runtime: None,
        daemon_persistent: false,
        seal_home: None,
    }
}

fn fake(ready_fails: bool) -> FakeProvider {
    FakeProvider {
        calls: Calls::default(),
        ready_fails,
        up_fails: false,
    }
}

#[test]
fn in_container_warn_keeps_vpn_capability_request() {
    let mut spec = vpn_spec(VpnOnError::Warn);
    spec.vpn.as_mut().unwrap().mode = VpnMode::InContainer;

    attach_vpn(&mut spec).unwrap();

    assert!(spec.vpn.is_some());
}

#[test]
fn in_container_launch_failure_does_not_roll_back_sidecar() {
    let mut spec = vpn_spec(VpnOnError::Warn);
    spec.vpn.as_mut().unwrap().mode = VpnMode::InContainer;
    let provider = fake(false);
    let calls = provider.calls.clone();
    let rt = OciRuntime::new(vec!["fake-runtime".into()]);

    let result: anyhow::Result<()> = run_with_vpn_rollback_using(
        &mut spec,
        &provider,
        &rt,
        |_spec| anyhow::bail!("ensure failed"),
        record_removal(&calls),
    );

    assert!(result.is_err());
    assert!(calls.downs.lock().unwrap().is_empty());
    assert!(calls.removals.lock().unwrap().is_empty());
}

#[test]
fn bring_up_failure_rolls_back_partial_sidecar() {
    let mut spec = vpn_spec(VpnOnError::Warn);
    let mut provider = fake(false);
    provider.up_fails = true;
    let calls = provider.calls.clone();
    let sidecar = thegn_core::sandbox::vpn_sidecar_name(&spec.name);
    let rt = OciRuntime::new(vec!["fake-runtime".into()]);

    attach_vpn_with(&mut spec, &provider, &rt, record_removal(&calls)).unwrap();

    assert!(spec.vpn.is_none());
    assert_eq!(*calls.ups.lock().unwrap(), vec![sidecar.clone()]);
    assert!(calls.readies.lock().unwrap().is_empty());
    assert_eq!(*calls.downs.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.removals.lock().unwrap(), vec![sidecar]);
}

fn record_removal(calls: &Calls) -> impl FnOnce(&OciRuntime, &str) -> anyhow::Result<()> + '_ {
    move |_rt, sidecar| {
        calls.removals.lock().unwrap().push(sidecar.to_string());
        Ok(())
    }
}

#[test]
fn readiness_failure_tears_down_exact_sidecar_under_fail_policy() {
    let mut spec = vpn_spec(VpnOnError::Fail);
    let provider = fake(true);
    let calls = provider.calls.clone();
    let sidecar = thegn_core::sandbox::vpn_sidecar_name(&spec.name);
    let rt = OciRuntime::new(vec!["fake-runtime".into()]);

    assert!(attach_vpn_with(&mut spec, &provider, &rt, record_removal(&calls)).is_err());
    assert_eq!(*calls.ups.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.readies.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.downs.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.removals.lock().unwrap(), vec![sidecar]);
}

#[test]
fn readiness_failure_warn_clears_vpn_after_teardown() {
    let mut spec = vpn_spec(VpnOnError::Warn);
    let provider = fake(true);
    let calls = provider.calls.clone();
    let sidecar = thegn_core::sandbox::vpn_sidecar_name(&spec.name);
    let rt = OciRuntime::new(vec!["fake-runtime".into()]);

    attach_vpn_with(&mut spec, &provider, &rt, record_removal(&calls)).unwrap();
    assert!(spec.vpn.is_none());
    assert_eq!(*calls.ups.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.readies.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.downs.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.removals.lock().unwrap(), vec![sidecar]);
}

#[test]
fn later_launch_failure_tears_down_attached_sidecar() {
    let mut spec = vpn_spec(VpnOnError::Fail);
    let provider = fake(false);
    let calls = provider.calls.clone();
    let sidecar = thegn_core::sandbox::vpn_sidecar_name(&spec.name);
    let rt = OciRuntime::new(vec!["fake-runtime".into()]);
    attach_vpn_with(&mut spec, &provider, &rt, record_removal(&calls)).unwrap();

    let result: anyhow::Result<()> = run_with_vpn_rollback_using(
        &mut spec,
        &provider,
        &rt,
        |_spec| anyhow::bail!("ensure failed"),
        record_removal(&calls),
    );

    assert!(result.is_err());
    assert_eq!(*calls.ups.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.readies.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.downs.lock().unwrap(), vec![sidecar.clone()]);
    assert_eq!(*calls.removals.lock().unwrap(), vec![sidecar]);
}
