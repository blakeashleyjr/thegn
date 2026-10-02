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

fn path_context() -> PathExpansionContext {
    PathExpansionContext::from_home(std::path::PathBuf::from("/captured/home"))
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
        paths: &path_context(),
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
        paths: &path_context(),
    });
    assert!(matches!(result, Err(ConfigAdmissionError::Oversized)));
}

#[test]
fn scanner_ignores_structure_inside_all_toml_string_forms() {
    let body = b"basic = \"[[not-a-table]] { nested = true }\"\nliteral = '[this.is.not.depth]'\nmultiline_basic = \"\"\"{ [[ still text ]] }\"\"\"\nmultiline_literal = '''# not a comment\n[still text]'''\n";
    assert!(crate::config_budget::scan(body).is_ok());
}

#[test]
fn scanner_accepts_literal_quotes_before_multiline_terminators() {
    for quote in ['"', '\''] {
        for trailing_quotes in 3..=5 {
            let source = format!(
                "x = {}value{}\nnext = 1\n",
                quote.to_string().repeat(3),
                quote.to_string().repeat(trailing_quotes),
            );
            assert!(
                crate::config_budget::scan(source.as_bytes()).is_ok(),
                "{source:?}"
            );
            let parsed: toml::Value = source.parse().unwrap();
            assert_eq!(parsed["x"].as_str().unwrap().len(), 5 + trailing_quotes - 3);
        }
    }
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

    let mut nodes = String::new();
    for array in 0..4 {
        nodes.push_str(&format!("values{array} = [\n"));
        for index in 0..crate::config_budget::MAX_MEMBERS {
            nodes.push_str("1,");
            if index % 100 == 0 {
                nodes.push('\n');
            }
        }
        nodes.push_str("]\n");
    }
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
        "[".repeat(crate::config_budget::MAX_DEPTH - 1),
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
        "x".repeat(crate::config_budget::MAX_LINE_BYTES)
    );
    assert!(crate::config_budget::scan(exact.as_bytes()).is_ok());
    let over = format!(
        "value = \"\"\"\n{}\n\"\"\"\n",
        "x".repeat(crate::config_budget::MAX_LINE_BYTES + 1)
    );
    assert!(matches!(
        crate::config_budget::scan(over.as_bytes()),
        Err(crate::config_budget::BudgetError::LineBytes)
    ));
}

#[test]
fn scanner_counts_multiline_delimiters_as_physical_line_bytes() {
    let exact = format!(
        "x = \"\"\"{}\"\"\"\n",
        "x".repeat(crate::config_budget::MAX_LINE_BYTES - 10)
    );
    assert!(crate::config_budget::scan(exact.as_bytes()).is_ok());

    let over = format!(
        "x = \"\"\"{}\"\"\"\n",
        "x".repeat(crate::config_budget::MAX_LINE_BYTES - 9)
    );
    assert_eq!(
        crate::config_budget::scan(over.as_bytes()),
        Err(crate::config_budget::BudgetError::LineBytes)
    );
}

#[test]
fn scanner_accumulates_table_path_dotted_key_and_inline_depth() {
    let table_path = (0..32).map(|_| "table").collect::<Vec<_>>().join(".");
    let key = (0..33).map(|_| "key").collect::<Vec<_>>().join(".");
    assert_eq!(
        crate::config_budget::scan(format!("[{table_path}]\n{key} = 1\n").as_bytes()),
        Err(crate::config_budget::BudgetError::Depth)
    );

    let key_at_limit = (0..32).map(|_| "key").collect::<Vec<_>>().join(".");
    assert!(
        crate::config_budget::scan(format!("[{table_path}]\n{key_at_limit} = 1\n").as_bytes())
            .is_ok()
    );

    let quoted = (0..32).map(|_| "key").collect::<Vec<_>>().join(".");
    assert!(
        crate::config_budget::scan(
            format!("value = {{ \"a.b\" = {{ {quoted} = 1 }} }}\n").as_bytes()
        )
        .is_ok()
    );

    let sibling = format!(
        "[{}a]\nx = 1\n[{}b]\ny = 1\n",
        "p.".repeat(40),
        "q.".repeat(40)
    );
    assert!(crate::config_budget::scan(sibling.as_bytes()).is_ok());

    let table_and_arrays = format!(
        "[{}z]\nx = {}1{}\n",
        "a.".repeat(62),
        "[".repeat(10),
        "]".repeat(10)
    );
    assert_eq!(
        crate::config_budget::scan(table_and_arrays.as_bytes()),
        Err(crate::config_budget::BudgetError::Depth)
    );

    let outer_and_inline = format!("{}v = {{ {}w = 1 }}\n", "a.".repeat(40), "b.".repeat(40));
    assert_eq!(
        crate::config_budget::scan(outer_and_inline.as_bytes()),
        Err(crate::config_budget::BudgetError::Depth)
    );
}

