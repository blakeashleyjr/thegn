use super::*;
use crate::config::{Config, EnvSource};
use crate::host_definition_snapshot::HostDefinitionsSnapshot;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

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

struct FlippingEnv {
    calls: Cell<usize>,
}

impl EnvSource for FlippingEnv {
    fn get(&self, _key: &str) -> Option<String> {
        let calls = self.calls.get();
        self.calls.set(calls + 1);
        (calls > 0).then(|| "appeared-later".to_string())
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

    let depth = format!(
        "value = {}0{}\n",
        "[".repeat(crate::config_budget::MAX_DEPTH + 1),
        "]".repeat(crate::config_budget::MAX_DEPTH + 1)
    );
    assert!(matches!(
        crate::config_budget::scan(depth.as_bytes()),
        Err(crate::config_budget::BudgetError::Depth)
    ));

    let mut string = String::from("value = \"\"\"\n");
    for _ in 0..(crate::config_budget::MAX_STRING_BYTES / 128 + 1) {
        string.push_str(&"x".repeat(128));
        string.push('\n');
    }
    string.push_str("\"\"\"\n");
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
        Err(crate::config_budget::BudgetError::Nodes)
    ));
}

#[test]
fn scanner_resets_statement_depth_and_counts_root_and_inline_members() {
    let deep_key = (0..=crate::config_budget::MAX_DEPTH)
        .map(|index| format!("k{index}"))
        .collect::<Vec<_>>()
        .join(".");
    let body = format!("first = 1\n{deep_key} = 2\n");
    assert!(matches!(
        crate::config_budget::scan(body.as_bytes()),
        Err(crate::config_budget::BudgetError::Depth)
    ));

    let quoted = "[\"a.b\".x]\nvalue = 1\n";
    assert!(crate::config_budget::scan(quoted.as_bytes()).is_ok());

    let nested = format!(
        "value = {}0{}\n",
        "[".repeat(crate::config_budget::MAX_DEPTH),
        "]".repeat(crate::config_budget::MAX_DEPTH)
    );
    assert!(crate::config_budget::scan(nested.as_bytes()).is_ok());
    assert!(crate::config_budget::scan(b"value = [[0]]\n").is_ok());

    let mut root = String::new();
    for index in 0..=crate::config_budget::MAX_MEMBERS {
        root.push_str(&format!("k{index} = 1\n"));
    }
    assert!(matches!(
        crate::config_budget::scan(root.as_bytes()),
        Err(crate::config_budget::BudgetError::Members)
    ));

    let mut table = String::from("[members]\n");
    for index in 0..=crate::config_budget::MAX_MEMBERS {
        table.push_str(&format!("k{index} = 1\n"));
    }
    assert!(matches!(
        crate::config_budget::scan(table.as_bytes()),
        Err(crate::config_budget::BudgetError::Members)
    ));

    let inline = "value = { first = 1, second = 2 }\n";
    assert!(crate::config_budget::scan(inline.as_bytes()).is_ok());
}

