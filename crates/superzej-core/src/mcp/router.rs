use crate::db::Db;
use crate::mcp::protocol::{JsonRpcError, JsonRpcRequest, JsonRpcResponse};
use serde_json::json;
use std::sync::Arc;

pub struct McpRouter {
    db: Arc<Db>,
    bus: Arc<crate::event_bus::EventBus>,
}

impl McpRouter {
    pub fn new(db: Arc<Db>, bus: Arc<crate::event_bus::EventBus>) -> Self {
        Self { db, bus }
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
        Ok(json!({
            "tools": [
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
            ]
        }))
    }

    fn handle_tools_call(
        &self,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, (i32, String)> {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let args = params.get("arguments").unwrap_or(&serde_json::Value::Null);

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

                self.bus.publish(&crate::event_bus::Event::AgentDone {
                    worktree: wt.to_string(),
                    agent: agent.to_string(),
                    success: true,
                });

                Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Subtask requested for {} with agent {}", wt, agent)
                    }]
                }))
            }
            "request_human" => {
                let wt = args.get("worktree").and_then(|v| v.as_str()).unwrap_or("");
                let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("");

                self.bus.publish(&crate::event_bus::Event::AgentDone {
                    worktree: wt.to_string(),
                    agent: "human_request".to_string(),
                    success: false,
                });

                Ok(json!({
                    "content": [{
                        "type": "text",
                        "text": format!("Human requested. Reason: {}", reason)
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
