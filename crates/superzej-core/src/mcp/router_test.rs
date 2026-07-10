// `McpRouter::new` takes `Arc<Db>` (its production API). `Db` wraps a rusqlite
// `Connection`, which is intentionally `!Sync`, so the lint fires on the test's
// `Arc::new(Db…)` — but the Arc is single-threaded shared ownership here, not a
// cross-thread share, so it's a false positive.
#![allow(clippy::arc_with_non_send_sync)]

use crate::db::Db;
use crate::event_bus::{Event, EventBus};
use crate::mcp::router::McpRouter;
use crate::store::NotificationStore;
use serde_json::json;
use std::sync::Arc;

#[test]
#[allow(clippy::arc_with_non_send_sync)]
fn test_mcp_initialize() {
    let db = Arc::new(Db::open_memory().unwrap());
    let bus = Arc::new(EventBus::new());
    let router = McpRouter::new(db, bus);

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });

    let res = router.handle_request(&req);
    assert_eq!(res["jsonrpc"], "2.0");
    assert_eq!(res["id"], 1);
    assert!(res["result"]["capabilities"]["tools"].is_object());
}

#[test]
#[allow(clippy::arc_with_non_send_sync)]
fn test_mcp_request_human_emits_event() {
    let db = Arc::new(Db::open_memory().unwrap());
    let bus = Arc::new(EventBus::new());
    let router = McpRouter::new(db, bus.clone());
    let rx = bus.subscribe();

    let req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "request_human",
            "arguments": {
                "worktree": "sz/foo",
                "reason": "i need help"
            }
        }
    });

    let res = router.handle_request(&req);
    assert_eq!(res["jsonrpc"], "2.0");

    let event = rx.try_recv().unwrap();
    if let Event::NotificationReceived { notification } = event {
        assert_eq!(
            notification.kind,
            crate::notification::NotificationKind::AgentAttention
        );
        assert_eq!(notification.worktree_path, "sz/foo");
        assert_eq!(notification.message, "i need help");
    } else {
        panic!("Wrong event type emitted: {event:?}");
    }
}

#[test]
#[allow(clippy::arc_with_non_send_sync)]
fn test_mcp_spawn_subtask_queues_dispatch_and_notifies() {
    let db = Arc::new(Db::open_memory().unwrap());
    let bus = Arc::new(EventBus::new());
    let router = McpRouter::new(db.clone(), bus.clone());
    let rx = bus.subscribe();

    let req = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "spawn_subtask", "arguments": { "worktree": "/wt/x", "agent": "pi" } }
    });
    let res = router.handle_request(&req);
    assert_eq!(res["jsonrpc"], "2.0");
    assert!(res["error"].is_null(), "unexpected error: {res}");

    // Publishes a human-attention notification (not a fake AgentDone)...
    let event = rx.try_recv().unwrap();
    assert!(
        matches!(event, Event::NotificationReceived { .. }),
        "expected NotificationReceived, got {event:?}"
    );
    // ...and records a tracked (queued) dispatch row.
    assert!(db.dispatch_for_worktree("/wt/x").unwrap().is_some());
}

/// A canned `HouseMerge` provider so the router tests don't need git/DB state.
struct FakeMerge;
impl crate::mcp::HouseMerge for FakeMerge {
    fn add(&self, worktree: &str) -> Result<String, String> {
        Ok(format!("queued branch for {worktree}"))
    }
    fn clear(&self, _worktree: &str) -> Result<String, String> {
        Ok("Cleared 2 entries from the merge queue.".to_string())
    }
    fn list(&self, _worktree: &str) -> Result<String, String> {
        Ok("Merge queue empty.".to_string())
    }
}

