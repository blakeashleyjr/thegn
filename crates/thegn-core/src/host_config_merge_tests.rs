//! Host-definition precedence through actual environment resolution. These
//! fixtures never open a state DB or invoke a transport/provider.

use super::*;
use crate::config_resolve::{Approvals, resolve_environment};
use crate::placement::Placement;
use crate::remote::GitLoc;

const NAME: &str = "precedence-fixture";
const REACHES: [HostReach; 4] = [
    HostReach::Local,
    HostReach::Ssh,
    HostReach::Iroh,
    HostReach::Cloud,
];

fn host(reach: HostReach, declared: bool) -> HostConfig {
    let source = if declared { "declared" } else { "database" };
    HostConfig {
        reach,
        image: format!("{source}:fixture"),
        ssh: EnvSshConfig {
            host: format!("{source}@{source}.invalid"),
            port: if declared { 2201 } else { 2202 },
            transport: if declared {
                RemoteTransport::Ssh
            } else {
                RemoteTransport::Mosh
            },
            forward_agent: declared,
            ssh_config: format!("/fixture/{source}/ssh-config"),
            jump_host: format!("{source}-jump.invalid"),
            identity: format!("/fixture/{source}/identity"),
            extra_args: vec![
                "-o".into(),
                format!("ConnectTimeout={}", if declared { 11 } else { 22 }),
            ],
        },
        iroh: HostIrohConfig {
            ticket: format!("{source}-ticket"),
            user: source.into(),
            ssh_port: 2223,
        },
        cloud: HostCloudConfig {
            provider: format!("{source}-provider"),
            api_base: format!("https://{source}.invalid"),
            ..HostCloudConfig::default()
        },
        ..HostConfig::default()
    }
}

fn assert_ssh(placement: &Placement, expected: &EnvSshConfig) {
    let Placement::Ssh(actual) = placement else {
        panic!("expected SSH placement, got {placement:?}");
    };
    assert_eq!(actual.host, expected.host);
    assert_eq!(actual.port, expected.port);
    assert_eq!(actual.forward_agent, expected.forward_agent);
    assert_eq!(
        actual.kind,
        match expected.transport {
            RemoteTransport::Ssh => TransportKind::Ssh,
            RemoteTransport::Mosh => TransportKind::Mosh,
        }
    );
    assert_eq!(
        actual.ssh_config.as_deref(),
        Some(expected.ssh_config.as_str())
    );
    assert_eq!(
        actual.jump_host.as_deref(),
        Some(expected.jump_host.as_str())
    );
    assert_eq!(actual.identity.as_deref(), Some(expected.identity.as_str()));
    assert_eq!(actual.extra_args, expected.extra_args);
}

fn resolve(cfg: &Config) -> crate::env::Environment {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    // An explicit local GitLoc avoids implicit DB discovery. root == worktree
    // skips the main-branch Git probe even with main_env configured. The only
    // repo-overlay reads see this owned empty directory.
    let loc = GitLoc::Local(root.to_path_buf());
    let (environment, resolved) =
        resolve_environment(cfg, root, &loc, root, Some(NAME), &Approvals::deny_all());
    assert!(resolved.events.is_empty());
    assert!(resolved.pending.is_empty());
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    directory
        .close()
        .expect("owned empty resolution fixture cleanup");
    environment
}

fn config() -> Config {
    let mut cfg = Config::default();
    cfg.sandbox.main_env = "unused-primary-default".into();
    cfg
}

