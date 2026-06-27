use crate::acp::transport::AcpTransport;
use anyhow::{Result, anyhow};
use futures_channel::oneshot;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use superzej_core::acp::capabilities::{
    AgentCapabilities, ClientCapabilities, ClientInfo, FsCapabilities, TerminalCapabilities,
};
use superzej_core::acp::methods::{
    InitializeRequest, InitializeResponse, ProvidersSetParams, SessionUpdateEvent,
};
use superzej_core::acp::types::{
    Id, JsonRpcError, JsonRpcMessage, Request, ResponseError, ResponseResult,
};
use tokio::sync::{Mutex, mpsc};

#[derive(Debug, Clone)]
pub enum AcpInbound {
    Initialized(AgentCapabilities),
    SessionUpdate(SessionUpdateEvent),
    TerminalCreateRequest {
        id: Id,
        command: String,
        cwd: Option<String>,
        env: Option<HashMap<String, String>>,
    },
    FsReadRequest {
        id: Id,
        path: String,
    },
    SuperzejEditRequest {
        id: Id,
        path: String,
        edits: Value,
    },
    SuperzejWriteRequest {
        id: Id,
        path: String,
        content: String,
    },
}

/// Map an agent→client JSON-RPC request to the `AcpInbound` the host services.
/// These are the client-serviced ACP methods the `pi` extension calls: shell
/// execution (`terminal/create`), file reads (`fs/read_text_file`), and the
/// bespoke review-pane edit/write methods (`superzej/edit`, `superzej/write`).
/// Unknown methods return `None` (the reader ignores them).
pub fn parse_client_request(req: Request) -> Option<AcpInbound> {
    let params = req.params.unwrap_or(Value::Null);
    let str_field = |key: &str| {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    match req.method.as_str() {
        "terminal/create" => {
            let env = params.get("env").and_then(|v| v.as_object()).map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect::<HashMap<String, String>>()
            });
            let cwd = params
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            Some(AcpInbound::TerminalCreateRequest {
                id: req.id,
                command: str_field("command"),
                cwd,
                env,
            })
        }
        "fs/read_text_file" => Some(AcpInbound::FsReadRequest {
            id: req.id,
            path: str_field("path"),
        }),
        "superzej/edit" => Some(AcpInbound::SuperzejEditRequest {
            id: req.id,
            path: str_field("path"),
            edits: params.get("edits").cloned().unwrap_or(Value::Null),
        }),
        "superzej/write" => Some(AcpInbound::SuperzejWriteRequest {
            id: req.id,
            path: str_field("path"),
            content: str_field("content"),
        }),
        _ => None,
    }
}

pub struct AcpClient {
    next_id: Arc<Mutex<i64>>,
    pending_requests: Arc<Mutex<HashMap<Id, oneshot::Sender<Result<Value>>>>>,
    tx_outbound: mpsc::Sender<JsonRpcMessage>,
}

impl AcpClient {
    pub async fn connect(port: u16) -> Result<(Self, mpsc::Receiver<AcpInbound>)> {
        let addr = format!("127.0.0.1:{}", port);
        let (mut transport_reader, mut transport_writer) = AcpTransport::connect(&addr).await?;

        let (tx_outbound, mut rx_outbound) = mpsc::channel::<JsonRpcMessage>(100);
        let (tx_inbound, rx_inbound) = mpsc::channel::<AcpInbound>(100);

        let pending_requests: Arc<Mutex<HashMap<Id, oneshot::Sender<Result<Value>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_requests_clone = pending_requests.clone();

        // Writer Loop
        tokio::spawn(async move {
            while let Some(msg) = rx_outbound.recv().await {
                let _ = transport_writer.send(&msg).await;
            }
        });

        // Reader Loop
        let tx_inbound_clone = tx_inbound.clone();
        tokio::spawn(async move {
            loop {
                match transport_reader.recv().await {
                    Ok(Some(msg)) => match msg {
                        JsonRpcMessage::ResponseResult(res) => {
                            let mut pending = pending_requests_clone.lock().await;
                            if let Some(tx) = pending.remove(&res.id) {
                                let _ = tx.send(Ok(res.result));
                            }
                        }
                        JsonRpcMessage::ResponseError(err) => {
                            let mut pending = pending_requests_clone.lock().await;
                            if let Some(tx) = pending.remove(&err.id) {
                                let _ = tx.send(Err(anyhow!(
                                    "RPC Error {}: {}",
                                    err.error.code,
                                    err.error.message
                                )));
                            }
                        }
                        JsonRpcMessage::Notification(notif) => {
                            if notif.method == "session/update"
                                && let Some(params) = notif.params
                                && let Ok(update_event) =
                                    serde_json::from_value::<SessionUpdateEvent>(params)
                            {
                                let _ = tx_inbound_clone
                                    .send(AcpInbound::SessionUpdate(update_event))
                                    .await;
                            }
                        }
                        JsonRpcMessage::Request(req) => {
                            if let Some(inbound) = parse_client_request(req) {
                                let _ = tx_inbound_clone.send(inbound).await;
                            }
                        }
                    },
                    Ok(None) => break, // EOF
                    Err(_) => break,   // Read error
                }
            }
        });

        let client = Self {
            next_id: Arc::new(Mutex::new(1)),
            pending_requests,
            tx_outbound,
        };

        Ok((client, rx_inbound))
    }

