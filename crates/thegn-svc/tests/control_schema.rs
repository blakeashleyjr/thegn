//! The control API's wire contract is pinned by a committed JSON-schema
//! snapshot: `docs/api/control-v1.json` must match the schema generated from
//! the current wire types. Change a wire type and this fails until you
//! either revert, or regenerate deliberately:
//!
//! ```sh
//! THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema
//! ```
//!
//! Within /v1 the snapshot may only be regenerated when the change is
//! additive (new optional fields, new variants with defaults) — the same
//! compatibility rule as the plugin wire.

use thegn_svc::control::*;

fn wire_schema() -> serde_json::Value {
    // One root object whose properties are the wire types, so a single file
    // pins the whole contract and a type removed from this list is itself a
    // visible diff. Routes are included so path/method changes surface too.
    let mut generator = schemars::r#gen::SchemaGenerator::default();
    let mut props = serde_json::Map::new();
    macro_rules! add {
        ($($t:ty),* $(,)?) => {$(
            let s = generator.subschema_for::<$t>();
            props.insert(stringify!($t).to_string(), serde_json::to_value(s).unwrap());
        )*};
    }
    add!(
        WorktreeInfo,
        SessionInfo,
        OpenSpec,
        ForkSpec,
        AttachKind,
        BrowserCommand,
        BrowserAction,
        PreviewFetchRequest,
        PreviewFetchReply,
        WaitCondition,
        WaitOutcome,
        SplitDir,
        RecordSpec,
        RecordStatus,
        GitFileStatus,
        PrStatusRow,
        PushedNote,
        WorktreeCreateReq,
        DispatchPutReq,
        SessionRecord,
    );
    let routes: Vec<serde_json::Value> = routes::API_CALLS
        .iter()
        .map(
            |(cap, method, path)| serde_json::json!({ "cap": cap, "method": method, "path": path }),
        )
        .collect();
    serde_json::json!({
        "$comment": "thegn control API v1 wire contract — generated; regenerate with THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema",
        "version": "1",
        "routes": routes,
        "types": serde_json::Value::Object(props),
        "definitions": serde_json::to_value(generator.take_definitions()).unwrap(),
    })
}

fn snapshot_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/api/control-v1.json")
}

#[test]
fn control_wire_matches_the_committed_snapshot() {
    let current = serde_json::to_string_pretty(&wire_schema()).unwrap() + "\n";
    let path = snapshot_path();
    if std::env::var_os("THEGN_UPDATE_SNAPSHOTS").is_some() {
        std::fs::write(&path, &current).expect("write snapshot");
        return;
    }
    let committed = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing {} — regenerate with THEGN_UPDATE_SNAPSHOTS=1",
            path.display()
        )
    });
    assert_eq!(
        committed, current,
        "control wire schema drifted from docs/api/control-v1.json. \
         If the change is additive, regenerate the snapshot (THEGN_UPDATE_SNAPSHOTS=1); \
         otherwise revert the wire change."
    );
}
