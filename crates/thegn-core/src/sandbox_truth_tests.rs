//! The containment-truth gate. These tests are the reason a pane cannot claim a
//! sandbox it does not have: [`every_backend_round_trips`] renders the REAL
//! `enter_argv` for every backend and asserts the observed backend matches, and
//! the list it walks is exhaustive by construction (a new `Backend` variant
//! fails to compile here), so the check cannot rot by omission.

use super::*;
use crate::config::{FileAccess, Network};
use crate::placement::Placement;
use crate::sandbox::{Mount, SandboxLimits, SandboxSpec, enter_argv};
use std::path::PathBuf;

fn spec(backend: Backend) -> SandboxSpec {
    SandboxSpec {
        backend,
        placement: Placement::Local,
        image: Some("img:latest".into()),
        gitshim_files: Vec::new(),
        git_autocrlf: None,
        worktree: PathBuf::from("/wt/feat"),
        mounts: vec![Mount {
            host: "/wt/feat".into(),
            dest: "/wt/feat".into(),
            ro: false,
            cache: false,
        }],
        env: vec![],
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
        ports: vec![],
        gpu: None,
        // CPU capping off: it scope-wraps the argv on a host with cgroup
        // delegation, which would make these assertions host-dependent.
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

/// Every backend, exhaustively. The dummy `match` makes a newly added variant a
/// COMPILE error here rather than a silently unchecked backend — the failure
/// mode that let a container label drift away from what actually runs.
fn all_backends() -> Vec<Backend> {
    fn _exhaustive(b: Backend) {
        match b {
            Backend::Podman
            | Backend::PodmanRootful
            | Backend::Docker
            | Backend::Smol
            | Backend::Apple
            | Backend::Wsl
            | Backend::Bwrap
            | Backend::Systemd
            | Backend::WinAppContainer
            | Backend::WinJobObject
            | Backend::None => (),
        }
    }
    vec![
        Backend::Podman,
        Backend::PodmanRootful,
        Backend::Docker,
        Backend::Smol,
        Backend::Apple,
        Backend::Wsl,
        Backend::Bwrap,
        Backend::Systemd,
        Backend::WinAppContainer,
        Backend::WinJobObject,
        Backend::None,
    ]
}

/// `jobobject` containment would happen in the spawn syscall, invisibly to argv
/// inspection, so it is excluded from the round trip and pinned separately in
/// [`windows_native_is_taken_at_its_word`].
///
/// `appcontainer` is NOT excluded any more: the token cannot be attached to the
/// ConPTY spawn, so its pane is routed through thegn's own `appcontainer-exec`
/// trampoline — which the argv shows, so it round-trips like everything else.
fn argv_visible(b: Backend) -> bool {
    !matches!(b, Backend::WinJobObject)
}

#[test]
fn every_backend_round_trips() {
    for b in all_backends().into_iter().filter(|b| argv_visible(*b)) {
        let argv = enter_argv(&spec(b), "zsh");
        assert_eq!(
            observed(&argv),
            b,
            "backend {} renders an argv that reads as {:?}: {argv:?}",
            b.label(),
            observed(&argv)
        );
    }
}

#[test]
fn host_shell_is_never_read_as_contained() {
    // Exactly what a degraded `podman-rootless` terminal spawns today.
    let argv = vec![
        "/bin/sh".to_string(),
        "-lc".into(),
        "cd '/Users/me/code' && exec '/bin/zsh' -l".into(),
    ];
    assert_eq!(observed(&argv), Backend::None);
}

#[test]
fn a_path_named_after_a_runtime_is_not_containment() {
    // The dangerous direction: an ARGUMENT that happens to be named `docker`
    // must never promote a host shell into a claimed container.
    for path in [
        "/Users/me/code/docker",
        "/home/me/podman",
        "/srv/bwrap/worktree",
    ] {
        let argv = vec![
            "/bin/sh".to_string(),
            "-lc".into(),
            format!("cd '{path}' && exec zsh"),
        ];
        assert_eq!(observed(&argv), Backend::None, "path {path}");
    }
    // Same for a git remote or an image name passed as an argument.
    let argv = vec![
        "/bin/sh".to_string(),
        "-lc".into(),
        "git clone https://github.com/me/docker && exec zsh".into(),
    ];
    assert_eq!(observed(&argv), Backend::None);
}

#[test]
fn rootful_podman_is_distinguished_from_rootless() {
    let rootless = enter_argv(&spec(Backend::Podman), "zsh");
    let rootful = enter_argv(&spec(Backend::PodmanRootful), "zsh");
    assert_eq!(observed(&rootless), Backend::Podman);
    assert_eq!(observed(&rootful), Backend::PodmanRootful);
    assert_ne!(observed(&rootless), observed(&rootful));
}

#[test]
fn remote_placement_still_reports_the_runtime() {
    // An ssh placement appends the container command as a remote script; the
    // runtime is inside that string, and must still be seen.
    let argv = vec![
        "ssh".to_string(),
        "-t".into(),
        "box".into(),
        "podman exec -it thegn-repo-feat /bin/sh -lc 'exec zsh'".into(),
    ];
    assert_eq!(observed(&argv), Backend::Podman);
}

#[test]
fn a_transport_handing_off_after_dashdash_is_seen() {
    // `kubectl exec … -- podman exec …` and `wsl.exe -- podman …`: the runtime
    // sits after the end-of-flags separator, not in argv[0].
    let argv = vec![
        "kubectl".to_string(),
        "exec".into(),
        "-it".into(),
        "pod/thegn".into(),
        "--".into(),
        "podman".into(),
        "exec".into(),
        "-it".into(),
        "thegn-repo-feat".into(),
    ];
    assert_eq!(observed(&argv), Backend::Podman);
}

#[test]
fn reconcile_reports_the_truth_not_the_request() {
    // The reported bug: pick rootless podman, get a bare shell.
    let host_argv = vec![
        "/bin/sh".to_string(),
        "-lc".into(),
        "cd '/Users/me' && exec zsh".into(),
    ];
    let t = reconcile("podman-rootless", &host_argv);
    assert_eq!(t.label, "host", "the label must describe the argv");
    assert!(
        t.degraded,
        "an unhonoured container request is a degradation"
    );
    assert!(
        t.warning.as_deref().is_some_and(|w| w.contains("host")),
        "the user must be told: {:?}",
        t.warning
    );
}

#[test]
fn reconcile_is_quiet_when_the_request_was_honoured() {
    for name in ["podman-rootless", "docker", "bwrap"] {
        let b = crate::config::SandboxBackend::from_str_validated(name)
            .ok()
            .and_then(Backend::from_config)
            .unwrap();
        let t = reconcile(name, &enter_argv(&spec(b), "zsh"));
        assert_eq!(t.label, b.label(), "{name}");
        assert!(!t.degraded, "{name}");
        assert_eq!(t.warning, None, "{name}");
    }
}

#[test]
fn host_and_auto_requests_are_not_degradations() {
    let host_argv = vec!["/bin/sh".to_string(), "-lc".into(), "exec zsh".to_string()];
    for name in ["", "auto", "host", "none"] {
        let t = reconcile(name, &host_argv);
        assert_eq!(t.label, "host", "{name:?}");
        assert!(!t.degraded, "{name:?}");
        assert_eq!(t.warning, None, "{name:?}");
    }
}

#[test]
fn falling_back_to_a_different_container_is_still_reported() {
    let t = reconcile("docker", &enter_argv(&spec(Backend::Podman), "zsh"));
    assert_eq!(t.label, "podman-rootless");
    assert!(t.degraded);
    assert!(t.warning.is_some_and(|w| w.contains("podman-rootless")));
}

#[test]
fn windows_native_is_taken_at_its_word() {
    // Documented limit, and now only ONE backend has it: a Job Object would be
    // applied in the spawn syscall, invisibly to argv inspection, so reconcile
    // trusts the request rather than reporting a false "host". (It is never
    // selected either — `available_probe` reports it Absent because nothing
    // actually assigns a pane to a job.)
    let argv = enter_argv(&spec(Backend::WinJobObject), "zsh");
    assert_eq!(observed(&argv), Backend::None, "argv cannot show it");
    let t = reconcile(Backend::WinJobObject.label(), &argv);
    assert_eq!(t.label, Backend::WinJobObject.label());
    assert!(!t.degraded);
}

/// AppContainer is the one win-native backend whose containment IS checkable
/// from the argv, and it must stay that way: if the pane ever stopped going
/// through the trampoline, the token would not be applied at all, and a truth
/// check that trusted the request would report a boundary that does not exist.
#[test]
fn appcontainer_containment_is_visible_in_the_argv() {
    let argv = enter_argv(&spec(Backend::WinAppContainer), "zsh");
    assert_eq!(observed(&argv), Backend::WinAppContainer, "{argv:?}");
    assert!(
        argv.iter().any(|a| a == "appcontainer-exec"),
        "the pane must route through the trampoline: {argv:?}"
    );
    // The profile is what ties the token to this worktree; without it the pane
    // would land in some other worktree's container.
    assert!(argv.iter().any(|a| a == "--profile"), "{argv:?}");
    let t = reconcile(Backend::WinAppContainer.label(), &argv);
    assert_eq!(t.label, Backend::WinAppContainer.label());
    assert!(!t.degraded);
}

/// The trampoline is recognised by POSITION, so merely mentioning it must not
/// promote a host shell into a claimed container — the same guarantee the
/// command-word scan gives every other backend.
#[test]
fn merely_naming_the_trampoline_is_not_containment() {
    for argv in [
        // A host shell that happens to echo the word.
        vec![
            "/bin/sh".to_string(),
            "-lc".into(),
            "echo appcontainer-exec".into(),
        ],
        // …and one where it is an argument rather than the subcommand.
        vec![
            "/bin/sh".to_string(),
            "-lc".into(),
            "grep appcontainer-exec log".into(),
        ],
    ] {
        assert_eq!(
            observed(&argv),
            Backend::None,
            "a host shell must not read as contained: {argv:?}"
        );
        assert!(
            reconcile("appcontainer", &argv).degraded,
            "asking for appcontainer and getting a host shell IS a degrade: {argv:?}"
        );
    }
}