/// A canned `HouseGit` provider — attaching it advertises the git/semantic
/// house tools (incl. `blast_radius`) and passes the provider gate. Its methods
/// are unused by `blast_radius`, which reads the persisted graph via the DB.
struct FakeGit;
impl crate::mcp::HouseGit for FakeGit {
    fn status(&self, _worktree: &str) -> Result<String, String> {
        Ok(String::new())
    }
    fn diff(&self, _worktree: &str) -> Result<String, String> {
        Ok(String::new())
    }
    fn branches(&self, _worktree: &str) -> Result<String, String> {
        Ok(String::new())
    }
    fn semantic_diff(&self, _worktree: &str) -> Result<String, String> {
        Ok(String::new())
    }
}

#[test]
#[allow(clippy::arc_with_non_send_sync)]
fn blast_radius_tool_advertised_and_degrades_without_graph() {
    // A real temp repo with an edited entity, but an EMPTY semantic graph
    // (nothing built it) ⇒ the tool degrades to a clear "unavailable" message
    // rather than erroring, and it is advertised once a git provider is attached.
    let dir = std::env::temp_dir().join(format!("sz-blast-mcp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| {
        assert!(
            crate::util::git_cmd(&dir)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@t.t"]);
    run(&["config", "user.name", "t"]);
    run(&["config", "commit.gpgsign", "false"]);
    let file = dir.join("lib.rs");
    std::fs::write(&file, "fn greet() -> u8 {\n    1\n}\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "init"]);
    std::fs::write(&file, "fn greet() -> u8 {\n    42\n}\n").unwrap();

    let db = Arc::new(Db::open_memory().unwrap());
    let bus = Arc::new(EventBus::new());
    let router =
        McpRouter::new(db, bus).with_git(Arc::new(FakeGit), dir.to_string_lossy().into_owned());

    // Advertised.
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });
    let names = tool_names(&router.handle_request(&list));
    assert!(names.contains(&"blast_radius".to_string()), "{names:?}");

    // Called → clean degradation (no graph built), not an error.
    let req = json!({
        "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "blast_radius", "arguments": {} }
    });
    let res = router.handle_request(&req);
    assert!(res["error"].is_null(), "unexpected error: {res}");
    let text = res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("graph unavailable"), "got: {text}");

    let _ = std::fs::remove_dir_all(&dir);
}

fn tool_names(res: &serde_json::Value) -> Vec<String> {
    res["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
#[allow(clippy::arc_with_non_send_sync)]
fn merge_tools_absent_until_provider_attached() {
    let db = Arc::new(Db::open_memory().unwrap());
    let bus = Arc::new(EventBus::new());
    let list = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} });

    // No merge provider → no merge tools advertised.
    let bare = McpRouter::new(db.clone(), bus.clone());
    let names = tool_names(&bare.handle_request(&list));
    assert!(!names.iter().any(|n| n.starts_with("merge_")), "{names:?}");

    // Attach the provider → all three merge tools advertised.
    let router = McpRouter::new(db, bus).with_merge(Arc::new(FakeMerge), "/wt".to_string());
    let names = tool_names(&router.handle_request(&list));
    for want in ["merge_add", "merge_clear", "merge_list"] {
        assert!(
            names.contains(&want.to_string()),
            "{want} missing from {names:?}"
        );
    }
}

#[test]
#[allow(clippy::arc_with_non_send_sync)]
fn merge_add_dispatches_to_provider() {
    let db = Arc::new(Db::open_memory().unwrap());
    let bus = Arc::new(EventBus::new());
    let router = McpRouter::new(db, bus).with_merge(Arc::new(FakeMerge), "/wt".to_string());

    let req = json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": { "name": "merge_add", "arguments": {} }
    });
    let res = router.handle_request(&req);
    assert!(res["error"].is_null(), "unexpected error: {res}");
    let text = res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("queued branch for /wt"), "got: {text}");

    // With no provider, calling the tool is a clean "not configured" error, not a
    // panic (mirrors the git/forge tools).
    let bare = McpRouter::new(
        Arc::new(Db::open_memory().unwrap()),
        Arc::new(EventBus::new()),
    );
    let res = bare.handle_request(&req);
    assert_eq!(
        res["error"]["code"], -32603,
        "no provider ⇒ not-configured error: {res}"
    );
}
