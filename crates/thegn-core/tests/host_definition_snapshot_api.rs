use thegn_core::store::{HostDefinitionReadError, HostDefinitionsSnapshot};

#[test]
fn external_backend_can_construct_only_source_valid_snapshots() {
    let rows = vec![("local".to_string(), r#"{"reach":"local"}"#.to_string())];
    let snapshot = HostDefinitionsSnapshot::from_captured_rows(0, rows)
        .expect("a backend can produce a validated snapshot");
    assert_eq!(snapshot.observed_schema(), 0);
    assert_eq!(snapshot.definitions().len(), 1);
    assert_eq!(snapshot.definitions()[0].0, "local");

    for (rows, expected) in [
        (
            vec![("bad".into(), "{".into())],
            HostDefinitionReadError::InvalidJson,
        ),
        (
            vec![("bad".into(), r#"{"reach":"local","reach":"local"}"#.into())],
            HostDefinitionReadError::DuplicateJsonKey,
        ),
        (
            vec![("bad".into(), r#"{"unknown":"private"}"#.into())],
            HostDefinitionReadError::InvalidDefinition,
        ),
        (
            vec![("same".into(), "{}".into()), ("same".into(), "{}".into())],
            HostDefinitionReadError::DuplicateName,
        ),
        (
            vec![("bad".into(), r#"{"reach":"unknown"}"#.into())],
            HostDefinitionReadError::InvalidDefinition,
        ),
        (
            vec![("bad".into(), " ".repeat(4 * 1024 * 1024 + 1))],
            HostDefinitionReadError::Bounds,
        ),
    ] {
        assert_eq!(
            HostDefinitionsSnapshot::from_captured_rows(0, rows).unwrap_err(),
            expected
        );
    }
}