fn assert_synthesized(cfg: &Config, expected: &HostConfig) {
    assert_eq!(
        serde_json::to_value(&cfg.host[NAME]).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    let resolved = resolve(cfg);
    assert_eq!(resolved.name, NAME);
    match expected.reach {
        HostReach::Local | HostReach::Ssh => {
            let env = &cfg.env[NAME];
            assert_eq!(env.host, NAME);
            assert_eq!(
                serde_json::to_value(&env.ssh).unwrap(),
                serde_json::to_value(&expected.ssh).unwrap()
            );
            assert!(!resolved.unresolved_selection);
            if expected.reach == HostReach::Local {
                assert_eq!(env.placement, PlacementMode::Local);
                assert!(matches!(resolved.placement, Placement::Local));
            } else {
                assert_eq!(env.placement, PlacementMode::Ssh);
                assert_ssh(&resolved.placement, &expected.ssh);
            }
        }
        HostReach::Iroh | HostReach::Cloud => {
            // These reaches intentionally have no synthesized pane env. A
            // selected missing name must remain diagnosed, never inherit the
            // losing DB definition's selectable SSH/local destination.
            assert!(!cfg.env.contains_key(NAME));
            assert!(resolved.unresolved_selection);
            assert!(matches!(resolved.placement, Placement::Local));
        }
    }
}

#[test]
fn declared_local_shadows_db_ssh_in_resolved_environment() {
    let mut cfg = config();
    let declared = host(HostReach::Local, true);
    cfg.host.insert(NAME.into(), declared.clone());
    let captured_defs = vec![(NAME.into(), host(HostReach::Ssh, false))];
    merge_host_defs(&mut cfg, &captured_defs);
    assert_synthesized(&cfg, &declared);
}

#[test]
fn declared_ssh_shadows_db_reaches_and_connection_settings() {
    for reach in REACHES {
        let mut cfg = config();
        let declared = host(HostReach::Ssh, true);
        cfg.host.insert(NAME.into(), declared.clone());
        let captured_defs = vec![(NAME.into(), host(reach, false))];
        merge_host_defs(&mut cfg, &captured_defs);
        assert_synthesized(&cfg, &declared);
    }
}

#[test]
fn declared_nonpane_reaches_never_synthesize_losing_db_environments() {
    for winning_reach in [HostReach::Iroh, HostReach::Cloud] {
        for losing_reach in REACHES {
            let mut cfg = config();
            let declared = host(winning_reach, true);
            cfg.host.insert(NAME.into(), declared.clone());
            let captured_defs = vec![(NAME.into(), host(losing_reach, false))];
            merge_host_defs(&mut cfg, &captured_defs);
            assert_synthesized(&cfg, &declared);
        }
    }
}

#[test]
fn explicit_environment_survives_host_definition_merge_and_resolution() {
    for placement in [PlacementMode::Local, PlacementMode::Ssh] {
        let mut cfg = config();
        let explicit = EnvConfig {
            placement,
            // No host pin: this explicit environment intentionally chooses its
            // own destination independently of the same-name host catalog.
            ssh: host(HostReach::Ssh, true).ssh,
            ..EnvConfig::default()
        };
        let before = serde_json::to_value(&explicit).unwrap();
        cfg.env.insert(NAME.into(), explicit.clone());
        let captured_defs = vec![(NAME.into(), host(HostReach::Ssh, false))];
        merge_host_defs(&mut cfg, &captured_defs);
        assert_eq!(serde_json::to_value(&cfg.env[NAME]).unwrap(), before);
        assert_eq!(cfg.host[NAME].ssh.host, "database@database.invalid");
        let resolved = resolve(&cfg);
        assert_eq!(resolved.name, NAME);
        assert!(!resolved.unresolved_selection);
        match placement {
            PlacementMode::Local => assert!(matches!(resolved.placement, Placement::Local)),
            PlacementMode::Ssh => assert_ssh(&resolved.placement, &explicit.ssh),
            _ => unreachable!("fixture only selects local or SSH"),
        }
    }
}

#[test]
fn unshadowed_db_definitions_resolve_only_supported_pane_reaches() {
    for reach in REACHES {
        let mut cfg = config();
        let database = host(reach, false);
        let captured_defs = vec![(NAME.into(), database.clone())];
        merge_host_defs(&mut cfg, &captured_defs);
        assert_synthesized(&cfg, &database);
    }
}