#[test]
fn scanner_counts_array_values_not_commas_or_trailing_delimiters() {
    let exact_no_trailing = format!(
        "values = [\n{}\n]\n",
        std::iter::repeat_n("1", crate::config_budget::MAX_MEMBERS)
            .collect::<Vec<_>>()
            .join(",\n")
    );
    assert!(crate::config_budget::scan(exact_no_trailing.as_bytes()).is_ok());

    let exact_trailing = format!(
        "values = [\n{}\n]\n",
        "1,\n".repeat(crate::config_budget::MAX_MEMBERS)
    );
    assert!(crate::config_budget::scan(exact_trailing.as_bytes()).is_ok());

    let over = format!(
        "values = [\n{}\n]\n",
        std::iter::repeat_n("1", crate::config_budget::MAX_MEMBERS + 1)
            .collect::<Vec<_>>()
            .join(",\n")
    );
    assert_eq!(
        crate::config_budget::scan(over.as_bytes()),
        Err(crate::config_budget::BudgetError::Members)
    );
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
        paths: &path_context(),
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
fn captured_path_context_is_part_of_deterministic_admission_revision() {
    let env = TestEnv::default();
    let overrides = Vec::new();
    let host_snapshot = hosts();
    let first_paths = path_context();
    let first = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"workspaces_dir = \"~/workspaces\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &first_paths,
    })
    .expect("captured paths admit");
    let second = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"workspaces_dir = \"~/workspaces\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &first_paths,
    })
    .expect("same captured paths remain deterministic");
    assert_eq!(first.revision().digest, second.revision().digest);
    assert_eq!(first.config().workspaces_dir, "/captured/home/workspaces");

    let other_paths = PathExpansionContext::from_home(std::path::PathBuf::from("/other/home"));
    let other = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"workspaces_dir = \"~/workspaces\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &other_paths,
    })
    .expect("different captured path context admits");
    assert_ne!(first.revision().digest, other.revision().digest);
}

#[test]
fn blank_default_folder_override_is_semantically_invalid() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    for value in ["default_folder=''", "default_folder='   '"] {
        let overrides = vec![value.to_string()];
        let result = admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
            profile: None,
            env: &env,
            overrides: &overrides,
            hosts: &host_snapshot,
            paths: &path_context(),
        });
        assert!(
            matches!(&result, Err(ConfigAdmissionError::SemanticInvalid)),
            "override {value}: {result:?}"
        );
    }
}

