//! Provider-as-plugin: bridge a seam's operations to a resident plugin over
//! `provider.call` requests.
//!
//! The host owns one [`ProviderBridge`] per resident plugin with provider
//! contributions. A seam adapter (today [`PluginIssueBackend`]) serializes
//! each trait call as `{"id": n, "method": "provider.call", "params":
//! {"seam", "op", "args"}}`, and the plugin answers the id with an
//! [`RpcResponse`]. Responses arrive on the session reader thread and are
//! resolved through [`ProviderBridge::resolve`] (the host's drain routes
//! `SessionEvent::Response` here), waking the waiting call. Every call
//! carries the plugin's own timeout so a stuck plugin degrades to a seam
//! error, never a hang.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use thegn_core::plugin_api::{
    PROVIDER_CALL_METHOD, RpcError, RpcErrorCode, RpcMessage, RpcResponse,
};

use super::session::SessionWriter;

/// The correlation half: pending requests waiting for their `RpcResponse`.
pub struct ProviderBridge {
    writer: SessionWriter,
    timeout: Duration,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::Sender<RpcResponse>>>,
}

/// A failed bridge call, classified like any seam error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    /// The plugin answered an [`RpcError`].
    Rpc(RpcError),
    /// No answer within the plugin's timeout, or the session died.
    Transport(String),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BridgeError::Rpc(e) => write!(f, "plugin answered {:?}: {}", e.code, e.message),
            BridgeError::Transport(e) => write!(f, "plugin transport: {e}"),
        }
    }
}

impl BridgeError {
    /// Whether the plugin declared the operation unsupported (the seam's
    /// optional-op fall-through).
    pub fn is_unsupported(&self) -> bool {
        matches!(self, BridgeError::Rpc(e) if e.code == RpcErrorCode::Unsupported)
    }
}

impl ProviderBridge {
    pub fn new(writer: SessionWriter, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            writer,
            timeout,
            // Provider request ids share the wire with host.call replies (the
            // plugin allocates its own request ids); start high so the two
            // streams cannot collide in logs.
            next_id: AtomicU64::new(1_000_000),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// Route a response from the session reader to its waiting call.
    /// Returns `false` when the id belongs to no pending call (the drain
    /// then logs it as junk).
    pub fn resolve(&self, resp: RpcResponse) -> bool {
        let tx = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(&resp.id)
        };
        match tx {
            // best-effort: the caller may have timed out and gone.
            Some(tx) => tx.send(resp).is_ok(),
            None => false,
        }
    }

