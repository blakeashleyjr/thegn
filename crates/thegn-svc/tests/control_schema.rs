//! The control API's wire contract is pinned by a committed JSON-schema
//! snapshot: `docs/api/control-v1.json` must match the schema generated from
//! the current wire types. Change a wire type and this fails until you
//! either revert, or regenerate deliberately:
//!
//! ```sh
//! THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema
//! ```
//!
//! Within /v1 the snapshot is normally additive-only. A removal requires an
//! accepted compatibility decision proving the operation has no success path;
//! THE-103 is that decision for the former browser-driving stub.

use thegn_core::control_wire::FeedFilter;
use thegn_svc::control::*;
// Named explicitly rather than glob-imported so a collision with an existing
// wire type is a compile error here instead of silently shadowing one.
use thegn_svc::control::http::{
    AgentSessionsQuery, AttachQuery, CalendarQuery, CommentBody, CommitBody, DetachBody,
    DispatchStatusBody, EventsQuery, InputBody, IssueBody, IssuesQuery, MergeBody,
    OpenWorktreeBody, PairBody, ResizeBody, SplitBody, StageBody, WaitBody, WorktreeQuery,
};

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
        SkillInfo,
        SkillsList,
        EditorOpenRequest,
        SessionInfo,
        OpenSpec,
        ForkSpec,
        AttachKind,
        PreviewFetchRequest,
        PreviewFetchReply,
        WaitCondition,
        WaitOutcome,
        SplitDir,
        RecordSpec,
        RecordStatus,
        GitFileStatus,
        PrStatusRow,
        CiRunsReply,
        CiLogsReply,
        PushedNote,
        AutomationOrigin,
        AutomationRuleInfo,
        AutomationTestRequest,
        AutomationTestReply,
        ToolRunRequest,
        WorktreeCreateReq,
        DispatchPutReq,
        SessionRecord,
        ErrorBody,
        FeedFilter,
    );
    // HTTP-only request DTOs. These exist solely as axum extractor types in
    // `control::http`, so they never reached the published contract even
    // though they ARE the shape a client has to send. The registration test
    // below fails if a new one is added without landing here.
    add!(
        PairBody,
        IssueBody,
        InputBody,
        ResizeBody,
        WaitBody,
        SplitBody,
        DetachBody,
        OpenWorktreeBody,
        IssuesQuery,
        CommentBody,
        DispatchStatusBody,
        WorktreeQuery,
        StageBody,
        CommitBody,
        MergeBody,
        CalendarQuery,
        AgentSessionsQuery,
        EventsQuery,
        AttachQuery,
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

#[test]
fn removed_browser_drive_is_absent_from_generated_contract_inputs() {
    let schema = wire_schema().to_string();
    assert!(!schema.contains("browser.drive"));
    assert!(!schema.contains("BrowserCommand"));
    assert!(!schema.contains("BrowserAction"));

    let proto = include_str!("../proto/thegn/control/v1/control.proto");
    assert!(!proto.contains("DriveBrowser"));
    assert!(!proto.contains("DriveBrowserRequest"));
}

/// Every DTO an HTTP handler accepts must appear in the published contract.
///
/// The `add!` list is hand-maintained, and for HTTP-only types nothing forced
/// it to stay complete: such a DTO exists only as an axum extractor in
/// `control::http`, so adding an endpoint shipped a request shape clients must
/// send but could not discover. This reads the handler signatures as the
/// authority and fails until the new type is registered.
///
/// Scope is request DTOs — `Json<T>` / `Query<T>` in extractor position.
/// Response bodies are built as untyped `serde_json` values and are pinned by
/// the route list plus the snapshot, not here.
#[test]
fn http_dtos_are_all_registered_in_the_published_schema() {
    // Recorded, reviewed exclusions. An entry is NOT "we chose not to publish
    // this" — it is a representability defect with a named cause, and the list
    // should shrink to nothing.
    //
    // CalendarIngestBody holds `Vec<thegn_core::calendar::CalEvent>`, whose
    // type graph (EventTime -> NaiveDate / NaiveDateTime / DateTime<Utc> /
    // TzRef, plus Recurrence, Reminder and theme::Hue) derives no JsonSchema
    // and would need schemars' chrono support threaded through thegn-core.
    // That is its own change; until then this endpoint's request shape is
    // undocumented and callers must read the handler.
    const UNREPRESENTABLE: &[&str] = &["CalendarIngestBody"];

    let source = include_str!("../src/control/http.rs");
    let schema = wire_schema();
    let registered = schema
        .get("types")
        .and_then(serde_json::Value::as_object)
        .expect("schema exposes a types object");

    let mut missing: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for open in ["Json<", "Query<"] {
        let mut rest = source;
        while let Some(at) = rest.find(open) {
            rest = &rest[at + open.len()..];
            let Some(end) = rest.find('>') else { break };
            let name = rest[..end].trim().to_string();
            // Simple identifiers only: a generic or qualified extractor payload
            // (`Json<serde_json::Value>`) is not a DTO type of ours.
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || !name.starts_with(|c: char| c.is_ascii_uppercase())
            {
                continue;
            }
            if !seen.contains(&name) {
                seen.push(name.clone());
                if !registered.contains_key(&name) && !UNREPRESENTABLE.contains(&name.as_str()) {
                    missing.push(name);
                }
            }
        }
    }

    assert!(
        seen.len() >= 20,
        "found only {} extractor DTOs; the scan has stopped matching the \
         handler signatures and would pass vacuously: {seen:?}",
        seen.len()
    );
    assert!(
        missing.is_empty(),
        "these HTTP request DTOs are not in the published control schema: \
         {missing:?}\nAdd each to the `add!` list in this file (deriving \
         schemars::JsonSchema on it), then regenerate the snapshot with \
         THEGN_UPDATE_SNAPSHOTS=1 cargo test -p thegn-svc --test control_schema"
    );
}
