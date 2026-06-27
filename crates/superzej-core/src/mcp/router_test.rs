// `McpRouter::new` takes `Arc<Db>` (its production API). `Db` wraps a rusqlite
// `Connection`, which is intentionally `!Sync`, so the lint fires on the test's
// `Arc::new(Db…)` — but the Arc is single-threaded shared ownership here, not a
// cross-thread share, so it's a false positive.
#![allow(clippy::arc_with_non_send_sync)]

use crate::db::Db;
use crate::event_bus::{Event, EventBus};
use crate::mcp::router::McpRouter;
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
