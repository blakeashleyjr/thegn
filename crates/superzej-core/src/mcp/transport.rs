use crate::event_bus::EventBus;
use crate::mcp::router::McpRouter;
use serde_json::Value;
use std::sync::Arc;

pub struct AgentMcpTransport {
    tx: std::sync::mpsc::Sender<Value>,
}

impl AgentMcpTransport {
    pub fn new(bus: Arc<EventBus>) -> (Self, std::sync::mpsc::Receiver<Value>) {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<Value>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<Value>();

        std::thread::spawn(move || {
            #[allow(clippy::arc_with_non_send_sync)]
            let db = match crate::db::Db::open() {
                Ok(d) => Arc::new(d),
                Err(_) => return,
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
