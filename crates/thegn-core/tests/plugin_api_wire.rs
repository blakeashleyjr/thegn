//! The plugin API wire contract is versioned by a committed JSON-schema
//! snapshot: `docs/api/plugin-api-<major>.<minor>.json` must match the schema
//! generated from the current wire types and host support tables. A wire-type
//! change requires an API-version decision; a support-state/doc correction may
//! retain the version but must still regenerate and review the snapshot:
//!
//! ```sh
//! THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-core --test plugin_api_wire
//! ```
//!
//! Within one minor version wire changes may only be additive (new optional
//! fields or defaulted variants). Host support metadata can narrow only when it
//! corrects an inaccurate claim or removes a runtime capability fail-closed.

use schemars::schema::RootSchema;
use thegn_core::plugin_api::*;

fn wire_schema() -> serde_json::Value {
    // One root object whose properties are the wire types, so a single file
    // pins the whole contract and a type removed from this list is itself a
    // visible diff.
    let mut root = RootSchema::default();
    let mut generator = schemars::r#gen::SchemaGenerator::default();
    let mut props = serde_json::Map::new();
    macro_rules! add {
        ($($t:ty),* $(,)?) => {$(
            let s = generator.subschema_for::<$t>();
            props.insert(stringify!($t).to_string(), serde_json::to_value(s).unwrap());
        )*};
    }
    add!(
        PluginManifest,
        PluginSpec,
        PluginMode,
        Contribution,
        ExtensionPoint,
        CadenceHint,
        Capability,
        Frame,
        RpcMessage,
        RpcResponse,
        RpcError,
        RpcErrorCode,
        HostVerb,
        PluginCallback,
        Event,
        EventKind,
        View,
        Span,
        StyleRole,
        Alert,
        IoRequest,
        IoResult,
        IoStatus,
    );
    root.definitions = generator.take_definitions();
    let mut v = serde_json::to_value(root).unwrap();
    v["x-thegn-api-version"] = serde_json::Value::String(API_VERSION.to_string());
    v["x-thegn-host-verbs"] = serde_json::Value::Array(
        HostVerb::ALL
            .iter()
            .map(|h| serde_json::Value::String(h.method_name().to_string()))
            .collect(),
    );
    v["x-thegn-extension-support"] = serde_json::Value::Array(
        HOST_EXTENSION_SUPPORT
            .iter()
            .map(|row| {
                serde_json::json!({
                    "extension_point": row.extension_point.wire_name(),
                    "since": row.since.to_string(),
                    "state": row.state.as_str(),
                    "required_capability": row.required_capability,
                    "modes": row.mode_names(),
                    "cadences": row.cadence_names(),
                    "callbacks": row.callbacks.iter().map(|callback| callback.method_name()).collect::<Vec<_>>(),
                    "owner": row.owner,
                })
            })
            .collect(),
    );
    v["x-thegn-host-verb-support"] = serde_json::Value::Array(
        HOST_VERB_SUPPORT
            .iter()
            .map(|row| {
                serde_json::json!({
                    "verb": row.verb.method_name(),
                    "since": row.since.to_string(),
                    "state": row.state.as_str(),
                    "authority": row.authority,
                    "modes": row.modes.iter().map(|mode| mode.wire_name()).collect::<Vec<_>>(),
                    "owner": row.owner,
                })
            })
            .collect(),
    );
    v["x-thegn-host-call-capabilities"] = serde_json::Value::Array(
        plugin_host_call_caps()
            .into_iter()
            .map(|capability| serde_json::Value::String(capability.to_string()))
            .collect(),
    );
    v["properties"] = serde_json::Value::Object(props);
    v
}

fn snapshot_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/api")
        .join(format!(
            "plugin-api-{}.{}.json",
            API_VERSION.major, API_VERSION.minor
        ))
}

#[test]
fn wire_schema_matches_the_committed_snapshot() {
    let current = wire_schema();
    let path = snapshot_path();
    let pretty = serde_json::to_string_pretty(&current).unwrap() + "\n";
    if std::env::var_os("THEGN_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &pretty).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}): the plugin API is at {API_VERSION} but no snapshot exists — \
             regenerate with THEGN_UPDATE_SNAPSHOTS=1",
            path.display()
        )
    });
    let committed: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
    assert_eq!(
        committed["x-thegn-api-version"],
        serde_json::Value::String(API_VERSION.to_string()),
        "snapshot file name and its embedded version disagree"
    );
    assert!(
        committed == current,
        "plugin API wire or host-support contract changed at {API_VERSION}.\n\
         Review the diff. Bump the API for a wire change (minor if additive, major if \
         breaking); support-state/documentation corrections may retain the version. Then \
         regenerate with THEGN_UPDATE_SNAPSHOTS=1.\n\
         Snapshot: {}",
        path.display()
    );
}

#[test]
fn snapshot_file_is_named_for_the_current_version() {
    assert!(
        snapshot_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(&format!("{}.{}.json", API_VERSION.major, API_VERSION.minor))
    );
}
