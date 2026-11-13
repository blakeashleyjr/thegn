//! Red-team coverage for autonomous credential-free sandbox launches.
//!
//! The queue runner removes credential-shaped environment and the credential
//! directories from a resolved spec before handing it to [`enter_argv`]. Keep
//! this test at the core launch boundary: it checks the final serialized argv,
//! environment pairs, and mount plan together, with sentinel data that cannot
//! be mistaken for an ordinary fixture path.

use super::*;
use crate::config::{FileAccess, Network};
use crate::placement::Placement;
use crate::sandbox::{Mount, SandboxLimits, SandboxSpec, enter_argv};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SENTINEL_VALUES: &[(&str, &str)] = &[
    ("SSH_AUTH_SOCK", "redteam-ssh-auth-sock-7f3a"),
    ("GH_TOKEN", "redteam-gh-token-31c9"),
    ("GITHUB_TOKEN", "redteam-github-token-9b12"),
    ("ANTHROPIC_API_KEY", "redteam-anthropic-key-4d8e"),
    ("GNUPGHOME", "redteam-gnupg-home-2e77"),
    ("GH_CONFIG_DIR", "redteam-gh-config-5a04"),
    ("GIT_SSH_COMMAND", "redteam-git-ssh-command-c6d1"),
    ("GPG_TTY", "redteam-gpg-tty-6a20"),
    ("GPG_AGENT_INFO", "redteam-gpg-agent-8c51"),
];

fn sentinel_home_files(home: &Path) -> Vec<PathBuf> {
    [
        ".ssh/id_ed25519",
        ".gnupg/private-keys-v1.d/key",
        ".config/gh/hosts.yml",
    ]
    .into_iter()
    .map(|relative| home.join(relative))
    .collect()
}

fn redteam_spec(home: &Path, worktree: &Path) -> SandboxSpec {
    let home = home.to_string_lossy().into_owned();
    let credential_mounts = sentinel_home_files(Path::new(&home))
        .into_iter()
        .map(|path| {
            let path = path.to_string_lossy().into_owned();
            Mount {
                host: path.clone(),
                dest: path,
                ro: true,
                cache: false,
            }
        });
    let mut env = SENTINEL_VALUES
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect::<Vec<_>>();
    env.push(("TERM".into(), "xterm-redteam-control".into()));
    env.push(("THEGN_SANDBOX".into(), "1".into()));

    SandboxSpec {
        backend: Backend::Bwrap,
        placement: Placement::Local,
        image: None,
        worktree: worktree.to_path_buf(),
        mounts: std::iter::once(Mount {
            host: home.clone(),
            dest: home,
            ro: true,
            cache: false,
        })
        .chain(credential_mounts)
        .collect(),
        env,
        env_overrides: HashMap::new(),
        env_block: Vec::new(),
        network: Network::Nat,
        network_allow: Vec::new(),
        network_block: Vec::new(),
        read_only_root: false,
        no_new_privileges: false,
        pids_limit: None,
        drop_capabilities: Vec::new(),
        add_capabilities: Vec::new(),
        file_access: FileAccess::Worktree,
        ports: Vec::new(),
        gpu: None,
        limits: SandboxLimits {
            // Keep the test independent of host cgroup delegation.
            cpu_total: Some("off".into()),
            ..SandboxLimits::default()
        },
        volumes: Vec::new(),
        compose: None,
        build: None,
        init_script: None,
        devenv: false,
        devenv_path: None,
        name: "thegn-redteam-agent".into(),
        vpn: None,
        oci_host: None,
        oci_runtime: None,
        daemon_persistent: false,
        seal_home: None,
        volume_owners: Vec::new(),
    }
}

fn credential_free_spec(mut spec: SandboxSpec, home: &Path) -> SandboxSpec {
    spec.env.retain(|(key, _)| {
        !matches!(
            key.as_str(),
            "SSH_AUTH_SOCK"
                | "GH_TOKEN"
                | "GITHUB_TOKEN"
                | "ANTHROPIC_API_KEY"
                | "GNUPGHOME"
                | "GH_CONFIG_DIR"
                | "GIT_SSH_COMMAND"
                | "GPG_TTY"
                | "GPG_AGENT_INFO"
        )
    });
    let credential_dirs = [
        home.join(".ssh"),
        home.join(".gnupg"),
        home.join(".config/gh"),
    ];
    spec.mounts.retain(|mount| {
        let destination = Path::new(&mount.dest);
        !credential_dirs
            .iter()
            .any(|credential_dir| destination.starts_with(credential_dir))
    });
    spec
}

fn assert_no_sentinel<T>(values: impl IntoIterator<Item = T>, sentinels: &[String], context: &str)
where
    T: AsRef<str>,
{
    for value in values {
        let value = value.as_ref();
        for sentinel in sentinels {
            assert!(
                !value.contains(sentinel),
                "{context} contains sentinel {sentinel}: {value}"
            );
        }
    }
}

#[test]
fn autonomous_credential_free_launch_has_no_sentinel_value_path_or_mount() {
    let state = crate::testenv::IsolatedState::new();
    let home = state.root().to_path_buf();
    let worktree = home.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    for file in sentinel_home_files(&home) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, b"redteam file sentinel").unwrap();
    }
    let _env = crate::testenv::EnvGuard::set(SENTINEL_VALUES);

    let interactive = redteam_spec(&home, &worktree);
    let autonomous = credential_free_spec(interactive.clone(), &home);
    let argv = enter_argv(&autonomous, "claude").expect("red-team launch argv");
    let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
    let env_pairs = autonomous
        .env
        .iter()
        .flat_map(|(key, value)| [key.as_str(), value.as_str()]);
    let mounts = autonomous
        .mounts
        .iter()
        .flat_map(|mount| [mount.host.as_str(), mount.dest.as_str()]);
    let sentinel_strings = SENTINEL_VALUES
        .iter()
        .map(|(_, value)| (*value).to_string())
        .chain(SENTINEL_VALUES.iter().map(|(key, _)| (*key).to_string()))
        .chain(
            sentinel_home_files(&home)
                .into_iter()
                .map(|path| path.to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>();

    assert_no_sentinel(argv, &sentinel_strings, "autonomous argv");
    assert_no_sentinel(env_pairs, &sentinel_strings, "autonomous environment");
    assert_no_sentinel(mounts, &sentinel_strings, "autonomous mount plan");
}

#[test]
fn interactive_launch_keeps_credential_sentinels_as_positive_control() {
    let state = crate::testenv::IsolatedState::new();
    let home = state.root().to_path_buf();
    let worktree = home.join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    for file in sentinel_home_files(&home) {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, b"redteam file sentinel").unwrap();
    }
    let spec = redteam_spec(&home, &worktree);

    for (key, value) in SENTINEL_VALUES {
        assert!(
            spec.env
                .iter()
                .any(|(candidate, actual)| candidate == key && actual == value),
            "interactive environment lost {key}"
        );
    }
    for path in sentinel_home_files(&home) {
        let path = path.to_string_lossy();
        assert!(
            spec.mounts
                .iter()
                .any(|mount| mount.host.as_str() == path.as_ref() && mount.dest == path.as_ref()),
            "interactive mount plan lost {path}"
        );
    }

    let argv = enter_argv(&spec, "claude").expect("interactive launch argv");
    for path in sentinel_home_files(&home) {
        let path = path.to_string_lossy();
        assert!(
            argv.iter()
                .any(|argument| argument.as_str() == path.as_ref()),
            "interactive argv lost mount {path}: {argv:?}"
        );
    }
}
