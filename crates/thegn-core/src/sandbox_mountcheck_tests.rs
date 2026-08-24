//! Tests for the in-container mount verification. Every function under test is
//! pure — `host_exists` and `have` are injected — so the whole matrix runs from
//! one machine, and the Linux arms are exercised on a Mac and vice versa.

use super::*;
use crate::config::FileAccess;
use crate::config::Network;
use crate::placement::Placement;
use crate::sandbox::{Backend, Mount, SandboxLimits, SandboxSpec};
use std::path::PathBuf;

/// A spec with a worktree at `/wt/feat` and its repo's git-common dir bound —
/// the same shape `sandbox_tests::spec` uses, kept local because that fixture is
/// private to its own test module.
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
        env: Vec::new(),
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
        ports: Vec::new(),
        gpu: None,
        limits: SandboxLimits::default(),
        volumes: Vec::new(),
        compose: None,
        build: None,
        init_script: None,
        file_access: FileAccess::WorktreePlusCaches,
        devenv: false,
        devenv_path: None,
        name: "thegn-repo-feat".into(),
        vpn: None,
        oci_host: None,
        oci_runtime: None,
        daemon_persistent: false,
    }
}

/// `host_exists` for a fixed set of paths.
fn has(paths: &'static [&'static str]) -> impl Fn(&str) -> bool {
    move |p: &str| paths.contains(&p)
}

#[test]
fn no_worktree_mount_yields_no_sentinel() {
    // `file_access = none` mounts nothing, so asserting anything about the
    // container would be exactly the false positive this must not produce.
    let mut s = spec(Backend::Podman);
    s.file_access = FileAccess::None;
    assert!(mount_sentinels(&s, &has(&["/wt/feat/.git"])).is_empty());
}

#[test]
fn unprovable_host_yields_todays_exact_probe() {
    // The no-regression lock. When the host has none of the sentinels we cannot
    // prove a failure, so the probe body must be the literal `true` it was
    // before this module existed.
    let s = spec(Backend::Podman);
    let sentinels = mount_sentinels(&s, &|_| false);
    assert!(sentinels.is_empty());
    assert_eq!(preflight_probe_body(&sentinels), "true");
}

#[test]
fn dot_git_is_the_sentinel_for_both_worktree_shapes() {
    // A linked worktree's `.git` is a FILE (holding the absolute `gitdir:`
    // pointer); the main checkout's is a DIRECTORY. `-e` covers both, which is
    // why it is the sentinel — it is the one thing every worktree has.
    let s = spec(Backend::Podman);
    let got = mount_sentinels(&s, &has(&["/wt/feat/.git"]));
    assert_eq!(got, vec!["/wt/feat/.git".to_string()]);
}

#[test]
fn git_common_head_is_added_when_the_host_has_it() {
    // The git-common dir is a separate bind and can live on a different volume,
    // so it can fail independently of the worktree.
    let s = spec(Backend::Podman);
    let got = mount_sentinels(&s, &has(&["/wt/feat/.git", "/repo/.git/HEAD"]));
    assert_eq!(got, vec!["/wt/feat/.git", "/repo/.git/HEAD"]);
}

#[test]
fn toolchain_and_cache_mounts_are_never_sentinels() {
    // $HOME and /nix have no HEAD, so `host_exists` filters them out for free —
    // which matters because podman machine does NOT share /nix, and asserting it
    // would fail the backend for every Nix-on-Mac user.
    let mut s = spec(Backend::Podman);
    s.mounts.push(Mount {
        host: "/nix".into(),
        dest: "/nix".into(),
        ro: true,
        cache: false,
    });
    s.mounts.push(Mount {
        host: "/home/me".into(),
        dest: "/home/me".into(),
        ro: true,
        cache: false,
    });
    let got = mount_sentinels(&s, &has(&["/wt/feat/.git"]));
    assert_eq!(got, vec!["/wt/feat/.git".to_string()]);
}

#[test]
fn sentinel_uses_the_container_dest_not_the_host_path() {
    // A user `[sandbox] mounts` entry can remap the two; the probe runs INSIDE
    // the container, so it must assert the destination.
    let mut s = spec(Backend::Podman);
    s.mounts[1] = Mount {
        host: "/host/repo/.git".into(),
        dest: "/guest/repo/.git".into(),
        ro: false,
        cache: false,
    };
    let got = mount_sentinels(&s, &has(&["/wt/feat/.git", "/host/repo/.git/HEAD"]));
    assert_eq!(got, vec!["/wt/feat/.git", "/guest/repo/.git/HEAD"]);
}

#[test]
fn a_non_local_placement_asserts_nothing() {
    // The files live on a machine whose filesystem we never inspected. Do not
    // invent facts about it.
    let mut s = spec(Backend::Podman);
    s.placement = Placement::Ssh(crate::placement::SshPlacement::plain(
        "box".into(),
        22,
        false,
        crate::placement::TransportKind::Ssh,
    ));
    assert!(mount_sentinels(&s, &|_| true).is_empty());
}

