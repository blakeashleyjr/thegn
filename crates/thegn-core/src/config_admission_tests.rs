use super::*;
use crate::config::{Config, EnvSource};
use crate::host_definition_snapshot::HostDefinitionsSnapshot;
use std::cell::Cell;
use std::collections::BTreeMap;

#[derive(Default)]
struct TestEnv(BTreeMap<String, String>);

impl EnvSource for TestEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned().filter(|value| !value.is_empty())
    }
}

struct CountingEnv {
    calls: Cell<usize>,
}

impl EnvSource for CountingEnv {
    fn get(&self, key: &str) -> Option<String> {
        if key == "THEGN_BRANCH_PREFIX" {
            self.calls.set(self.calls.get() + 1);
            Some("captured/".into())
        } else {
            None
        }
    }
}

fn hosts() -> HostDefinitionsSnapshot {
    HostDefinitionsSnapshot::from_raw(68, Vec::new()).expect("empty host snapshot")
}

#[test]
fn bounded_reader_accepts_limit_and_rejects_limit_plus_one() {
    let mut exact = Vec::with_capacity(crate::config_budget::MAX_SOURCE_BYTES);
    while exact.len() + 65 <= crate::config_budget::MAX_SOURCE_BYTES {
        exact.extend_from_slice(
            b"# bounded comment padding................................................\n",
        );
    }
    exact.resize(crate::config_budget::MAX_SOURCE_BYTES, b' ');
    exact[crate::config_budget::MAX_SOURCE_BYTES - 1] = b'\n';
    assert!(crate::config_budget::scan(&exact).is_ok());

    let env = TestEnv::default();
    let overrides = Vec::new();
    let host_snapshot = hosts();
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, &exact),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(
        admitted.is_ok(),
        "exact source budget must admit: {admitted:?}"
    );

    let too_large = vec![b' '; crate::config_budget::MAX_SOURCE_BYTES + 1];
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, &too_large),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::Oversized)));
}

#[test]
fn scanner_ignores_structure_inside_all_toml_string_forms() {
    let body = b"basic = \"[[not-a-table]] { nested = true }\"\nliteral = '[this.is.not.depth]'\nmultiline_basic = \"\"\"{ [[ still text ]] }\"\"\"\nmultiline_literal = '''# not a comment\n[still text]'''\n";
    assert!(crate::config_budget::scan(body).is_ok());
}

#[test]
fn scanner_rejects_line_depth_node_member_string_and_work_overages() {
    assert!(matches!(
        crate::config_budget::scan(&vec![b'a'; crate::config_budget::MAX_LINE_BYTES + 1]),
        Err(crate::config_budget::BudgetError::LineBytes)
    ));

    let depth = format!("{}x = 1\n", "[".repeat(crate::config_budget::MAX_DEPTH + 1));
    assert!(matches!(
        crate::config_budget::scan(depth.as_bytes()),
        Err(crate::config_budget::BudgetError::Depth)
    ));

    let string = format!(
        "value = \"{}\"\n",
        "x".repeat(crate::config_budget::MAX_STRING_BYTES + 1)
    );
    assert!(matches!(
        crate::config_budget::scan(string.as_bytes()),
        Err(crate::config_budget::BudgetError::StringBytes)
    ));

    let mut members = String::from("values = [\n");
    for _ in 0..crate::config_budget::MAX_MEMBERS + 1 {
        members.push_str("1,\n");
    }
    members.push_str("]\n");
    assert!(matches!(
        crate::config_budget::scan(members.as_bytes()),
        Err(crate::config_budget::BudgetError::Members)
    ));

    let mut nodes = String::from("values = [\n");
    for index in 0..crate::config_budget::MAX_NODES {
        nodes.push_str("1,");
        if index % 100 == 0 {
            nodes.push('\n');
        }
    }
    nodes.push_str("]\n");
    assert!(matches!(
        crate::config_budget::scan(nodes.as_bytes()),
        Err(crate::config_budget::BudgetError::Work)
            | Err(crate::config_budget::BudgetError::Nodes)
    ));
}

#[test]
fn valid_composition_records_precedence_and_compatibility_trace() {
    let base = b"workspaces_dir = \"base/\"\nbranch_prefix = \"base/\"\n";
    let profile = b"projects_dir = \"profile/\"\nbranch_prefix = \"profile/\"\n";
    let mut env = TestEnv::default();
    env.0.insert("THEGN_BRANCH_PREFIX".into(), "env/".into());
    let overrides = vec!["branch_prefix=cli/".to_string()];
    let host_snapshot = hosts();
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, base),
        profile: Some(SourceInput::bytes("profile", true, profile)),
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .expect("valid complete composition");

    assert_eq!(admitted.config().branch_prefix, "cli/");
    assert_eq!(admitted.config().workspaces_dir, "profile/");
    assert_eq!(admitted.health(), AdmissionHealth::Healthy);
    assert_eq!(
        admitted.revision().normalization_version,
        NORMALIZATION_VERSION
    );
    assert!(
        admitted
            .trace()
            .entries()
            .iter()
            .any(|entry| entry.layer == LayerKind::Profile)
    );
    assert!(
        admitted
            .trace()
            .entries()
            .iter()
            .any(|entry| entry.layer == LayerKind::Environment)
    );
}

