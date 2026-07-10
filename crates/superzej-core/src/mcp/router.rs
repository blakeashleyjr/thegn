use crate::db::Db;
use crate::mcp::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use crate::store::{CacheStore, NotificationStore, ProxyStore};
use serde_json::json;
use std::sync::Arc;

pub struct McpRouter {
    db: Arc<Db>,
    bus: Arc<crate::event_bus::EventBus>,
    /// Host-injected git/semantic provider (svc-backed). When set, the git house
    /// tools are advertised + serviced against `worktree`.
    git: Option<Arc<dyn crate::mcp::HouseGit>>,
    /// Host-injected forge (PR/CI) + git-write provider. When set, the forge house
    /// tools are advertised + serviced against `worktree`.
    forge: Option<Arc<dyn crate::mcp::HouseForge>>,
    /// Host-injected merge-queue provider. When set, the `merge_add`/`merge_clear`/
    /// `merge_list` house tools are advertised + serviced against `worktree`.
    merge: Option<Arc<dyn crate::mcp::HouseMerge>>,
    /// The connection's worktree (the git/forge tools operate here; the agent
    /// doesn't pass a path, so it can't reach other worktrees).
    worktree: Option<String>,
}

impl McpRouter {
    pub fn new(db: Arc<Db>, bus: Arc<crate::event_bus::EventBus>) -> Self {
        Self {
            db,
            bus,
            git: None,
            forge: None,
            merge: None,
            worktree: None,
        }
    }

    /// Attach the host's git/semantic provider scoped to `worktree`, enabling the
    /// `git_status`/`git_diff`/`git_branches`/`semantic_diff` house tools.
    pub fn with_git(mut self, git: Arc<dyn crate::mcp::HouseGit>, worktree: String) -> Self {
        self.git = Some(git);
        self.worktree = Some(worktree);
        self
    }

    /// Attach the host's forge (PR/CI) + git-write provider scoped to `worktree`,
    /// enabling `pr_status`/`pr_list`/`ci_runs`/`create_branch`/`commit`.
    pub fn with_forge(mut self, forge: Arc<dyn crate::mcp::HouseForge>, worktree: String) -> Self {
        self.forge = Some(forge);
        self.worktree = Some(worktree);
        self
    }

    /// Attach the host's merge-queue provider scoped to `worktree`, enabling the
    /// `merge_add`/`merge_clear`/`merge_list` house tools.
    pub fn with_merge(mut self, merge: Arc<dyn crate::mcp::HouseMerge>, worktree: String) -> Self {
        self.merge = Some(merge);
        self.worktree = Some(worktree);
        self
    }

    pub fn handle_request(&self, req_raw: &serde_json::Value) -> serde_json::Value {
        let req: JsonRpcRequest = match serde_json::from_value(req_raw.clone()) {
            Ok(r) => r,
            Err(e) => {
                return serde_json::to_value(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: serde_json::Value::Null,
                    result: None,
                    error: Some(JsonRpcError {
                        code: -32700,
                        message: format!("Parse error: {}", e),
                        data: None,
                    }),
                })
                .unwrap();
            }
        };

        let id = req.id.clone().unwrap_or(serde_json::Value::Null);

        let result = match req.method.as_str() {
            "initialize" => self.handle_initialize(),
            "tools/list" => self.handle_tools_list(),
            "tools/call" => self.handle_tools_call(&req.params),
            "resources/list" => self.handle_resources_list(),
            "resources/read" => self.handle_resources_read(&req.params),
            _ => {
                return serde_json::to_value(JsonRpcResponse::error(
                    id,
                    -32601,
                    "Method not found",
                ))
                .unwrap();
            }
        };