#[test]
fn cli_schema_is_checked_before_lenient_override_deserialization() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    let cases = [
        ("picker=\"not-a-picker\"", ConfigAdmissionError::CliInvalid),
        // SemanticInvalid, not CliInvalid — and the difference is only about
        // WHICH check speaks first; both refuse the override.
        // `typed_semantic_errors` runs ahead of the schema walk in
        // `config_validate`, so now that `AppsConfig::validate` knows the tab
        // registry it rejects an unknown id before the schema enum is reached.
        // That is the better of the two errors: it can say the id is unknown (or
        // that `observe` is disabled) where a schema mismatch only says the value
        // is not in an enum.
        //
        // The ordering this test is NAMED for is still pinned by the
        // `picker="not-a-picker"` case above, which no semantic check claims.
        (
            "apps.tab_order=[\"work\",\"observe-2\"]",
            ConfigAdmissionError::SemanticInvalid,
        ),
    ];
    for (override_value, expected) in cases {
        let overrides = vec![override_value.to_string()];
        let result = admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
            profile: None,
            env: &env,
            overrides: &overrides,
            hosts: &host_snapshot,
            paths: &path_context(),
        });
        assert!(
            matches!(&result, Err(error) if *error == expected),
            "override {override_value}: {result:?}"
        );
    }

    let overrides = vec!["keybinds.custom-action=\"ctrl-x\"".to_string()];
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &path_context(),
    })
    .expect("valid custom map key override");
    assert_eq!(
        admitted.config().keybinds.normal.get("custom-action"),
        Some(&"ctrl-x".to_string())
    );

    let overrides = vec!["apps.tab_order=[\"work\",\"observe\"]".to_string()];
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &path_context(),
    })
    .expect("valid array override");
    assert_eq!(
        admitted.config().apps.tab_order,
        vec!["work".to_string(), "observe".to_string()]
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
        paths: &path_context(),
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
        paths: &path_context(),
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
        paths: &path_context(),
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
        paths: &path_context(),
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
            paths: &path_context(),
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
            paths: &path_context(),
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
        paths: &path_context(),
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
        paths: &path_context(),
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
        paths: &path_context(),
    });
    assert!(matches!(result, Err(ConfigAdmissionError::Oversized)));

    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        // A typo inside a security-relevant table still refuses; the
        // relaxed case (an unknown key elsewhere) has its own test.
        base: SourceInput::bytes("base", false, b"[sandbox]\ntypoed_key = true\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &path_context(),
    });
    assert!(matches!(result, Err(ConfigAdmissionError::SchemaInvalid)));

    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"[serve]\ncors_origins = [\"*\"]\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &path_context(),
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
        paths: &path_context(),
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
        paths: &path_context(),
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
        paths: &path_context(),
    });
    assert!(matches!(result, Err(ConfigAdmissionError::ParseInvalid)));
}

#[test]
fn rejected_candidate_does_not_install_process_global_remote_policy() {
    // The claim under test is that admission — rejected OR accepted — leaves
    // the process-global tuning alone, so the check is simply that the value
    // does not move across it.
    //
    // It deliberately does NOT plant a sentinel first. `set_ssh_tune` is
    // first-set-wins by design (see `remote_tune::set_ssh_tune`), so under
    // `cargo test`'s SHARED process — which is what `just coverage` runs,
    // unlike nextest's process-per-test — planting one is a silent no-op the
    // moment any other test has loaded a config, and so is the "restore" that
    // used to close this test. The assertion then turned on which test ran
    // first. Comparing before against after proves the same property and does
    // not care.
    let before = crate::remote_tune::ssh_tune();

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
        paths: &path_context(),
    });
    assert!(matches!(result, Err(ConfigAdmissionError::SemanticInvalid)));
    assert_eq!(
        crate::remote_tune::ssh_tune(),
        before,
        "a rejected candidate moved the process-global tuning"
    );

    let valid = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"[remote]\nkeepalive_interval_secs = 9\n"),
        profile: None,
        env: &env,
        overrides: &overrides,
        hosts: &host_snapshot,
        paths: &path_context(),
    })
    .expect("pure admission of valid policy");
    assert_eq!(valid.config().remote.keepalive_interval_secs, 9);
    assert_eq!(
        crate::remote_tune::ssh_tune(),
        before,
        "admission installed the process-global tuning; only an explicit \
         `RemoteConfig::install` may"
    );
}

#[cfg(unix)]
#[test]
fn admission_refuses_lossy_captured_home_identity() {
    use std::os::unix::ffi::OsStringExt;
    let paths = PathExpansionContext::from_home(std::path::PathBuf::from(
        std::ffi::OsString::from_vec(b"/home/non-utf8-\xff".to_vec()),
    ));
    let env = TestEnv::default();
    let host_snapshot = hosts();
    let result = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::absent("base", false),
        profile: None,
        env: &env,
        overrides: &[],
        hosts: &host_snapshot,
        paths: &paths,
    });
    assert!(matches!(result, Err(ConfigAdmissionError::InvalidUtf8)));
}