#[test]
fn a_compose_service_asserts_nothing() {
    // The last of the four "cannot prove it" escape hatches, and the subtlest:
    // the pane enters via `compose exec <service>`, a DIFFERENT container than
    // the one the preflight probe targets. Asserting the worktree here would
    // fail a compose sandbox whose bind is perfectly fine — a pre-existing
    // mismatch this module deliberately does not widen.
    let mut s = spec(Backend::Docker);
    s.compose = Some(
        crate::sandbox_compose::ComposeSpec {
            files: vec!["docker-compose.yml".into()],
            service: Some("dev".into()),
            run_services: Vec::new(),
        }
        .encode(),
    );
    assert!(mount_sentinels(&s, &has(&["/wt/feat/.git"])).is_empty());

    // A compose spec with no service DOES enter the probed container, so it is
    // checked like any other — the gate is the service, not compose itself.
    s.compose = Some("docker-compose.yml".to_string());
    assert_eq!(
        mount_sentinels(&s, &has(&["/wt/feat/.git"])),
        vec!["/wt/feat/.git".to_string()]
    );
}

#[test]
fn probe_body_quotes_paths_and_marks_the_missing_one() {
    let body = preflight_probe_body(&["/a b/.git".to_string()]);
    // A path with a space must survive as one word.
    assert!(body.contains("'/a b/.git'"), "{body}");
    assert!(body.contains(MOUNT_MISSING_MARKER), "{body}");
    // Non-zero exit drives the existing `success() == false` control flow.
    assert!(body.contains("exit 97"), "{body}");
}

#[test]
fn parse_missing_sentinel_survives_login_shell_noise() {
    let noisy = format!(
        "/etc/profile: line 3: warning\nsome other chatter\n{MOUNT_MISSING_MARKER}/wt/feat/.git\n"
    );
    assert_eq!(parse_missing_sentinel(&noisy), Some("/wt/feat/.git"));
    // Unrelated stderr must not be read as a mount failure — that would turn a
    // genuine runtime error into a misleading "widen your VM share".
    assert_eq!(parse_missing_sentinel(""), None);
    assert_eq!(parse_missing_sentinel("crun: exec failed: ENOENT"), None);
    assert_eq!(
        parse_missing_sentinel(&format!("{MOUNT_MISSING_MARKER}   ")),
        None
    );
}

#[test]
fn share_root_takes_the_first_component() {
    assert_eq!(share_root("/Volumes/ext/code/x"), Some("/Volumes"));
    assert_eq!(share_root("/Users/me/code"), Some("/Users"));
    assert_eq!(share_root("/opt"), Some("/opt"));
    assert_eq!(share_root("/"), None);
    assert_eq!(share_root("relative/path"), None);
}

fn probe<'a>(os: HostOs, backend: Backend, worktree: &'a str, missing: &'a str) -> MountProbe<'a> {
    MountProbe {
        backend,
        os,
        file_access: FileAccess::WorktreePlusCaches,
        worktree,
        missing,
    }
}

#[test]
fn remedy_is_runtime_and_os_specific() {
    let no_colima = |_: &str| false;
    let colima = |b: &str| b == "colima";

    // podman: name the recreate, and that `machine set` cannot do it.
    let m = mount_failure(
        &probe(
            HostOs::MacOs,
            Backend::Podman,
            "/Volumes/ext/repo",
            "/Volumes/ext/repo/.git",
        ),
        &no_colima,
    );
    assert!(
        m.remedy
            .contains("podman machine init -v /Volumes:/Volumes"),
        "{}",
        m.remedy
    );
    assert!(m.remedy.contains("no --volume"), "{}", m.remedy);

    // docker + colima: colima's own flag, not Docker Desktop's settings pane.
    let m = mount_failure(
        &probe(
            HostOs::MacOs,
            Backend::Docker,
            "/Volumes/ext/repo",
            "/Volumes/ext/repo/.git",
        ),
        &colima,
    );
    assert!(
        m.remedy.contains("colima start -V /Volumes:w"),
        "{}",
        m.remedy
    );

    // docker without colima: Docker Desktop.
    let m = mount_failure(
        &probe(
            HostOs::MacOs,
            Backend::Docker,
            "/Volumes/ext/repo",
            "/Volumes/ext/repo/.git",
        ),
        &no_colima,
    );
    assert!(m.remedy.contains("File Sharing"), "{}", m.remedy);

    // Linux: the honest local causes, and NO mention of a VM, machine or colima.
    // This is the Linux-regression guard, mirroring `sandbox_support`'s.
    let m = mount_failure(
        &probe(
            HostOs::Linux,
            Backend::Podman,
            "/srv/repo",
            "/srv/repo/.git",
        ),
        &colima,
    );
    for forbidden in ["machine", "colima", "Docker Desktop", "VM"] {
        assert!(
            !m.remedy.contains(forbidden),
            "Linux remedy must not mention {forbidden}: {}",
            m.remedy
        );
    }
    assert!(m.remedy.contains(":z"), "{}", m.remedy);
}