    /// Send a JSON-RPC request to the agent and await its result. Allocates a
    /// fresh id, registers a oneshot for the reply, and resolves when the reader
    /// loop matches the response.
    async fn request(&self, method: &str, params: Option<Value>) -> Result<Value> {
        let req_id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };
        let req = Request::new(Id::Number(req_id), method, params);

        let (tx, rx) = oneshot::channel();
        self.pending_requests
            .lock()
            .await
            .insert(Id::Number(req_id), tx);

        self.tx_outbound.send(JsonRpcMessage::Request(req)).await?;
        rx.await?
    }

    pub async fn initialize(&self) -> Result<AgentCapabilities> {
        let params = serde_json::to_value(InitializeRequest {
            protocol_version: "1.0".to_string(),
            client_capabilities: ClientCapabilities {
                fs: Some(FsCapabilities {
                    read_text_file: true,
                    write_text_file: true,
                }),
                terminal: Some(TerminalCapabilities {}),
            },
            client_info: ClientInfo {
                name: "superzej".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        })
        .ok();

        let res_value = self.request("initialize", params).await?;
        let init_res: InitializeResponse = serde_json::from_value(res_value)?;
        Ok(init_res.agent_capabilities)
    }

    /// Point the agent's model traffic at a provider (the R↔U bridge): the `pi`
    /// extension's `providers/set` handler registers it and switches the active
    /// model. Used to route through `szproxy` with a per-worktree virtual key
    /// carried in `headers.Authorization`.
    pub async fn set_provider(&self, params: ProvidersSetParams) -> Result<()> {
        self.request("providers/set", serde_json::to_value(params).ok())
            .await?;
        Ok(())
    }

    pub async fn reply_result(&self, id: Id, result: Value) -> Result<()> {
        let msg = JsonRpcMessage::ResponseResult(ResponseResult {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        });
        self.tx_outbound.send(msg).await?;
        Ok(())
    }

    /// Reply to an agent request with a JSON-RPC error so it fails gracefully
    /// instead of waiting forever for a result.
    pub async fn reply_error(&self, id: Id, code: i64, message: &str) -> Result<()> {
        let msg = JsonRpcMessage::ResponseError(ResponseError {
            jsonrpc: "2.0".to_string(),
            id,
            error: JsonRpcError {
                code,
                message: message.to_string(),
                data: None,
            },
        });
        self.tx_outbound.send(msg).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(method: &str, params: serde_json::Value) -> Request {
        Request::new(Id::Number(1), method, Some(params))
    }

    #[test]
    fn parses_terminal_create_with_cwd_and_env() {
        let parsed = parse_client_request(req(
            "terminal/create",
            json!({ "command": "ls -a", "cwd": "/wt", "env": { "FOO": "bar" } }),
        ))
        .expect("terminal/create should parse");
        match parsed {
            AcpInbound::TerminalCreateRequest {
                command, cwd, env, ..
            } => {
                assert_eq!(command, "ls -a");
                assert_eq!(cwd.as_deref(), Some("/wt"));
                assert_eq!(env.unwrap().get("FOO").map(String::as_str), Some("bar"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_fs_read_edit_write_and_ignores_unknown() {
        assert!(matches!(
            parse_client_request(req("fs/read_text_file", json!({ "path": "a.rs" }))),
            Some(AcpInbound::FsReadRequest { .. })
        ));
        assert!(matches!(
            parse_client_request(req(
                "superzej/edit",
                json!({ "path": "a.rs", "edits": [{ "oldText": "x", "newText": "y" }] })
            )),
            Some(AcpInbound::SuperzejEditRequest { .. })
        ));
        assert!(matches!(
            parse_client_request(req(
                "superzej/write",
                json!({ "path": "a.rs", "content": "hi" })
            )),
            Some(AcpInbound::SuperzejWriteRequest { .. })
        ));
        // session/prompt is a client→agent method, not serviced here.
        assert!(parse_client_request(req("session/prompt", json!({ "prompt": "hi" }))).is_none());
    }

    #[test]
    fn providers_set_serializes_camelcase_for_extension() {
        // The pi extension reads `baseUrl`, `apiType`, and `headers.Authorization`.
        let params = ProvidersSetParams {
            id: "szproxy".to_string(),
            base_url: "http://127.0.0.1:8383/v1".to_string(),
            api_type: Some("openai".to_string()),
            headers: Some(json!({ "Authorization": "Bearer szk-abc" })),
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["baseUrl"], "http://127.0.0.1:8383/v1");
        assert_eq!(v["apiType"], "openai");
        assert_eq!(v["headers"]["Authorization"], "Bearer szk-abc");
    }
}