#[test]
fn compatibility_diagnostics_are_bounded_by_bytes_as_well_as_count() {
    // A wide legacy workspace table produces one compatibility diagnostic per
    // child.  Keep the source under the scanner work cap while making each
    // diagnostic large enough to expose count-only retention.
    let slug = "s".repeat(900);
    let mut source = String::new();
    for index in 0..crate::config_budget::MAX_DIAGNOSTICS {
        source.push_str(&format!("[workspace.\"{slug}{index:03}\"]\n"));
    }
    let normalized = crate::config_compat::normalize_admission(&source)
        .expect("wide compatibility source remains within source/serialization budgets");
    assert!(normalized.diagnostics.len() <= crate::config_budget::MAX_DIAGNOSTICS);
    assert!(
        normalized
            .diagnostics
            .iter()
            .all(|message| message.len() <= crate::config_budget::MAX_DIAGNOSTIC_BYTES)
    );
    let total = normalized
        .diagnostics
        .iter()
        .map(String::len)
        .sum::<usize>();
    assert!(
        total <= crate::config_budget::MAX_DIAGNOSTICS * crate::config_budget::MAX_DIAGNOSTIC_BYTES
    );
}

/// Display/telemetry values the runtime has always clamped or defaulted keep
/// that behavior (main started with `metrics.timeout_ms = 50`), but each is
/// named as a warning instead of refusing startup or disappearing silently.
#[test]
fn previously_clamped_values_clamp_with_a_named_warning() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    for (body, key) in [
        (&b"[metrics]\ntimeout_ms = 50\n"[..], "metrics.timeout_ms"),
        (
            &b"[metrics]\ninterval_secs = 0.5\n"[..],
            "metrics.interval_secs",
        ),
        (
            &b"[clipboard]\nkeep_hours = 0\n"[..],
            "clipboard.keep_hours",
        ),
        (&b"[bars]\ndate_format = \"%Q\"\n"[..], "bars.date_format"),
    ] {
        let admitted = admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, body),
            profile: None,
            env: &env,
            overrides: &[],
            hosts: &host_snapshot,
            paths: &path_context(),
        })
        .unwrap_or_else(|error| panic!("{key}: {error}"));
        assert!(
            admitted
                .trace()
                .diagnostics()
                .iter()
                .any(|d| d.severity == DiagnosticSeverity::Warning && d.message.starts_with(key)),
            "{key} must be named: {:?}",
            admitted.trace().diagnostics()
        );
    }
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"[metrics]\ntimeout_ms = 50\n"),
        profile: None,
        env: &env,
        overrides: &[],
        hosts: &host_snapshot,
        paths: &path_context(),
    })
    .unwrap();
    assert_eq!(admitted.config().metrics.timeout_ms, 100);
}

/// A selected profile whose overlay file does not exist is an empty layer
/// (nothing creates one); an explicit missing source stays an error.
#[test]
fn absent_selected_profile_overlay_is_an_empty_layer() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    let admitted = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: Some(SourceInput::absent("profile", false)),
        env: &env,
        overrides: &[],
        hosts: &host_snapshot,
        paths: &path_context(),
    })
    .expect("absent overlay admits");
    assert_eq!(admitted.config().branch_prefix, "safe/");
    assert!(
        !admitted
            .trace()
            .entries()
            .iter()
            .any(|entry| entry.layer == LayerKind::Profile)
    );
}

/// Layers are admitted once; composing hosts onto them equals one-shot
/// admission.
#[test]
fn staged_layers_with_hosts_equal_one_shot_admission() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    let one_shot = admit(AdmissionInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &env,
        overrides: &[],
        hosts: &host_snapshot,
        paths: &path_context(),
    })
    .unwrap();
    let staged = admit_layers(LayerInputs {
        defaults: Config::default(),
        base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
        profile: None,
        env: &env,
        overrides: &[],
        paths: &path_context(),
    })
    .unwrap()
    .with_hosts(&host_snapshot)
    .unwrap();
    assert_eq!(one_shot.revision(), staged.revision());
}