#[test]
fn an_already_shared_root_gets_a_different_remedy() {
    // Telling someone to add /Users when podman already shares /Users sends them
    // to recreate a machine for nothing, and hides the real cause.
    let m = mount_failure(
        &probe(
            HostOs::MacOs,
            Backend::Podman,
            "/Users/me/code/repo",
            "/Users/me/code/repo/.git",
        ),
        &|_| false,
    );
    assert!(m.remedy.contains("already shared"), "{}", m.remedy);
    assert!(!m.remedy.contains("machine init"), "{}", m.remedy);
    assert!(m.remedy.contains("podman machine ssh"), "{}", m.remedy);
}

#[test]
fn apple_and_windows_get_their_own_remedy() {
    // Apple's `container` has no user-facing share setting at all, so the only
    // honest advice is to move the worktree — telling someone to widen a share
    // that does not exist would be worse than saying nothing.
    let m = mount_failure(
        &probe(
            HostOs::MacOs,
            Backend::Apple,
            "/Volumes/ext/repo",
            "/Volumes/ext/repo/.git",
        ),
        &|_| false,
    );
    assert!(m.remedy.contains("/Volumes"), "{}", m.remedy);
    assert!(m.remedy.contains("under /Users"), "{}", m.remedy);
    assert!(!m.remedy.contains("colima"), "{}", m.remedy);

    // Windows reaches its Linux container through a VM too, so the diagnosis is
    // the same shape as macOS — but never names a macOS-only tool.
    let m = mount_failure(
        &probe(HostOs::Windows, Backend::Docker, "/c/repo", "/c/repo/.git"),
        &|b| b == "colima",
    );
    assert!(m.remedy.contains("/c"), "{}", m.remedy);
    for forbidden in ["colima", "podman machine"] {
        assert!(!m.remedy.contains(forbidden), "{}", m.remedy);
    }
}

#[test]
fn the_remedy_names_the_missing_paths_root_not_the_worktrees() {
    // The cross-volume case, and the reason the root comes from `missing`: a main
    // repo on an external disk with its linked worktree under $HOME. /Users is
    // shared and working; /Volumes is the one that failed. Keying the remedy off
    // the worktree would print "/Users is already shared, so the share is not the
    // problem" — dead wrong, and it hides the only fix.
    let m = mount_failure(
        &probe(
            HostOs::MacOs,
            Backend::Podman,
            "/Users/me/wt/feat",
            "/Volumes/ext/repo/.git/HEAD",
        ),
        &|_| false,
    );
    assert!(
        m.remedy
            .contains("podman machine init -v /Volumes:/Volumes"),
        "{}",
        m.remedy
    );
    assert!(!m.remedy.contains("already shared"), "{}", m.remedy);
}

#[test]
fn a_root_bind_on_macos_is_diagnosed_as_the_vm_root() {
    // `file_access = all` binds `/:/`. Inside a VM that binds the VM's root, so
    // widening a share would not help — different cause, different remedy.
    let mut p = probe(
        HostOs::MacOs,
        Backend::Podman,
        "/Users/me/repo",
        "/etc/passwd",
    );
    p.file_access = FileAccess::All;
    let m = mount_failure(&p, &|_| false);
    assert!(m.remedy.contains("VM's root"), "{}", m.remedy);
    assert!(m.remedy.contains("file_access"), "{}", m.remedy);
}

#[test]
fn headline_names_the_missing_path_and_comes_first() {
    // The warning lands in a width-fitted status line, so truncation must cost
    // the remedy and never the diagnosis.
    let m = mount_failure(
        &probe(
            HostOs::MacOs,
            Backend::Podman,
            "/Volumes/ext/repo",
            "/Volumes/ext/repo/.git",
        ),
        &|_| false,
    );
    assert!(
        m.headline.contains("/Volumes/ext/repo/.git"),
        "{}",
        m.headline
    );
    assert!(m.headline.contains("podman"), "{}", m.headline);
    assert!(m.one_line().starts_with(&m.headline));
}

#[test]
fn parse_unshared_bind_reads_the_runtimes_own_words() {
    // Verified verbatim against podman 5.8.6 on macOS 26: `run` exits 125 with
    // exactly this line and creates no container.
    assert_eq!(
        parse_unshared_bind("Error: statfs /opt/homebrew/repo/.git: no such file or directory"),
        Some("/opt/homebrew/repo/.git")
    );
    // docker's phrasing for the same condition.
    assert_eq!(
        parse_unshared_bind(
            "docker: Error response from daemon: invalid mount config for type \"bind\": \
             bind source path does not exist: /Volumes/ext/repo"
        ),
        Some("/Volumes/ext/repo")
    );
    // An unrecognized failure must fall through to the generic error rather than
    // being dressed up as a share problem — a wrong remedy is worse than none.
    assert_eq!(parse_unshared_bind(""), None);
    assert_eq!(
        parse_unshared_bind("Error: crun: cannot set memory limit: Operation not permitted"),
        None
    );
    // Relative paths are not bind sources; never build a remedy from one.
    assert_eq!(parse_unshared_bind("Error: statfs foo: no such file"), None);
}
