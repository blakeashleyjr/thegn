//! Checked captured host definitions, not launch authority or a database lease.
//!
//! The storage adapter supplies bounded raw rows from one read transaction.
//! Schema validation precedes the legacy, permissive HostConfig decoder. No
//! secret resolution, provisioning, global policy install or migration occurs.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::OnceLock;

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::host_config::HostConfig;

pub const MAX_HOST_DEFINITIONS: usize = 1024;
pub const MAX_HOST_ROWS: usize = 8192;
pub const MAX_HOST_SCHEMA_COLUMNS: usize = 128;
pub const MAX_HOST_NAME_BYTES: usize = 256;
pub const MAX_HOST_DEFINITION_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_HOST_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
const MAX_JSON_DEPTH: usize = 32;
const MAX_JSON_NODES: usize = 16384;
const MAX_JSON_KEY_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostDefinitionReadError {
    TransactionActive,
    Unavailable,
    Busy,
    IncompatibleSchema { observed: i64, supported: i64 },
    InvalidSchema,
    TooManyRows,
    TooManyDefinitions,
    Bounds,
    InvalidColumnType,
    InvalidName,
    DuplicateName,
    InvalidUtf8,
    InvalidJson,
    DuplicateJsonKey,
    InvalidDefinition,
}

impl fmt::Display for HostDefinitionReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TransactionActive => "host snapshot requires its own read transaction",
            Self::Unavailable => "host snapshot could not be read",
            Self::Busy => "host snapshot database is busy",
            Self::IncompatibleSchema { .. } => "host snapshot database schema is unsupported",
            Self::InvalidSchema => "host snapshot table schema is invalid",
            Self::TooManyRows => "host snapshot exceeds the inventory row limit",
            Self::TooManyDefinitions => "host snapshot exceeds the definition count limit",
            Self::Bounds => "host snapshot exceeds an input limit",
            Self::InvalidColumnType => "host snapshot contains an invalid column type",
            Self::InvalidName => "host snapshot contains an invalid name",
            Self::DuplicateName => "host snapshot contains duplicate names",
            Self::InvalidUtf8 => "host snapshot contains invalid UTF-8",
            Self::InvalidJson => "host snapshot contains invalid JSON",
            Self::DuplicateJsonKey => "host snapshot contains duplicate JSON keys",
            Self::InvalidDefinition => "host snapshot contains an invalid definition",
        })
    }
}

impl std::error::Error for HostDefinitionReadError {}

/// Immutable source-validity data. Not proof of ownership, freshness after the
/// read, final composed config validity, or permission to execute a workload.
pub struct HostDefinitionsSnapshot {
    observed_schema: i64,
    definitions: Vec<(String, HostConfig)>,
}

impl fmt::Debug for HostDefinitionsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostDefinitionsSnapshot")
            .field("observed_schema", &self.observed_schema)
            .field("definition_count", &self.definitions.len())
            .finish_non_exhaustive()
    }
}

impl HostDefinitionsSnapshot {
    pub fn observed_schema(&self) -> i64 {
        self.observed_schema
    }

    pub fn definitions(&self) -> &[(String, HostConfig)] {
        &self.definitions
    }

    pub(crate) fn from_raw(
        observed_schema: i64,
        rows: Vec<(String, String)>,
    ) -> Result<Self, HostDefinitionReadError> {
        if rows.len() > MAX_HOST_DEFINITIONS {
            return Err(HostDefinitionReadError::TooManyDefinitions);
        }
        let mut names = BTreeSet::new();
        let mut bytes = 0;
        let mut definitions = Vec::with_capacity(rows.len());
        for (name, json) in rows {
            validate_name(&name)?;
            if !names.insert(name.clone()) {
                return Err(HostDefinitionReadError::DuplicateName);
            }
            bytes = add_bytes(bytes, name.len(), json.len())?;
            definitions.push((name, decode_definition(&json)?));
        }
        definitions.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Self {
            observed_schema,
            definitions,
        })
    }
}

pub(crate) fn validate_name(name: &str) -> Result<(), HostDefinitionReadError> {
    if name.is_empty() || name.len() > MAX_HOST_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(HostDefinitionReadError::InvalidName);
    }
    Ok(())
}