/// A refusal names its source and key path (never the value) so the
/// startup error is actionable; before this the message was category-only.
#[test]
fn rejection_detail_names_source_and_key_without_values() {
    let clean = TestEnv::default();
    let base = |body: &'static [u8]| SourceInput::bytes("base", false, body);
    let detail = |base_input, env: &TestEnv, overrides: &[String]| {
        rejection_detail(&LayerInputs {
            defaults: Config::default(),
            base: base_input,
            profile: None,
            env,
            overrides,
            paths: &path_context(),
        })
    };
    let file = detail(base(b"picker = \"sideways\"\n"), &clean, &[]).expect("file detail");
    assert!(file.starts_with("config file:"), "{file}");
    assert!(file.contains("picker"), "{file}");

    let mut env = TestEnv::default();
    env.0
        .insert("THEGN_SANDBOX_ENABLED".into(), "canary-not-bool".into());
    let env_detail = detail(base(b""), &env, &[]).expect("env detail");
    assert!(env_detail.contains("THEGN_SANDBOX_ENABLED"), "{env_detail}");
    assert!(!env_detail.contains("canary"), "{env_detail}");

    let cli =
        detail(base(b""), &clean, &["picker=\"canary-picker\"".to_string()]).expect("cli detail");
    assert!(cli.starts_with("--set picker"), "{cli}");
    assert!(!cli.contains("canary"), "{cli}");

    let profile = rejection_detail(&LayerInputs {
        defaults: Config::default(),
        base: base(b""),
        profile: Some(SourceInput::bytes("profile", false, b"picker = [")),
        env: &clean,
        overrides: &[],
        paths: &path_context(),
    })
    .expect("profile detail");
    assert!(profile.starts_with("profile overlay:"), "{profile}");

    assert_eq!(
        detail(base(b"branch_prefix = \"ok/\"\n"), &clean, &[]),
        None
    );
}

/// One shared config.toml is read by builds of different ages. An unknown key
/// in a security-relevant table still refuses (an ignored policy key can mean
/// policy silently not applied); anywhere else it is an ignorable warning, so
/// a newer build's key cannot brick an older build.
#[test]
fn unknown_keys_refuse_only_in_security_relevant_tables() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    let admit_body = |body: &'static [u8]| {
        admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, body),
            profile: None,
            env: &env,
            overrides: &[],
            hosts: &host_snapshot,
            paths: &path_context(),
        })
    };
    for body in [
        &b"[sandbox]\nunknown_policy_knob = true\n"[..],
        &b"[merge_queue]\nunknown_gate = \"x\"\n"[..],
        &b"[database]\nunknown_authority = \"any\"\n"[..],
        &b"[network]\nunknown_mode = 1\n"[..],
        &b"[[agents]]\nname = \"a\"\ncommand = \"a\"\nunknown_permission = true\n"[..],
    ] {
        assert!(
            matches!(
                admit_body(body),
                Err(ConfigAdmissionError::SchemaInvalid | ConfigAdmissionError::SemanticInvalid)
            ),
            "{}: a security-relevant unknown key must refuse",
            String::from_utf8_lossy(body)
        );
    }
    for body in [
        &b"[ui]\nnewer_build_knob = true\n"[..],
        &b"[bars]\nnewer_build_knob = 3\n"[..],
        &b"newer_top_level_table_from_a_newer_build = { a = 1 }\n"[..],
    ] {
        let admitted = admit_body(body)
            .unwrap_or_else(|error| panic!("{}: {error}", String::from_utf8_lossy(body)));
        assert!(
            admitted.trace().diagnostics().iter().any(|diagnostic| {
                diagnostic.kind == DiagnosticKind::UnknownKey
                    && diagnostic.severity == DiagnosticSeverity::Warning
            }),
            "{}: must warn: {:?}",
            String::from_utf8_lossy(body),
            admitted.trace().diagnostics()
        );
    }
}

/// The security split lives at the call site: the host-row decoder and every
/// other schema consumer still refuse every unknown key.
#[test]
fn only_the_configuration_layers_relax_unknown_keys() {
    use crate::config_validate::{UnknownKeys, is_security_relevant_path, split_unknown_keys};
    assert!(is_security_relevant_path("sandbox.knob"));
    assert!(is_security_relevant_path("env.dev.provider.knob"));
    assert!(is_security_relevant_path("agents[0].knob"));
    assert!(!is_security_relevant_path("ui.knob"));
    // A bare root key is the "newer build added a table" case.
    assert!(!is_security_relevant_path("brand_new_table"));
    let messages = vec![
        "ui.knob: unknown key".to_string(),
        "sandbox.knob: unknown key".to_string(),
    ];
    let (errors, warnings) = split_unknown_keys(messages.clone(), UnknownKeys::Reject);
    assert_eq!(errors.len(), 2);
    assert!(warnings.is_empty());
    let (errors, warnings) = split_unknown_keys(messages, UnknownKeys::RejectSecurityRelevant);
    assert_eq!(errors, ["sandbox.knob: unknown key"]);
    assert_eq!(warnings.len(), 1);
    // The strict host-row decoder is not on the relaxed path.
    let row = serde_json::json!({"reach": "ssh", "unknown_row_field": 1});
    assert!(
        !crate::config_validate::validate_schema_value_with_root(
            &row,
            &schemars::schema_for!(crate::host_config::HostConfig),
        )
        .is_empty()
    );
}