    /// One blocking `provider.call` round-trip. Callers run on seam threads
    /// (hydration workers, `BoxFuture` executors) — never the event loop.
    pub fn call(
        &self,
        seam: &str,
        op: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.insert(id, tx);
        }
        let msg = RpcMessage {
            id: Some(id),
            method: PROVIDER_CALL_METHOD.to_string(),
            params: serde_json::json!({ "seam": seam, "op": op, "args": args }),
        };
        let sent = self
            .writer
            .send_raw(&serde_json::to_string(&msg).unwrap_or_default());
        if let Err(e) = sent {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(&id);
            return Err(BridgeError::Transport(e.to_string()));
        }
        let out = rx.recv_timeout(self.timeout);
        // Timed out or hung up: forget the id so a late reply is dropped.
        if out.is_err() {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(&id);
        }
        match out {
            Ok(RpcResponse { error: Some(e), .. }) => Err(BridgeError::Rpc(e)),
            Ok(resp) => Ok(resp.result.unwrap_or(serde_json::Value::Null)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(BridgeError::Transport(format!(
                "no reply to {seam}.{op} within {:?}",
                self.timeout
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(BridgeError::Transport("bridge dropped".into()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Issue seam adapter
// ---------------------------------------------------------------------------

use futures_util::future::BoxFuture;
use thegn_core::issue::{Issue, IssueDetail, IssueDraft, IssueFilter, IssuePatch};

use crate::issue::{IssueBackend, IssueCaps, IssueError};

/// An issue backend implemented by a resident plugin (`ExtensionPoint::
/// IssueProvider`). Every trait op is one `provider.call` with seam
/// `"issues"`; ops the plugin refuses with `unsupported` surface exactly
/// like a built-in provider's absent capability.
pub struct PluginIssueBackend {
    bridge: Arc<ProviderBridge>,
    caps: IssueCaps,
    /// `"plugin:<id>"` — the `Issue.provider` slug and probe id.
    provider_id: &'static str,
}

impl PluginIssueBackend {
    /// `provider_id` is leaked once per plugin (a handful per process): the
    /// seam wants `&'static str` ids and plugins load once per config life.
    pub fn new(bridge: Arc<ProviderBridge>, plugin_id: &str, caps: IssueCaps) -> Self {
        let provider_id: &'static str = Box::leak(format!("plugin:{plugin_id}").into_boxed_str());
        Self {
            bridge,
            caps,
            provider_id,
        }
    }

    fn op<T: serde::de::DeserializeOwned>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> Result<T, IssueError> {
        let out = self.bridge.call("issues", op, args).map_err(|e| {
            if e.is_unsupported() {
                IssueError::unsupported(op)
            } else {
                IssueError::Api(e.to_string())
            }
        })?;
        serde_json::from_value(out).map_err(|e| IssueError::Api(format!("bad {op} reply: {e}")))
    }
}

impl IssueBackend for PluginIssueBackend {
    fn provider_id(&self) -> &'static str {
        self.provider_id
    }

    fn caps(&self) -> IssueCaps {
        self.caps
    }

    fn list_issues<'a>(
        &'a self,
        filter: &'a IssueFilter,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let args = serde_json::to_value(filter).unwrap_or_default();
            self.op("list_issues", args)
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move { self.op("get_issue", serde_json::json!({ "id": id })) })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let args = serde_json::to_value(draft).unwrap_or_default();
            self.op("create_issue", args)
        })
    }

    fn update_issue<'a>(
        &'a self,
        id: &'a str,
        patch: &'a IssuePatch,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let args = serde_json::json!({
                "id": id,
                "patch": serde_json::to_value(patch).unwrap_or_default(),
            });
            self.op("update_issue", args)
        })
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            self.op(
                "search",
                serde_json::json!({ "query": query, "limit": limit }),
            )
        })
    }

    fn add_comment<'a>(
        &'a self,
        id: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        if !self.caps.comments {
            return Box::pin(async { Err(IssueError::unsupported("add_comment")) });
        }
        Box::pin(async move {
            self.op::<serde_json::Value>(
                "add_comment",
                serde_json::json!({ "id": id, "body": body }),
            )
            .map(|_| ())
        })
    }

    fn attach_label<'a>(
        &'a self,
        id: &'a str,
        label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        if !self.caps.labels {
            return Box::pin(async { Err(IssueError::unsupported("attach_label")) });
        }
        Box::pin(async move {
            self.op::<serde_json::Value>(
                "attach_label",
                serde_json::json!({ "id": id, "label": label }),
            )
            .map(|_| ())
        })
    }

    fn detach_label<'a>(
        &'a self,
        id: &'a str,
        label: &'a str,
    ) -> BoxFuture<'a, Result<(), IssueError>> {
        if !self.caps.labels {
            return Box::pin(async { Err(IssueError::unsupported("detach_label")) });
        }
        Box::pin(async move {
            self.op::<serde_json::Value>(
                "detach_label",
                serde_json::json!({ "id": id, "label": label }),
            )
            .map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::session::{ResidentSession, SessionEvent};
    use std::collections::BTreeMap;

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    /// A scripted "issue provider": answers every provider.call line it reads
    /// with a canned reply keyed on the op.
    const FAKE: &str = r#"
while read -r line; do
  # The request serializes `id` first; strip up to it and take the digits.
  id=${line#*'"id":'}; id=${id%%,*}
  case "$line" in
    *'"op":"list_issues"'*)
      printf '{"id":%s,"result":[{"id":"plugin:demo:1","number":"1","provider":"plugin:demo","title":"from plugin","status":"todo","priority":"low","url":"","updated_at_ms":0}]}\n' "$id" ;;
    *'"op":"add_comment"'*)
      printf '{"id":%s,"error":{"code":"unsupported","message":"no comments"}}\n' "$id" ;;
    *)
      printf '{"id":%s,"result":null}\n' "$id" ;;
  esac
done
"#;

    fn live_bridge() -> (ResidentSession, Arc<ProviderBridge>) {
        let bridge_slot: Arc<Mutex<Option<Arc<ProviderBridge>>>> = Arc::new(Mutex::new(None));
        let route = bridge_slot.clone();
        let session = ResidentSession::spawn(&sh(FAKE), &BTreeMap::new(), None, move |ev| {
            if let SessionEvent::Response(resp) = ev
                && let Some(b) = route.lock().unwrap().as_ref()
            {
                b.resolve(resp);
            }
        })
        .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(10));
        *bridge_slot.lock().unwrap() = Some(bridge.clone());
        (session, bridge)
    }

    #[test]
    fn issue_ops_round_trip_through_a_scripted_plugin() {
        let (_session, bridge) = live_bridge();
        let backend = PluginIssueBackend::new(
            bridge,
            "demo",
            IssueCaps {
                comments: true,
                labels: false,
            },
        );
        assert_eq!(backend.provider_id(), "plugin:demo");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let issues = rt
            .block_on(backend.list_issues(&IssueFilter::my_open(10)))
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "from plugin");
        assert_eq!(issues[0].provider, "plugin:demo");
        // An unsupported op surfaces as a classified error.
        let err = rt
            .block_on(backend.add_comment("plugin:demo:1", "hi"))
            .unwrap_err();
        let IssueError::Unsupported(op) = &err else {
            panic!("{err:?}")
        };
        assert_eq!(*op, "add_comment");
    }

    #[test]
    fn timeout_and_dead_session_degrade_to_errors() {
        // A plugin that never answers: the call times out.
        let session = ResidentSession::spawn(
            &sh("while read -r _; do :; done"),
            &BTreeMap::new(),
            None,
            |_| {},
        )
        .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_millis(200));
        let err = bridge
            .call("issues", "list_issues", serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(err, BridgeError::Transport(_)), "{err:?}");
        session.kill();
        // A dead session errors on send, not by timeout.
        std::thread::sleep(Duration::from_millis(100));
        let err = bridge
            .call("issues", "list_issues", serde_json::json!({}))
            .unwrap_err();
        assert!(matches!(err, BridgeError::Transport(_)), "{err:?}");
    }

    #[test]
    fn late_and_unknown_responses_are_reported_unroutable() {
        let (_session, bridge) = live_bridge();
        assert!(!bridge.resolve(RpcResponse::ok(424242, serde_json::Value::Null)));
    }
}