pub(crate) fn add_bytes(
    prior: usize,
    name: usize,
    json: usize,
) -> Result<usize, HostDefinitionReadError> {
    if name > MAX_HOST_NAME_BYTES || json > MAX_HOST_DEFINITION_BYTES {
        return Err(HostDefinitionReadError::Bounds);
    }
    prior
        .checked_add(name)
        .and_then(|n| n.checked_add(json))
        .filter(|n| *n <= MAX_HOST_SNAPSHOT_BYTES)
        .ok_or(HostDefinitionReadError::Bounds)
}

fn decode_definition(raw: &str) -> Result<HostConfig, HostDefinitionReadError> {
    if raw.len() > MAX_HOST_DEFINITION_BYTES {
        return Err(HostDefinitionReadError::Bounds);
    }
    let budget = JsonBudget {
        nodes: Cell::new(0),
        error: Cell::new(None),
    };
    let mut deserializer = serde_json::Deserializer::from_str(raw);
    let value = JsonSeed {
        budget: &budget,
        depth: 0,
    }
    .deserialize(&mut deserializer)
    .map_err(|_| {
        budget
            .error
            .get()
            .unwrap_or(HostDefinitionReadError::InvalidJson)
    })?;
    deserializer
        .end()
        .map_err(|_| HostDefinitionReadError::InvalidJson)?;
    if !value.is_object()
        || !crate::config_validate::validate_schema_value_with_root(&value, host_schema())
            .is_empty()
    {
        return Err(HostDefinitionReadError::InvalidDefinition);
    }
    serde_json::from_value(value).map_err(|_| HostDefinitionReadError::InvalidDefinition)
}

fn host_schema() -> &'static schemars::schema::RootSchema {
    static SCHEMA: OnceLock<schemars::schema::RootSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| schemars::schema_for!(HostConfig))
}

struct JsonBudget {
    nodes: Cell<usize>,
    error: Cell<Option<HostDefinitionReadError>>,
}

impl JsonBudget {
    fn fail<E: serde::de::Error>(&self, error: HostDefinitionReadError) -> E {
        self.error.set(Some(error));
        E::custom("invalid host definition")
    }
}