        match result {
            Ok(res) => serde_json::to_value(JsonRpcResponse::success(id, res)).unwrap(),
            Err((code, msg)) => {
                serde_json::to_value(JsonRpcResponse::error(id, code, &msg)).unwrap()
            }
        }
    }

    fn handle_initialize(&self) -> Result<serde_json::Value, (i32, String)> {
        Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {},
                "resources": {}
            },
            "serverInfo": {
                "name": "superzej-mcp",
                "version": "0.1.0"
            }
        }))
    }

    fn handle_tools_list(&self) -> Result<serde_json::Value, (i32, String)> {
        // The git/semantic house tools take no args — they operate on the
        // connection's worktree — and are advertised only when a provider is
        // attached.
        let mut tools = if self.git.is_some() {
            vec![
                json!({ "name": "git_status", "description": "Working-tree status (staged/unstaged/untracked) for this worktree.",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "git_diff", "description": "Changed files vs HEAD with +/- line counts for this worktree.",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "git_branches", "description": "Local branches in this worktree (current is marked).",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "semantic_diff", "description": "Entity-level (function/struct/...) summary of the diff vs HEAD plus a suggested commit message.",
                    "inputSchema": { "type": "object", "properties": {} } }),
            ]
        } else {
            Vec::new()
        };
        if self.forge.is_some() {
            tools.extend([
                json!({ "name": "pr_status", "description": "Pull-request state for the current branch.",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "pr_list", "description": "Open pull requests in this repo.",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "ci_runs", "description": "Recent CI runs for this repo.",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "create_branch", "description": "Create a git branch off `base` (default HEAD) in this worktree.",
                    "inputSchema": { "type": "object", "properties": { "name": { "type": "string" }, "base": { "type": "string" } }, "required": ["name"] } }),
                json!({ "name": "commit", "description": "Commit staged changes in this worktree with a message.",
                    "inputSchema": { "type": "object", "properties": { "message": { "type": "string" } }, "required": ["message"] } }),
            ]);
        }
        // Merge-queue house tools — no args; scoped to the connection's repo.
        if self.merge.is_some() {
            tools.extend([
                json!({ "name": "merge_add", "description": "Add this worktree's current branch to superzej's local merge queue.",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "merge_clear", "description": "Clear superzej's merge queue for this repo.",
                    "inputSchema": { "type": "object", "properties": {} } }),
                json!({ "name": "merge_list", "description": "Show superzej's merge queue for this repo.",
                    "inputSchema": { "type": "object", "properties": {} } }),
            ]);
        }
        let base = json!([
                {
                    "name": "check_my_budget",
                    "description": "Checks the token/cost budget for the agent's current scope.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "scope": {
                                "type": "string",
                                "description": "The scope to check, e.g. 'worktree:/path/to/repo'"
                            }
                        },
                        "required": ["scope"]
                    }
                },
                {
                    "name": "spawn_subtask",
                    "description": "Spawns a new tab/pane task in the workspace host.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "worktree": {
                                "type": "string",
                                "description": "The ID or path of the worktree"
                            },
                            "agent": {
                                "type": "string",
                                "description": "The agent identity to handle the task"
                            }
                        },
                        "required": ["worktree", "agent"]
                    }
                },
                {
                    "name": "request_human",
                    "description": "Raises an alert asking the human for attention.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "worktree": {
                                "type": "string"
                            },
                            "reason": {
                                "type": "string"
                            }
                        },
                        "required": ["worktree", "reason"]
                    }
                }
        ]);
        if let serde_json::Value::Array(items) = base {
            tools.extend(items);
        }
        Ok(json!({ "tools": tools }))
    }

    /// Dispatch the git/semantic house tools against the connection worktree.
    /// Returns `None` when `name` isn't one of them (caller falls through to the
    /// built-in tools).
    fn git_tool(&self, name: &str) -> Option<Result<serde_json::Value, (i32, String)>> {
        if !matches!(
            name,
            "git_status" | "git_diff" | "git_branches" | "semantic_diff"
        ) {
            return None;
        }
        let (Some(git), Some(wt)) = (self.git.as_ref(), self.worktree.as_deref()) else {
            return Some(Err((-32603, "git provider not configured".to_string())));
        };
        let res = match name {
            "git_status" => git.status(wt),
            "git_diff" => git.diff(wt),
            "git_branches" => git.branches(wt),
            "semantic_diff" => git.semantic_diff(wt),
            _ => unreachable!(),
        };
        Some(match res {
            Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
            Err(e) => Err((-32603, e)),
        })
    }

    /// Dispatch the forge/git-write house tools (these take args). Returns `None`
    /// when `name` isn't one of them.
    fn forge_tool(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Option<Result<serde_json::Value, (i32, String)>> {
        if !matches!(
            name,
            "pr_status" | "pr_list" | "ci_runs" | "create_branch" | "commit"
        ) {
            return None;
        }
        let (Some(forge), Some(wt)) = (self.forge.as_ref(), self.worktree.as_deref()) else {
            return Some(Err((-32603, "forge provider not configured".to_string())));
        };
        let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or_default();
        let res = match name {
            "pr_status" => forge.pr_status(wt),
            "pr_list" => forge.pr_list(wt),
            "ci_runs" => forge.ci_runs(wt),
            "create_branch" => {
                let base = args.get("base").and_then(|v| v.as_str()).unwrap_or("HEAD");
                forge.create_branch(wt, str_arg("name"), base)
            }
            "commit" => forge.commit(wt, str_arg("message")),
            _ => unreachable!(),
        };
        Some(match res {
            Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
            Err(e) => Err((-32603, e)),
        })
    }

    /// Dispatch the merge-queue house tools against the connection worktree/repo.
    /// Returns `None` when `name` isn't one of them.
    fn merge_tool(&self, name: &str) -> Option<Result<serde_json::Value, (i32, String)>> {
        if !matches!(name, "merge_add" | "merge_clear" | "merge_list") {
            return None;
        }
        let (Some(merge), Some(wt)) = (self.merge.as_ref(), self.worktree.as_deref()) else {
            return Some(Err((
                -32603,
                "merge-queue provider not configured".to_string(),
            )));
        };
        let res = match name {
            "merge_add" => merge.add(wt),
            "merge_clear" => merge.clear(wt),
            "merge_list" => merge.list(wt),
            _ => unreachable!(),
        };
        Some(match res {
            Ok(text) => Ok(json!({ "content": [{ "type": "text", "text": text }] })),
            Err(e) => Err((-32603, e)),
        })
    }

    fn handle_tools_call(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)> {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let args = params.get("arguments").unwrap_or(&serde_json::Value::Null);

        // Git/semantic house tools — no args; operate on the connection worktree.
        if let Some(out) = self.git_tool(name) {
            return out;
        }
        // Forge / git-write house tools (take args).
        if let Some(out) = self.forge_tool(name, args) {
            return out;
        }
        // Merge-queue house tools — no args; scoped to the connection's repo.
        if let Some(out) = self.merge_tool(name) {
            return out;
        }

        match name {
            "check_my_budget" => {
                let scope = args
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .unwrap_or("global");
                match self.db.proxy_budget(scope) {
                    Ok(Some(row)) => Ok(json!({
                        "content": [{
                            "type": "text",
                            "text": format!("Budget for {}: Limit {:?}, Used {}", scope, row.limit_cost, row.spent_cost)
                        }]
                    })),
                    Ok(None) => Ok(json!({
                        "content": [{
                            "type": "text",
                            "text": format!("No budget limits set for scope {}", scope)
                        }]
                    })),
                    Err(e) => Err((-32603, format!("DB Error: {}", e))),
                }
            }
            "spawn_subtask" => {
                let wt = args.get("worktree").and_then(|v| v.as_str()).unwrap_or("");
                let agent = args.get("agent").and_then(|v| v.as_str()).unwrap_or("");

                // Record a real, tracked dispatch (status 'queued') + notify the
                // human, who launches it. We deliberately do NOT auto-spawn an agent
                // from an agent's tool call (runaway/recursion risk) — the human (or
                // an orchestrator) is the gate. Replaces the old fake AgentDone.
                let _ = self
                    .db
                    .put_agent_dispatch(&format!("subtask:{wt}"), wt, agent);
                let msg = format!("agent requested a subtask: run `{agent}` in {wt}");
                let _ = self.db.put_notification(
                    crate::notification::NotificationKind::AgentAttention.as_str(),
                    wt,
                    &msg,
                    wt,
                );
                self.bus.publish_with_notification(
                    &crate::event_bus::Event::NotificationReceived {
                        notification: crate::notification::Notification {
                            id: 0,
                            kind: crate::notification::NotificationKind::AgentAttention,
                            source_ref: format!("subtask:{wt}"),
                            message: msg,
                            created_at_ms: crate::util::now(),
                            read: false,
                            worktree_path: wt.to_string(),
                        },
                    },
                );

                Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Subtask queued: `{agent}` in {wt} (awaiting human launch).")
                    }]
                }))
            }
            "request_human" => {
                let wt = args
                    .get("worktree")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or(self.worktree.as_deref())
                    .unwrap_or("");
                let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("");

                // Real attention notification: persist to the inbox + publish for a
                // desktop toast (Alert priority). Not a fake AgentDone.
                let kind = crate::notification::NotificationKind::AgentAttention;
                let _ = self.db.put_notification(kind.as_str(), wt, reason, wt);
                self.bus.publish_with_notification(
                    &crate::event_bus::Event::NotificationReceived {
                        notification: crate::notification::Notification {
                            id: 0,
                            kind,
                            source_ref: wt.to_string(),
                            message: reason.to_string(),
                            created_at_ms: crate::util::now(),
                            read: false,
                            worktree_path: wt.to_string(),
                        },
                    },
                );

                Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Flagged for human attention: {reason}")
                    }]
                }))
            }
            _ => Err((-32601, format!("Tool not found: {}", name))),
        }
    }

    fn handle_resources_list(&self) -> Result<serde_json::Value, (i32, String)> {
        Ok(json!({
            "resources": [
                {
                    "uri": "fleet://status",
                    "name": "Global Fleet Status",
                    "description": "Status of all worktrees and agents"
                },
                {
                    "uri": "worktree://{id}/status",
                    "name": "Worktree Diff Status",
                    "description": "Cached diff and status output for a worktree"
                }
            ]
        }))
    }

    fn handle_resources_read(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)> {
        let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");

        if uri == "fleet://status" {
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": "{ \"status\": \"nominal\" }"
                }]
            }))
        } else if let Some(wt) = uri
            .strip_prefix("worktree://")
            .and_then(|s| s.strip_suffix("/status"))
        {
            match self.db.get_diff_cache(wt) {
                Ok(Some((diff_text, _ts))) => Ok(json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": "text/plain",
                        "text": diff_text
                    }]
                })),
                Ok(None) => Ok(json!({
                    "contents": [{
                        "uri": uri,
                        "mimeType": "text/plain",
                        "text": ""
                    }]
                })),
                Err(e) => Err((-32603, format!("DB Error: {}", e))),
            }
        } else {
            Err((-32602, format!("Resource not found: {}", uri)))
        }
    }
}
