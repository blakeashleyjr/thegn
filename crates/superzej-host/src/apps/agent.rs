use serde_json::Value;
use std::sync::Arc;
use std::sync::mpsc;
use superzej_core::db::Db;
use superzej_core::event_bus::EventBus;
use superzej_core::mcp::router::McpRouter;
use sz_kit::{AppTile, InputEvent, InputResult, ratatui::buffer::Buffer, ratatui::prelude::Rect};

pub struct AgentMcpTransport {
    tx: mpsc::Sender<Value>,
}

impl AgentMcpTransport {
    pub fn new(bus: Arc<EventBus>) -> (Self, mpsc::Receiver<Value>) {
        let (req_tx, req_rx) = mpsc::channel::<Value>();
        let (res_tx, res_rx) = mpsc::channel::<Value>();

        std::thread::spawn(move || {
            #[allow(clippy::arc_with_non_send_sync)]
            let db = match Db::open() {
                Ok(d) => std::sync::Arc::new(d),
                Err(_e) => return,
            };
            let router = McpRouter::new(db, bus);

            while let Ok(req_json) = req_rx.recv() {
                let res_json = router.handle_request(&req_json);
                if res_tx.send(res_json).is_err() {
                    break;
                }
            }
        });

        (Self { tx: req_tx }, res_rx)
    }

    pub fn send_message(&self, msg: Value) {
        let _ = self.tx.send(msg);
    }
}

pub struct AgentUi {
    pub mcp_transport: AgentMcpTransport,
}

impl AppTile for AgentUi {
    fn id(&self) -> &'static str {
        "agent"
    }

    fn title(&self) -> String {
        "agent ●".to_string()
    }

    fn wants_redraw(&self) -> bool {
        false
    }

    fn handle_input(&mut self, _event: InputEvent) -> InputResult {
        InputResult::Ignored
    }

    fn render(&mut self, _area: Rect, _buf: &mut Buffer) {}

    fn pump(&mut self) -> bool {
        false
    }
}
