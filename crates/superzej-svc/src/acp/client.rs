use crate::acp::transport::AcpTransport;
use anyhow::{Result, anyhow};
use futures_channel::oneshot;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use superzej_core::acp::capabilities::{
    AgentCapabilities, ClientCapabilities, ClientInfo, FsCapabilities, TerminalCapabilities,
};
use superzej_core::acp::methods::{InitializeRequest, InitializeResponse, SessionUpdateEvent};
use superzej_core::acp::types::{Id, JsonRpcMessage, Request, ResponseResult};
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
                    Ok(Some(msg)) => {
                        match msg {
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
                                if notif.method == "session/update" {
                                    if let Some(params) = notif.params {
                                        if let Ok(update_event) =
                                            serde_json::from_value::<SessionUpdateEvent>(params)
                                        {
                                            let _ = tx_inbound_clone
                                                .send(AcpInbound::SessionUpdate(update_event))
                                                .await;
                                        }
                                    }
                                }
                            }
                            JsonRpcMessage::Request(req) => {
                                if req.method == "terminal/create" {
                                    // Parse terminal creation request and push to superzej host
                                    if let Some(params) = req.params {
                                        // A real parser would map this perfectly
                                        let command = params
                                            .get("command")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let _ = tx_inbound_clone
                                            .send(AcpInbound::TerminalCreateRequest {
                                                id: req.id,
                                                command,
                                                cwd: None,
                                                env: None,
                                            })
                                            .await;
                                    }
                                } else if req.method == "fs/read_text_file" {
                                    if let Some(params) = req.params {
                                        let path = params
                                            .get("path")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let _ = tx_inbound_clone
                                            .send(AcpInbound::FsReadRequest { id: req.id, path })
                                            .await;
                                    }
                                }
                            }
                        }
                    }
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

    pub async fn initialize(&self) -> Result<AgentCapabilities> {
        let req_id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next += 1;
            id
        };

        let req = Request::new(
            Id::Number(req_id),
            "initialize",
            serde_json::to_value(InitializeRequest {
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
            .ok(),
        );

        let (tx, rx) = oneshot::channel();
        self.pending_requests
            .lock()
            .await
            .insert(Id::Number(req_id), tx);

        self.tx_outbound.send(JsonRpcMessage::Request(req)).await?;

        let res_value = rx.await??;
        let init_res: InitializeResponse = serde_json::from_value(res_value)?;

        Ok(init_res.agent_capabilities)
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
}