/// The EFFECTIVE (post-processed) candidate is validated too: tilde
/// expansion, injected defaults and clamps must not produce a config the
/// runtime uses but nothing ever validated.
#[test]
fn the_post_processed_candidate_is_validated() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    let ((), events) = crate::config_validate::semantic_observation::capture(|| {
        admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, b"branch_prefix = \"safe/\"\n"),
            profile: None,
            env: &env,
            overrides: &[],
            hosts: &host_snapshot,
            paths: &path_context(),
        })
        .map(|_| ())
        .expect("valid candidate")
    });
    let passes = events
        .iter()
        .filter(|event| **event == "semantic_start")
        .count();
    assert!(
        passes >= 2,
        "the raw AND the post-processed candidate are validated: {events:?}"
    );
}

/// The per-project overlay carries accounts, hooks, sandbox mounts, both
/// queues, ci, autopilot, the MCP scope ceiling, env bundles, git and editor.
/// `[workspace.<slug>]` is normalized to `[project.<slug>]` BEFORE any schema
/// walk, so the protected name is the schema's (`project`) — the legacy
/// spelling alone protected nothing.
#[test]
fn project_overlay_unknown_keys_refuse_through_the_legacy_spelling_too() {
    use crate::config_validate::is_security_relevant_path;
    assert!(is_security_relevant_path("project.x.hooks.pre"));
    assert!(is_security_relevant_path("project.thegn.sandbox_mount"));
    assert!(is_security_relevant_path("workspace.x.hooks.pre"));

    let env = TestEnv::default();
    let host_snapshot = hosts();
    // Written with the legacy spelling, as the user's live config does.
    for body in [
        &b"[workspace.thegn]\nsandbox_mount = [\"/etc\"]\n"[..],
        &b"[project.thegn]\nsandbox_mount = [\"/etc\"]\n"[..],
        &b"[workspace.thegn.mcp_serve]\nscopes_v2 = [\"all\"]\n"[..],
    ] {
        let result = admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, body),
            profile: None,
            env: &env,
            overrides: &[],
            hosts: &host_snapshot,
            paths: &path_context(),
        });
        assert!(
            matches!(result, Err(ConfigAdmissionError::SchemaInvalid)),
            "{} must refuse, not warn",
            String::from_utf8_lossy(body)
        );
    }
}

/// A typo'd top-level security table ([sandboxx], [metric]) would otherwise
/// vanish with its whole contents: the nearest-key hint the walk already
/// computes makes it a refusal, while a genuinely new table — never within
/// the hint's edit distance — still only warns.
#[test]
fn a_typoed_security_table_refuses_but_a_new_table_warns() {
    let env = TestEnv::default();
    let host_snapshot = hosts();
    let admit_body = |body: &'static [u8]| {
        admit(AdmissionInputs {
            defaults: Config::default(),
            base: SourceInput::bytes("base", false, body),
            profile: None,
            env: &env,
            overrides: &[],
            hosts: &host_snapshot,
            paths: &path_context(),
        })
    };
    for body in [
        &b"[sandboxx]\nenabled = true\n"[..],
        &b"[metric]\ninterval_secs = 5\n"[..],
    ] {
        assert!(
            matches!(admit_body(body), Err(ConfigAdmissionError::SchemaInvalid)),
            "{} is a typo of a security table",
            String::from_utf8_lossy(body)
        );
    }
    let admitted = admit_body(b"[telemetry_from_a_newer_build]\nknob = 1\n")
        .expect("an unrecognizable new table still only warns");
    assert!(
        admitted
            .trace()
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.kind == DiagnosticKind::UnknownKey)
    );
}
