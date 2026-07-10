//! `HouseMerge` implementation — exposes superzej's local merge queue to the
//! embedded agent as MCP house tools (`merge_add`/`merge_clear`/`merge_list`).
//! Lives in the host crate (where `integrate`/`merge_driver`/`merge_ops` live),
//! implementing the `superzej_core::mcp::HouseMerge` trait the core `McpRouter`
//! calls — the same inversion `mcp_git.rs` uses for the git/forge tools. Each
//! method opens its own DB handle (like the CLI) and scopes to the connection
//! worktree's repo, so an agent can only touch its own queue.

use std::path::Path;
use std::sync::Arc;
use superzej_core::config::MergeQueueConfig;
use superzej_core::db::Db;

/// Build the full house MCP router (budget/fleet + git/forge, plus the merge
/// tools when the queue is enabled) scoped to `worktree`, and handle one request.
/// Opens its own DB and builds the Arcs on the calling (blocking) thread — `Db`'s
/// `Connection` is `!Send`, so the router can't cross threads. Extracted from the
/// ratchet-pinned `run.rs` ACP dispatcher.
pub fn handle_house_request(
    inner: &serde_json::Value,
    bus: superzej_core::event_bus::EventBus,
    worktree: &str,
    merge_queue: Option<MergeQueueConfig>,
) -> serde_json::Value {
    let id = inner.get("id").cloned().unwrap_or(serde_json::Value::Null);
    match Db::open() {
        Ok(db) => {
            #[allow(clippy::arc_with_non_send_sync)]
            let mut router =
                superzej_core::mcp::router::McpRouter::new(Arc::new(db), Arc::new(bus))
                    .with_git(
                        Arc::new(superzej_svc::mcp_git::HouseGitImpl),
                        worktree.to_string(),
                    )
                    .with_forge(
                        Arc::new(superzej_svc::mcp_git::HouseGitImpl),
                        worktree.to_string(),
                    );
            if let Some(mq) = merge_queue {
                router = router.with_merge(Arc::new(HouseMergeImpl::new(mq)), worktree.to_string());
            }
            router.handle_request(inner)
        }
        Err(e) => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32603, "message": format!("db open: {e}") }
        }),
    }
}

pub struct HouseMergeImpl {
    mq: MergeQueueConfig,
}

impl HouseMergeImpl {
    pub fn new(mq: MergeQueueConfig) -> Self {
        Self { mq }
    }

    fn repo_root(worktree: &str) -> Result<std::path::PathBuf, String> {
        crate::merge_ops::repo_root_of(Path::new(worktree))
            .ok_or_else(|| format!("{worktree}: not inside a git repository"))
    }
}

impl superzej_core::mcp::HouseMerge for HouseMergeImpl {
    fn add(&self, worktree: &str) -> Result<String, String> {
        let db = Db::open().map_err(|e| e.to_string())?;
        crate::merge_ops::enqueue_worktree(&self.mq, &db, Path::new(worktree))
            .map_err(|e| e.to_string())
    }

    fn clear(&self, worktree: &str) -> Result<String, String> {
        let root = Self::repo_root(worktree)?;
        let db = Db::open().map_err(|e| e.to_string())?;
        let n = crate::merge_ops::clear_repo(&db, &root).map_err(|e| e.to_string())?;
        Ok(format!(
            "Cleared {n} entr{} from the merge queue.",
            if n == 1 { "y" } else { "ies" }
        ))
    }

    fn list(&self, worktree: &str) -> Result<String, String> {
        let root = Self::repo_root(worktree)?;
        let db = Db::open().map_err(|e| e.to_string())?;
        let rows = crate::merge_ops::rows_for_repo(&db, &root);
        if rows.is_empty() {
            return Ok("Merge queue empty.".to_string());
        }
        let mut s = String::new();
        for r in &rows {
            let detail = r
                .conflict_paths
                .as_deref()
                .or(r.error_detail.as_deref())
                .map(|d| format!(" — {}", d.replace('\n', ", ")))
                .unwrap_or_default();
            s.push_str(&format!(
                "{} {} → {}{}\n",
                r.status, r.branch, r.target_branch, detail
            ));
        }
        Ok(s)
    }
}