#[test]
fn environment_is_captured_once_and_not_reopened_during_composition() {
    let env = CountingEnv {
        calls: Cell::new(0),
    };
    let overrides = Vec::new();
    let host_snapshot = hosts();
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"base/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .expect("captured environment admits");
    assert_eq!(admitted.config().branch_prefix, "captured/");
    assert_eq!(env.calls.get(), 1);
}

#[test]
fn only_absent_implicit_default_can_select_first_run_defaults() {
    let env = TestEnv::default();
    let overrides = Vec::new();
    let host_snapshot = hosts();
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::absent("default", false),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .expect("implicit absent default is an explicit first-run outcome");
    assert_eq!(admitted.health(), AdmissionHealth::FirstRunDefault);

    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::absent("explicit", true),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(
        result,
        Err(ConfigAdmissionError::ExplicitPathMissing)
    ));
}

#[test]
fn invalid_profile_env_cli_schema_and_semantics_never_publish() {
    let env = TestEnv::default();
    let overrides = Vec::new();
    let host_snapshot = hosts();

    let profile_missing = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: Some(SourceInput::absent("profile", true)),
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(
        profile_missing,
        Err(ConfigAdmissionError::ProfileInvalid)
    ));

    for content in [
        SourceContent::Failure(SourceFailure::Unreadable),
        SourceContent::Failure(SourceFailure::InvalidUtf8),
        SourceContent::Bytes(b"= = broken\n"),
        SourceContent::Bytes(b"[sandbox]\nenabled = \"not-bool\"\n"),
    ] {
        let result = admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
            profile: Some(SourceInput {
                identity: "profile",
                explicit: true,
                content,
            }),
            env: &env,
            overrides: &overrides,
            hosts: &host_snapshot,
        });
        assert!(matches!(result, Err(ConfigAdmissionError::ProfileInvalid)));
    }

    let mut bad_env = TestEnv::default();
    bad_env
        .0
        .insert("THEGN_SANDBOX_ENABLED".into(), "not-a-bool".into());
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &bad_env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(
        result,
        Err(ConfigAdmissionError::EnvironmentInvalid)
    ));

    let bad_cli = vec!["picker=not-a-picker".to_string()];
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &env,
        overrides: &bad_cli,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::CliInvalid)));

    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"typoed_key = true\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::SchemaInvalid)));

    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"[serve]\ncors_origins = [\"*\"]\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::SemanticInvalid)));
}

#[test]
fn revision_changes_for_sources_and_host_snapshot_without_exposing_secret_values() {
    let env = TestEnv::default();
    let overrides = Vec::new();
    let first_hosts = hosts();
    let first = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes(
            "/private/supersecret/config.toml",
            false,
            b"[issues.linear]\napi_key = \"supersecret\"\n",
        ),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &first_hosts,
    })
    .expect("secret-bearing valid config still admits");
    let debug = format!("{first:?}");
    assert!(!debug.contains("supersecret"));
    assert!(!debug.contains("/private/"));

    let second_hosts = HostDefinitionsSnapshot::from_raw(
        68,
        vec![("workstation".into(), r#"{"reach":"local"}"#.into())],
    )
    .unwrap();
    let second = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"changed/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &second_hosts,
    })
    .expect("changed source and host snapshot admit");
    assert_ne!(first.revision().digest, second.revision().digest);
    assert_ne!(
        first.revision().sources.host_digest,
        second.revision().sources.host_digest
    );
}

#[test]
fn failed_admission_never_constructs_an_admitted_config() {
    let env = TestEnv::default();
    let overrides = Vec::new();
    let host_snapshot = hosts();
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"= = broken\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::ParseInvalid)));
}

#[test]
fn revision_guard_fences_publication_generation() {
    let env = TestEnv::default();
    let overrides = Vec::new();
    let host_snapshot = hosts();
    let first = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"one/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .expect("first candidate");
    let store = AdmissionStore::new(first);
    let revision = store.current().unwrap().revision();
    assert!(require_current(&revision, &store).is_ok());

    let second = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"two/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .expect("second candidate");
    let published = store.publish(second, revision.generation).unwrap();
    assert_eq!(published.revision().generation, revision.generation + 1);
    assert!(require_current(&revision, &store).is_err());
}
