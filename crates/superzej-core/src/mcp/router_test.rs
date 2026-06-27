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
    if let Event::AgentDone {
        worktree,
        agent,
        success,
    } = event
    {
        assert_eq!(worktree, "sz/foo");
        assert_eq!(agent, "human_request");
        assert!(!success);
    } else {
        panic!("Wrong event type emitted");
    }
}