struct JsonSeed<'a> {
    budget: &'a JsonBudget,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Value, D::Error> {
        if self.depth > MAX_JSON_DEPTH || self.budget.nodes.get() >= MAX_JSON_NODES {
            return Err(self.budget.fail(HostDefinitionReadError::Bounds));
        }
        self.budget.nodes.set(self.budget.nodes.get() + 1);
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for JsonSeed<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded host definition JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| self.budget.fail(HostDefinitionReadError::InvalidJson))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(JsonSeed {
            budget: self.budget,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key_seed(JsonKey(self.budget))? {
            if values.contains_key(&key) {
                return Err(self.budget.fail(HostDefinitionReadError::DuplicateJsonKey));
            }
            let value = map.next_value_seed(JsonSeed {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

struct JsonKey<'a>(&'a JsonBudget);
impl<'de> DeserializeSeed<'de> for JsonKey<'_> {
    type Value = String;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<String, D::Error> {
        de.deserialize_str(self)
    }
}
impl<'de> Visitor<'de> for JsonKey<'_> {
    type Value = String;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded JSON key")
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<String, E> {
        if value.len() > MAX_JSON_KEY_BYTES {
            return Err(self.0.fail(HostDefinitionReadError::Bounds));
        }
        Ok(value.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_schema_preserves_generic_validator_results() {
        assert!(std::ptr::eq(host_schema(), host_schema()));
        for value in [
            serde_json::json!({}),
            serde_json::json!({"reach": "local", "install_runtime": "always"}),
            serde_json::json!({"reach": "typo"}),
            serde_json::json!({"ssh": {"host": false}}),
            serde_json::json!({"unknown": 1}),
            serde_json::json!([]),
        ] {
            assert_eq!(
                crate::config_validate::validate_schema_value::<HostConfig>(&value),
                crate::config_validate::validate_schema_value_with_root(&value, host_schema()),
            );
        }
    }

    #[test]
    fn raw_schema_validation_precedes_lenient_host_decode() {
        for raw in [
            r#"{"reach":"typo"}"#,
            r#"{"install_runtime":"typo"}"#,
            r#"{"reach":false}"#,
            r#"{"unknown":"private-secret"}"#,
            "[]",
        ] {
            assert_eq!(
                decode_definition(raw).unwrap_err(),
                HostDefinitionReadError::InvalidDefinition,
                "{raw}"
            );
        }
        assert_eq!(
            decode_definition(r#"{"reach":"local","install_runtime":"always"}"#)
                .unwrap()
                .install_runtime,
            crate::host_config::InstallConsent::Auto
        );
    }

    #[test]
    fn duplicate_keys_are_rejected_before_value_collapse() {
        for raw in [
            r#"{"reach":"ssh","reach":"local"}"#,
            r#"{"ssh":{"host":"one","host":"two"}}"#,
            r#"{"reach":"ssh","\u0072each":"local"}"#,
        ] {
            assert_eq!(
                decode_definition(raw).unwrap_err(),
                HostDefinitionReadError::DuplicateJsonKey
            );
        }
        for raw in ["", "{", "{} {}", r#"{"reach":"local"} trailing"#] {
            assert_eq!(
                decode_definition(raw).unwrap_err(),
                HostDefinitionReadError::InvalidJson
            );
        }
    }

    #[test]
    fn json_and_row_budgets_refuse_without_truncation() {
        let deep = format!(
            "{}0{}",
            "[".repeat(MAX_JSON_DEPTH + 1),
            "]".repeat(MAX_JSON_DEPTH + 1)
        );
        assert_eq!(
            decode_definition(&deep).unwrap_err(),
            HostDefinitionReadError::Bounds
        );
        let wide = format!("[{}]", vec!["0"; MAX_JSON_NODES].join(","));
        assert_eq!(
            decode_definition(&wide).unwrap_err(),
            HostDefinitionReadError::Bounds
        );
        let key = format!(r#"{{"{}":0}}"#, "x".repeat(MAX_JSON_KEY_BYTES + 1));
        assert_eq!(
            decode_definition(&key).unwrap_err(),
            HostDefinitionReadError::Bounds
        );
        assert_eq!(
            decode_definition(&" ".repeat(MAX_HOST_DEFINITION_BYTES + 1)).unwrap_err(),
            HostDefinitionReadError::Bounds
        );
        assert_eq!(
            add_bytes(usize::MAX, 1, 1),
            Err(HostDefinitionReadError::Bounds)
        );
        assert_eq!(
            add_bytes(0, 1, MAX_HOST_SNAPSHOT_BYTES - 1),
            Ok(MAX_HOST_SNAPSHOT_BYTES)
        );
        assert_eq!(
            add_bytes(1, 1, MAX_HOST_SNAPSHOT_BYTES - 1),
            Err(HostDefinitionReadError::Bounds)
        );
    }

    #[test]
    fn names_are_checked_and_snapshot_is_sorted() {
        let rows = vec![
            ("z".into(), "{}".into()),
            ("a".into(), r#"{"reach":"local"}"#.into()),
        ];
        let snapshot = HostDefinitionsSnapshot::from_raw(68, rows).unwrap();
        assert_eq!(snapshot.observed_schema(), 68);
        assert_eq!(snapshot.definitions()[0].0, "a");
        for name in ["", "bad\nname", "\u{7f}"] {
            assert_eq!(
                validate_name(name),
                Err(HostDefinitionReadError::InvalidName)
            );
        }
        assert!(validate_name(&"x".repeat(MAX_HOST_NAME_BYTES)).is_ok());
        assert_eq!(
            validate_name(&"x".repeat(MAX_HOST_NAME_BYTES + 1)),
            Err(HostDefinitionReadError::InvalidName)
        );
        assert_eq!(
            HostDefinitionsSnapshot::from_raw(68, vec![("a".into(), "{}".into()); 2]).unwrap_err(),
            HostDefinitionReadError::DuplicateName
        );
    }

    #[test]
    fn errors_never_echo_private_definition_contents() {
        let error = decode_definition(r#"{"reach":"secret\u001b[2J\n"}"#).unwrap_err();
        assert_eq!(
            error.to_string(),
            "host snapshot contains an invalid definition"
        );
        assert!(!format!("{error:?}").contains("secret"));
        let snapshot = HostDefinitionsSnapshot::from_raw(
            68,
            vec![(
                "private-name".into(),
                r#"{"image":"private-secret"}"#.into(),
            )],
        )
        .unwrap();
        assert!(!format!("{snapshot:?}").contains("private"));
    }
}
