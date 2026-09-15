//! Real SQLite capture and shipping composition; no launch/provider surrogate.
use super::*;
use crate::config::{EnvConfig, EnvSshConfig, PlacementMode, ProfileConfig, RemoteTransport};
use crate::config_automations::{AutomationActionConfig, AutomationRuleConfig};
use crate::config_pipeline::PipelineStage;
use crate::config_resolve::{Approvals, resolve_environment};
use crate::config_validate::semantic_observation;
use crate::db::Db;
use crate::host_config::{HostConfig, HostReach};
use crate::placement::Placement;
use crate::remote::GitLoc;

const NAME: &str = "checked-fixture";

fn config() -> Config {
    // Defaults may consult path environment; no effective loader/normalizer is
    // invoked. All actual resolver reads below use an explicit owned directory.
    Config::default()
}

fn ssh(target: &str, port: u16) -> HostConfig {
    HostConfig {
        reach: HostReach::Ssh,
        ssh: EnvSshConfig {
            host: target.into(),
            port,
            transport: RemoteTransport::Ssh,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn store_with(host: &HostConfig) -> Db {
    let db = Db::open_memory().unwrap();
    db.put_host_def(NAME, host, 1).unwrap();
    db
}

fn empty_snapshot() -> HostDefinitionsSnapshot {
    Db::open_memory().unwrap().host_defs_checked().unwrap()
}

fn compose(cfg: Config) -> Result<HostComposedConfig, HostCompositionError> {
    compose_host_definitions_checked(&cfg, &empty_snapshot())
}

fn resolve(cfg: &Config) -> crate::env::Environment {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    let loc = GitLoc::Local(root.to_path_buf());
    let (environment, state) =
        resolve_environment(cfg, root, &loc, root, Some(NAME), &Approvals::deny_all());
    assert!(state.events.is_empty());
    assert!(state.pending.is_empty());
    assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    directory.close().expect("owned resolver directory cleanup");
    environment
}

fn assert_target(cfg: &Config, target: &str, port: u16) {
    let environment = resolve(cfg);
    assert!(!environment.unresolved_selection);
    let Placement::Ssh(placement) = environment.placement else {
        panic!("expected captured SSH placement");
    };
    assert_eq!(placement.host, target);
    assert_eq!(placement.port, port);
}

#[test]
fn actual_capture_contributes_unshadowed_host_and_preserves_declarative_winner() {
    let persisted = ssh("persisted@unused.invalid", 2202);
    let db = store_with(&persisted);
    let changes = db.conn().total_changes();
    let cfg = capture_and_compose_hosts_checked(&config(), &db).unwrap();
    assert_target(cfg.config(), &persisted.ssh.host, persisted.ssh.port);

    let mut declared = config();
    declared.host.insert(
        NAME.into(),
        HostConfig {
            reach: HostReach::Local,
            ..Default::default()
        },
    );
    let cfg = capture_and_compose_hosts_checked(&declared, &db).unwrap();
    assert_eq!(cfg.config().host[NAME].reach, HostReach::Local);
    assert_eq!(cfg.config().env[NAME].placement, PlacementMode::Local);
    let resolved = resolve(cfg.config());
    assert!(!resolved.unresolved_selection);
    assert!(matches!(resolved.placement, Placement::Local));

    let mut declared = config();
    let effective = ssh("declared@unused.invalid", 2201);
    declared.host.insert(NAME.into(), effective.clone());
    let cfg = capture_and_compose_hosts_checked(&declared, &db).unwrap();
    assert_eq!(cfg.config().env[NAME].ssh.host, effective.ssh.host);
    assert_target(cfg.config(), &effective.ssh.host, effective.ssh.port);
    assert_eq!(db.conn().total_changes(), changes);
    assert!(db.conn().is_autocommit());
}

#[test]
fn explicit_environment_and_undefined_host_fallback_keep_existing_semantics() {
    let db = store_with(&ssh("persisted@unused.invalid", 2202));
    let mut declared = config();
    declared.env.insert(
        NAME.into(),
        EnvConfig {
            placement: PlacementMode::Ssh,
            host: "undefined-reference".into(),
            ssh: EnvSshConfig {
                host: "inline@unused.invalid".into(),
                port: 2203,
                transport: RemoteTransport::Ssh,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let before = serde_json::to_value(&declared.env[NAME]).unwrap();
    let cfg = capture_and_compose_hosts_checked(&declared, &db).unwrap();
    assert!(serde_json::to_value(&cfg.config().env[NAME]).unwrap() == before);
    assert_target(cfg.config(), "inline@unused.invalid", 2203);
}

#[test]
fn malformed_shadowed_source_refuses_before_composition_or_semantic_entry() {
    for (raw, expected) in [
        ("{", HostDefinitionReadError::InvalidJson),
        (
            r#"{"reach":"typo"}"#,
            HostDefinitionReadError::InvalidDefinition,
        ),
        (
            r#"{"reach":"local","unknown":"secret-canary"}"#,
            HostDefinitionReadError::InvalidDefinition,
        ),
        (
            r#"{"reach":"local","reach":"ssh"}"#,
            HostDefinitionReadError::DuplicateJsonKey,
        ),
    ] {
        let db = store_with(&ssh("persisted@unused.invalid", 22));
        db.conn()
            .execute("UPDATE hosts SET config_json=?1", [raw])
            .unwrap();
        let mut declared = config();
        declared.host.insert(
            NAME.into(),
            HostConfig {
                reach: HostReach::Local,
                ..Default::default()
            },
        );
        // This independent invalid input would give Bounds if composition ran
        // first. The actual wrapper must report the raw source failure instead.
        declared.branch_prefix = "x".repeat(MAX_STRING_BYTES + 1);
        let changes = db.conn().total_changes();
        let (result, events) =
            semantic_observation::capture(|| capture_and_compose_hosts_checked(&declared, &db));
        assert_eq!(result.unwrap_err(), HostCompositionError::Source(expected));
        assert!(events.is_empty());
        assert_eq!(db.conn().total_changes(), changes);
        assert!(db.conn().is_autocommit());
        let retained: String = db
            .conn()
            .query_row("SELECT config_json FROM hosts", [], |row| row.get(0))
            .unwrap();
        assert!(retained == raw);
    }
}

#[test]
fn captured_revision_is_immutable_and_new_capture_refuses_new_corruption() {
    let db = store_with(&ssh("captured@unused.invalid", 2204));
    let snapshot = db.host_defs_checked().unwrap();
    db.conn()
        .execute("UPDATE hosts SET config_json='{'", [])
        .unwrap();
    let cfg = compose_host_definitions_checked(&config(), &snapshot).unwrap();
    assert_target(cfg.config(), "captured@unused.invalid", 2204);
    assert_eq!(
        capture_and_compose_hosts_checked(&config(), &db).unwrap_err(),
        HostCompositionError::Source(HostDefinitionReadError::InvalidJson)
    );
}

#[test]
fn final_semantics_schema_and_debug_are_checked_without_echoing_inputs() {
    let mut valid = config();
    valid.branch_prefix = "private-success-canary".into();
    let result = compose(valid).unwrap();
    assert_eq!(format!("{result:?}"), "HostComposedConfig([redacted])");
    let mut invalid = config();
    invalid.diagnostics.crash_sink = "private-error-canary".into();
    let (result, events) = semantic_observation::capture(|| compose(invalid));
    assert_eq!(events, ["config_clone", "semantic_start"]);
    let error = result.unwrap_err();
    assert_eq!(error, HostCompositionError::InvalidFinalConfig);
    assert!(!format!("{error:?} {error}").contains("private-error-canary"));

    let mut invalid = config();
    invalid.weather.hard_expiry_secs = invalid.weather.stale_after_secs;
    assert_eq!(
        compose(invalid).unwrap_err(),
        HostCompositionError::InvalidFinalConfig
    );
    let mut invalid = config();
    invalid.pr.ttl_secs = u64::MAX;
    let (result, events) = semantic_observation::capture(|| compose(invalid));
    assert_eq!(
        result.unwrap_err(),
        HostCompositionError::InvalidFinalConfig
    );
    assert_eq!(
        events,
        ["config_clone"],
        "schema refusal precedes semantic validation"
    );
}

#[test]
fn disabled_sections_and_non_pane_hosts_are_not_new_global_restrictions() {
    let mut cfg = config();
    cfg.model_proxy.enabled = false;
    cfg.model_proxy.listen = "invalid-but-disabled".into();
    assert!(compose(cfg).is_ok());
    for reach in [HostReach::Iroh, HostReach::Cloud] {
        let mut host = HostConfig {
            reach,
            ..Default::default()
        };
        host.iroh.ticket = "file:/never-open-this-private-test-reference".into();
        host.iroh.user = "fixture".into();
        host.cloud.provider = "fixture".into();
        let db = store_with(&host);
        let result = capture_and_compose_hosts_checked(&config(), &db).unwrap();
        assert_eq!(result.config().host[NAME].reach, reach);
        assert!(!result.config().env.contains_key(NAME));
        assert_eq!(result.config().host[NAME].iroh.ticket, host.iroh.ticket);
    }
    // Existing document validation does not require an SSH target. Binding
    // resolution handles unconfigured reach separately; do not change that here.
    let db = store_with(&ssh("", 22));
    assert!(capture_and_compose_hosts_checked(&config(), &db).is_ok());
}

fn stage(index: usize, next: Option<String>) -> PipelineStage {
    PipelineStage {
        name: format!("stage-{index}"),
        agent: "claude".into(),
        prompt:
            "Complete {row}, commit {artifact}, then run thegn dispatch report {row} --text done"
                .into(),
        next,
        ..Default::default()
    }
}

fn chain(count: usize, cycle: bool) -> Config {
    let mut cfg = config();
    cfg.pipeline.stages = (0..count)
        .map(|index| {
            let next = if index + 1 < count {
                Some(format!("stage-{}", index + 1))
            } else if cycle {
                Some("stage-0".into())
            } else {
                None
            };
            stage(index, next)
        })
        .collect();
    cfg
}

fn rule(index: usize) -> AutomationRuleConfig {
    AutomationRuleConfig {
        name: format!("rule-{index}"),
        when: "agent_needs_you".into(),
        then: AutomationActionConfig {
            cap: "notify.push".into(),
            body: Some("{message}".into()),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn profiles(cfg: &mut Config, count: usize) {
    for index in 0..count {
        let mut profile = ProfileConfig::default();
        profile.automations.enabled = Some(false);
        cfg.profiles.insert(format!("profile-{index:02}"), profile);
    }
}

fn assert_bounds_before_semantics(cfg: Config) {
    let (result, events) = semantic_observation::capture(|| compose(cfg));
    assert_eq!(result.unwrap_err(), HostCompositionError::Bounds);
    assert!(
        events.is_empty(),
        "no semantic entry or effective profile clone"
    );
}

#[test]
fn pipeline_limit_admits_actual_chain_and_refuses_cycle_or_oversized_graph() {
    assert!(compose(chain(MAX_PIPELINE_STAGES, false)).is_ok());
    assert_eq!(
        compose(chain(MAX_PIPELINE_STAGES, true)).unwrap_err(),
        HostCompositionError::InvalidFinalConfig
    );
    assert_bounds_before_semantics(chain(MAX_PIPELINE_STAGES + 1, false));
}

#[test]
fn profile_rule_and_effective_visit_limits_are_inclusive_before_any_clone() {
    let mut cfg = config();
    profiles(&mut cfg, MAX_PROFILES);
    let (result, events) = semantic_observation::capture(|| compose(cfg.clone()));
    assert!(result.is_ok());
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "profile_clone")
            .count(),
        MAX_PROFILES
    );
    profiles(&mut cfg, MAX_PROFILES + 1);
    assert_bounds_before_semantics(cfg);

    let mut cfg = config();
    cfg.automations.rules = (0..MAX_AUTOMATION_RULES).map(rule).collect();
    assert!(compose(cfg.clone()).is_ok());
    profiles(&mut cfg, 15); // base256 +15 inherited256 =4096 real visits
    assert!(compose(cfg.clone()).is_ok());
    profiles(&mut cfg, 16);
    assert_bounds_before_semantics(cfg);

    let mut cfg = config();
    cfg.automations.rules = (0..=MAX_AUTOMATION_RULES).map(rule).collect();
    assert_bounds_before_semantics(cfg);
    let mut cfg = config();
    profiles(&mut cfg, 1);
    cfg.profiles
        .get_mut("profile-00")
        .unwrap()
        .automations
        .rules = Some((0..MAX_AUTOMATION_RULES).map(rule).collect());
    assert!(compose(cfg.clone()).is_ok());
    cfg.profiles
        .get_mut("profile-00")
        .unwrap()
        .automations
        .rules
        .as_mut()
        .unwrap()
        .push(rule(MAX_AUTOMATION_RULES));
    assert_bounds_before_semantics(cfg);
}

#[test]
fn replacement_counts_effective_rules_but_still_charges_original_clone_bytes() {
    let mut cfg = config();
    cfg.automations.rules = (0..MAX_AUTOMATION_RULES).map(rule).collect();
    profiles(&mut cfg, MAX_PROFILES);
    for profile in cfg.profiles.values_mut() {
        profile.automations.rules = Some(Vec::new());
    }
    // Replacement yields only base256 visits, not65*256. Every base clone still
    // happens in the validator, even though apply immediately replaces rules.
    assert!(compose(cfg.clone()).is_ok());
    let base = serde_json::to_vec(&cfg.automations).unwrap().len();
    let overlays: usize = cfg
        .profiles
        .values()
        .map(|profile| serde_json::to_vec(&profile.automations).unwrap().len())
        .sum();
    let expected_bytes = base * (MAX_PROFILES + 1) + overlays;
    assert!(check_automation_work(&cfg, MAX_AUTOMATION_RULES, expected_bytes).is_ok());
    assert_eq!(
        check_automation_work(&cfg, MAX_AUTOMATION_RULES - 1, expected_bytes),
        Err(HostCompositionError::Bounds)
    );
    assert_eq!(
        check_automation_work(&cfg, MAX_AUTOMATION_RULES, expected_bytes - 1),
        Err(HostCompositionError::Bounds)
    );

    // One otherwise valid large base rule repeated64times is small serialized
    // input but would exceed the public work-byte cap despite only one visit.
    cfg.automations.rules = vec![rule(0)];
    cfg.automations.rules[0].then.body = Some("x".repeat(16 * 1024));
    for index in 1..32 {
        cfg.automations.rules.push(rule(index));
    }
    for rule in &mut cfg.automations.rules {
        rule.then.body = Some("x".repeat(16 * 1024));
    }
    assert_bounds_before_semantics(cfg);
}

#[test]
fn first_error_stops_before_later_profile_clone_and_legacy_order_is_retained() {
    let raw = "[profiles.alpha.automations]\nmax_concurrent=0\n[profiles.beta.automations]\nqueue_capacity=0\n";
    let cfg: Config = toml::from_str(raw).unwrap();
    let (first, first_events) = semantic_observation::capture(|| {
        config_validate::typed_semantic_errors(&cfg, SemanticMode::StopOnError)
    });
    let (all, all_events) = semantic_observation::capture(|| {
        config_validate::typed_semantic_errors(&cfg, SemanticMode::AllDiagnostics)
    });
    assert_eq!(first_events, ["semantic_start", "profile_clone"]);
    assert_eq!(
        all_events,
        ["semantic_start", "profile_clone", "profile_clone"]
    );
    assert_eq!(first.len(), 1);
    assert_eq!(all.len(), 2);
    assert_eq!(first[0], all[0]);
    assert!(all[0].starts_with("profiles.alpha:"));
    assert!(all[1].starts_with("profiles.beta:"));
    assert_eq!(config_validate::validate_str(raw), all);

    let mut early = cfg;
    early.weather.hard_expiry_secs = early.weather.stale_after_secs;
    let (result, events) = semantic_observation::capture(|| compose(early));
    assert_eq!(
        result.unwrap_err(),
        HostCompositionError::InvalidFinalConfig
    );
    assert_eq!(events, ["config_clone", "semantic_start"]);
}

#[path = "host_config_checked_bounds_tests.rs"]
mod bounds;

#[path = "host_config_checked_recursive_tests.rs"]
mod recursive;
