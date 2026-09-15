use super::*;

fn structure(value: &Value) -> Result<usize, HostCompositionError> {
    let mut nodes = 0;
    check_structure(value, 0, &mut nodes)?;
    Ok(nodes)
}

#[test]
fn structure_depth_node_container_key_and_utf8_limits_are_inclusive() {
    let mut value = Value::Null;
    for _ in 0..MAX_DEPTH {
        value = Value::Array(vec![value]);
    }
    assert_eq!(structure(&value), Ok(MAX_DEPTH + 1));
    assert_eq!(
        structure(&Value::Array(vec![value])),
        Err(HostCompositionError::Bounds)
    );

    let mut groups = vec![Value::Array(vec![Value::Null; 1023]); 64];
    groups[63] = Value::Array(vec![Value::Null; 1022]);
    let mut value = Value::Array(groups);
    assert_eq!(structure(&value), Ok(MAX_NODES));
    value.as_array_mut().unwrap()[63]
        .as_array_mut()
        .unwrap()
        .push(Value::Null);
    assert_eq!(structure(&value), Err(HostCompositionError::Bounds));

    assert!(structure(&Value::Array(vec![Value::Null; MAX_CONTAINER])).is_ok());
    assert_eq!(
        structure(&Value::Array(vec![Value::Null; MAX_CONTAINER + 1])),
        Err(HostCompositionError::Bounds)
    );
    let mut map = serde_json::Map::new();
    for i in 0..MAX_CONTAINER {
        map.insert(format!("key-{i}"), Value::Null);
    }
    assert!(structure(&Value::Object(map.clone())).is_ok());
    map.insert("one-too-many".into(), Value::Null);
    assert_eq!(
        structure(&Value::Object(map)),
        Err(HostCompositionError::Bounds)
    );

    let key = "k".repeat(MAX_KEY_BYTES);
    assert!(structure(&serde_json::json!({ key: null })).is_ok());
    let key = "k".repeat(MAX_KEY_BYTES + 1);
    assert_eq!(
        structure(&serde_json::json!({ key: null })),
        Err(HostCompositionError::Bounds)
    );
    let text = "é".repeat(MAX_STRING_BYTES / 2);
    assert!(structure(&Value::String(text.clone())).is_ok());
    assert_eq!(
        structure(&Value::String(format!("{text}x"))),
        Err(HostCompositionError::Bounds)
    );
}

#[test]
fn streaming_byte_counter_refuses_one_over_without_retaining_payload() {
    let text = "x".repeat(1022); // quoted JSON is exactly1024 bytes
    assert_eq!(serialized_len(&text, 1024), Ok(1024));
    assert_eq!(
        serialized_len(&text, 1023),
        Err(HostCompositionError::Bounds)
    );
    let mut output = JsonOutput {
        limit: 1024,
        written: 0,
        bytes: None,
    };
    serde_json::to_writer(&mut output, &text).unwrap();
    assert_eq!(output.written, 1024);
    assert!(output.bytes.is_none());
    let mut total = usize::MAX;
    assert_eq!(
        charge(&mut total, 1, usize::MAX),
        Err(HostCompositionError::Bounds)
    );
}

/// ASCII padding in an existing inert string map. Independent serialization
/// computes exact public API boundaries without changing admission constants.
pub(super) fn config_bytes(target: usize) -> Config {
    let mut cfg = config();
    let mut padding = std::collections::BTreeMap::new();
    for index in 0..64 {
        padding.insert(format!("entry-{index:02}"), String::new());
    }
    cfg.program_remap.insert("private-padding".into(), padding);
    let initial = serde_json::to_vec(&cfg).unwrap().len();
    let mut remaining = target
        .checked_sub(initial)
        .expect("fixture target exceeds defaults");
    for text in cfg
        .program_remap
        .get_mut("private-padding")
        .unwrap()
        .values_mut()
    {
        let bytes = remaining.min(MAX_STRING_BYTES);
        *text = "x".repeat(bytes);
        remaining -= bytes;
    }
    assert_eq!(remaining, 0);
    assert_eq!(serde_json::to_vec(&cfg).unwrap().len(), target);
    cfg
}

#[test]
fn complete_configs_at_byte_limit_pass_and_post_merge_growth_is_refused() {
    assert!(compose(config_bytes(MAX_CONFIG_BYTES - 1)).is_ok());
    assert!(compose(config_bytes(MAX_CONFIG_BYTES)).is_ok());
    assert_bounds_before_semantics(config_bytes(MAX_CONFIG_BYTES + 1));
    let db = store_with(&HostConfig {
        reach: HostReach::Local,
        ..Default::default()
    });
    let cfg = config_bytes(MAX_CONFIG_BYTES);
    assert!(admit(&cfg).is_ok(), "pre-merge input is admitted");
    let (result, events) =
        semantic_observation::capture(|| capture_and_compose_hosts_checked(&cfg, &db));
    assert_eq!(result.unwrap_err(), HostCompositionError::Bounds);
    assert_eq!(
        events,
        ["config_clone"],
        "post-merge refusal precedes semantic work"
    );
}

#[test]
fn complete_config_string_and_key_limits_precede_semantic_work() {
    let mut cfg = config();
    cfg.branch_prefix = "x".repeat(MAX_STRING_BYTES);
    assert!(compose(cfg.clone()).is_ok());
    cfg.branch_prefix.push('x');
    assert_bounds_before_semantics(cfg);

    let mut cfg = config();
    cfg.program_remap
        .insert("k".repeat(MAX_KEY_BYTES), Default::default());
    assert!(compose(cfg.clone()).is_ok());
    cfg.program_remap
        .insert("k".repeat(MAX_KEY_BYTES + 1), Default::default());
    assert_bounds_before_semantics(cfg);
}

#[test]
fn existing_project_schema_branches_remain_shared() {
    for failover in [
        Value::Bool(true),
        Value::Bool(false),
        Value::String("next".into()),
    ] {
        let value = serde_json::json!({"sandbox": {"failover": failover}});
        let expected = config_validate::validate_schema_value::<Config>(&value);
        assert_eq!(
            config_validate::validate_config_schema_value(&value),
            expected
        );
    }
    assert!(
        config_validate::validate_config_schema_value(
            &serde_json::json!({"sandbox": {"failover": true}})
        )
        .is_empty()
    );
    assert!(
        !config_validate::validate_config_schema_value(
            &serde_json::json!({"sandbox": {"failover": "invalid-checked-fixture"}})
        )
        .is_empty()
    );
}

#[test]
fn semantic_observation_restores_nested_and_unwound_fixture_custody() {
    let (_, outer) = semantic_observation::capture(|| {
        let (_, inner) = semantic_observation::capture(|| {
            assert!(
                config_validate::typed_semantic_errors(&config(), SemanticMode::StopOnError)
                    .is_empty()
            );
        });
        assert_eq!(inner, ["semantic_start"]);
        let unwound = std::panic::catch_unwind(|| {
            semantic_observation::capture(|| panic!("owned observation unwind"));
        });
        assert!(unwound.is_err());
        assert!(
            config_validate::typed_semantic_errors(&config(), SemanticMode::StopOnError).is_empty()
        );
    });
    assert_eq!(outer, ["semantic_start"]);
}
