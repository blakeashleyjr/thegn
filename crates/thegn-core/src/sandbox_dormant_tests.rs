//! Dormant-runtime policy. The decision is pure, so every branch is pinned here
//! rather than only being reachable through a live launch with a stopped daemon.

use super::*;
use crate::capabilities::IsolationClass;

fn support(backend: Backend, name: &str, state: BackendState) -> BackendSupport {
    BackendSupport {
        backend,
        name: name.into(),
        state,
        isolation: Some(IsolationClass::SharedKernel),
        remedy: Some("start it somehow".into()),
        caveat: crate::sandbox_support::caveat_for(backend, state),
    }
}

fn nothing_installed(_: &str) -> bool {
    false
}
fn colima_installed(bin: &str) -> bool {
    bin == "colima"
}

#[test]
fn podman_starts_its_machine_off_linux_and_its_socket_on_linux() {
    assert_eq!(
        start_argv(Backend::Podman, HostOs::MacOs, &nothing_installed),
        Some(vec!["podman".into(), "machine".into(), "start".into()])
    );
    // Rootless podman on Linux needs no VM — a stopped one is a user service.
    assert_eq!(
        start_argv(Backend::Podman, HostOs::Linux, &nothing_installed),
        Some(vec![
            "systemctl".into(),
            "--user".into(),
            "start".into(),
            "podman.socket".into()
        ])
    );
}

#[test]
fn macos_docker_prefers_colima_and_falls_back_to_the_desktop_app() {
    // There is no `dockerd` on macOS: the runtime is whichever VM is installed.
    assert_eq!(
        start_argv(Backend::Docker, HostOs::MacOs, &colima_installed),
        Some(vec!["colima".into(), "start".into()])
    );
    // Docker Desktop is a GUI app, so it is launched, not service-started.
    assert_eq!(
        start_argv(Backend::Docker, HostOs::MacOs, &nothing_installed),
        Some(vec!["open".into(), "-a".into(), "Docker".into()])
    );
    // Linux keeps the daemon verb.
    assert_eq!(
        start_argv(Backend::Docker, HostOs::Linux, &nothing_installed),
        Some(vec!["systemctl".into(), "start".into(), "docker".into()])
    );
}

#[test]
fn rootful_podman_is_never_started_for_the_user() {
    // It needs a password, and a launch path must not drive an interactive sudo.
    assert_eq!(
        start_argv(Backend::PodmanRootful, HostOs::Linux, &nothing_installed),
        None
    );
}

#[test]
fn first_dormant_picks_the_runtime_the_chain_would_have_used() {
    let report = vec![
        support(
            Backend::Podman,
            "podman-rootless",
            BackendState::NotInstalled,
        ),
        support(Backend::Docker, "docker", BackendState::NotRunning),
        support(Backend::Bwrap, "bwrap", BackendState::NotRunning),
    ];
    let d = first_dormant(&report).expect("a dormant runtime");
    assert_eq!(
        d.name, "docker",
        "chain order decides, not backend identity"
    );

    // Nothing merely stopped ⇒ nothing to offer.
    let none_dormant = vec![support(
        Backend::Podman,
        "podman-rootless",
        BackendState::NotInstalled,
    )];
    assert!(first_dormant(&none_dormant).is_none());
}

fn dormant_docker(os: HostOs, have: &dyn Fn(&str) -> bool) -> DormantRuntime {
    runtime_for(
        &support(Backend::Docker, "docker", BackendState::NotRunning),
        os,
        have,
    )
}

#[test]
fn an_uncontained_launch_is_never_interrupted() {
    // `auto`/`host` landing on the host is the configured outcome, not a
    // degradation — prompting there would nag every plain shell.
    let rt = dormant_docker(HostOs::Linux, &nothing_installed);
    for policy in [
        OnDormant::Ask,
        OnDormant::Start,
        OnDormant::Host,
        OnDormant::Cancel,
    ] {
        assert_eq!(
            decide(policy, false, Some(rt.clone())),
            DormantAction::Proceed,
            "{policy:?}"
        );
    }
}

#[test]
fn nothing_dormant_means_nothing_to_decide() {
    // Even `cancel`: refusing to launch would strand a user whose runtime simply
    // is not installed, which no prompt can fix.
    for policy in [OnDormant::Ask, OnDormant::Cancel, OnDormant::Start] {
        assert_eq!(decide(policy, true, None), DormantAction::Proceed);
    }
}

#[test]
fn each_policy_takes_its_branch() {
    let rt = dormant_docker(HostOs::Linux, &nothing_installed);
    assert_eq!(
        decide(OnDormant::Host, true, Some(rt.clone())),
        DormantAction::Proceed
    );
    assert_eq!(
        decide(OnDormant::Cancel, true, Some(rt.clone())),
        DormantAction::Cancel(rt.clone())
    );
    assert_eq!(
        decide(OnDormant::Ask, true, Some(rt.clone())),
        DormantAction::Ask(rt.clone())
    );
    assert_eq!(
        decide(OnDormant::Start, true, Some(rt.clone())),
        DormantAction::Start(rt)
    );
}

#[test]
fn start_downgrades_to_ask_when_there_is_no_command_to_run() {
    // Rootful podman has no unattended start. `start` must not silently become
    // "host anyway" — the user is told instead.
    let rt = runtime_for(
        &support(
            Backend::PodmanRootful,
            "podman-rootful",
            BackendState::NotRunning,
        ),
        HostOs::Linux,
        &nothing_installed,
    );
    assert_eq!(rt.start_argv, None);
    assert_eq!(
        decide(OnDormant::Start, true, Some(rt.clone())),
        DormantAction::Ask(rt)
    );
}

#[test]
fn the_offer_carries_both_the_command_and_the_sentence() {
    let rt = dormant_docker(HostOs::MacOs, &colima_installed);
    assert_eq!(rt.start_display().as_deref(), Some("colima start"));
    assert_eq!(rt.remedy, "start it somehow");
    assert_eq!(rt.backend, Backend::Docker);
}

#[test]
fn the_macos_docker_remedy_sentence_does_not_name_systemd() {
    // The old sentence sent Mac users to `systemctl`, which does not exist there.
    let r = crate::sandbox_support::remedy_for_test(Backend::Docker, BackendState::NotRunning);
    if matches!(crate::sandbox_backend::host_os(), HostOs::MacOs) {
        let r = r.expect("a remedy");
        assert!(!r.contains("systemctl"), "{r}");
        assert!(r.contains("colima"), "{r}");
    }
}