#[test]
fn scanner_accepts_physical_line_edge_inside_valid_multiline_string() {
    let exact = format!(
        "value = \"\"\"\n{}\n\"\"\"\n",
        "x".repeat(crate::config_budget::MAX_LINE_BYTES - 1)
    );
    assert!(crate::config_budget::scan(exact.as_bytes()).is_ok());
    let over = format!(
        "value = \"\"\"\n{}\n\"\"\"\n",
        "x".repeat(crate::config_budget::MAX_LINE_BYTES)
    );
    assert!(matches!(
        crate::config_budget::scan(over.as_bytes()),
        Err(crate::config_budget::BudgetError::LineBytes)
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
fn captured_environment_freezes_absent_values_as_well_as_present_values() {
    let source = FlippingEnv {
        calls: Cell::new(0),
    };
    let captured = CapturedEnv::new(&source);
    assert_eq!(captured.get("THEGN_BRANCH_PREFIX"), None);
    assert_eq!(captured.get("THEGN_BRANCH_PREFIX"), None);
    assert_eq!(source.calls.get(), 1);
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

    for (key, value) in [
        ("THEGN_SANDBOX_ENABLED", "not-a-bool"),
        ("THEGN_PICKER", "not-a-picker"),
        ("THEGN_REPO_SCAN_DEPTH", "not-a-number"),
    ] {
        let mut bad_env = TestEnv::default();
        bad_env.0.insert(key.into(), value.into());
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
    }

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

    let deep_cli = vec![format!(
        "{}.branch_prefix=value",
        (0..=crate::config_budget::MAX_DEPTH)
            .map(|index| format!("segment{index}"))
            .collect::<Vec<_>>()
            .join(".")
    )];
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &env,
        overrides: &deep_cli,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::Oversized)));

    let too_many_cli =
        vec!["branch_prefix=safe/".to_string(); crate::config_budget::MAX_CLI_ENTRIES + 1];
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &env,
        overrides: &too_many_cli,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::Oversized)));

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
    let revision_debug = format!("{:?}", first.revision());
    assert!(!revision_debug.contains(&format!("{:?}", first.revision().digest)));
    assert!(!revision_debug.contains(&format!(
        "{:?}",
        first.revision().sources.base.content_digest
    )));
    let input_debug = format!(
        "{:?}",
        SourceInput::bytes("/private/supersecret/config.toml", false, b"supersecret")
    );
    assert!(!input_debug.contains("supersecret"));
    assert!(!input_debug.contains("/private/"));

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
fn rejected_candidate_does_not_install_process_global_remote_policy() {
    let before = crate::remote_tune::ssh_tune();
    let mut changed = before;
    changed.keepalive_interval_secs = before.keepalive_interval_secs.saturating_add(1);
    crate::remote_tune::set_ssh_tune(changed);

    let env = TestEnv::default();
    let overrides = Vec::new();
    let host_snapshot = hosts();
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes(
            "base",
            false,
            b"[remote]\nkeepalive_interval_secs = 9\n[serve]\ncors_origins = [\"*\"]\n",
        ),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::SemanticInvalid)));
    assert_eq!(crate::remote_tune::ssh_tune(), changed);

    let valid = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"[remote]\nkeepalive_interval_secs = 9\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .expect("pure admission of valid policy");
    assert_eq!(valid.config().remote.keepalive_interval_secs, 9);
    assert_eq!(crate::remote_tune::ssh_tune(), changed);
    crate::remote_tune::set_ssh_tune(before);
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

#[test]
fn store_default_reload_health_and_generation_exhaustion_are_explicit() {
    let store = AdmissionStore::default();
    assert_eq!(store.health(), StoreHealth::Empty);
    assert!(matches!(
        require_current(
            &ConfigRevision {
                generation: 0,
                digest: [0; 32],
                sources: SourceIdentities {
                    base: synthetic_identity("base", 0),
                    profile: None,
                    environment_digest: [0; 32],
                    overrides_digest: [0; 32],
                    host_digest: [0; 32],
                },
                normalization_version: NORMALIZATION_VERSION,
                host_schema: 68,
            },
            &store,
        ),
        Err(StaleConfigError {
            reason: StaleConfigReason::NoSnapshot,
            ..
        })
    ));

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
    .unwrap();
    let published = store.publish(first, 0).unwrap();
    assert_eq!(published.revision().generation, 1);

    let max_candidate = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"max/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .unwrap();
    let max_store = AdmissionStore {
        current: Mutex::new(Some(Arc::new(max_candidate.with_generation(u64::MAX)))),
        health: Mutex::new(StoreHealth::Healthy),
    };
    let next_candidate = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"next/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
    })
    .unwrap();
    assert!(matches!(
        max_store.publish(next_candidate, u64::MAX),
        Err(StaleConfigError {
            reason: StaleConfigReason::GenerationExhausted,
            ..
        })
    ));

    store.record_reload_failure();
    assert_eq!(store.health(), StoreHealth::ReloadFailed);
    assert_eq!(store.current().unwrap().revision().generation, 1);
    assert!(matches!(
        require_current(&published.revision(), &store),
        Err(StaleConfigError {
            reason: StaleConfigReason::ReloadFailed,
            ..
        })
    ));
}
