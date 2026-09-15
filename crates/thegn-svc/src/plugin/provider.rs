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

use super::session::{MAX_QUEUED_FRAMES, SessionFailure, SessionWriter};

/// The correlation half: pending requests waiting for their `RpcResponse`.
pub struct ProviderBridge {
    writer: SessionWriter,
    timeout: Duration,
    next_id: AtomicU64,
    pending: Mutex<Pending>,
}

struct Pending {
    closed: Option<SessionFailure>,
    calls: HashMap<u64, mpsc::Sender<Result<RpcResponse, SessionFailure>>>,
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
        let bridge = Arc::new(Self {
            writer,
            timeout,
            // Provider request ids share the wire with host.call replies (the
            // plugin allocates its own request ids); start high so the two
            // streams cannot collide in logs.
            next_id: AtomicU64::new(1_000_000),
            pending: Mutex::new(Pending {
                closed: None,
                calls: HashMap::new(),
            }),
        });
        let closing = Arc::downgrade(&bridge);
        let routing = Arc::downgrade(&bridge);
        if let Err(error) = bridge.writer.bind_provider(
            Box::new(move |error| {
                if let Some(bridge) = closing.upgrade() {
                    bridge.close(error);
                }
            }),
            Arc::new(move |response| {
                routing
                    .upgrade()
                    .is_some_and(|bridge| bridge.resolve(response))
            }),
        ) {
            bridge.close(error);
        }
        bridge
    }

    fn close(&self, error: SessionFailure) {
        let pending = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.closed = Some(error);
            std::mem::take(&mut pending.calls)
        };
        for (_, sender) in pending {
            drop(sender.send(Err(error)));
        }
    }

    /// Route a response from the session reader to its waiting call.
    /// Returns `false` when the id belongs to no pending call (the drain
    /// then logs it as junk).
    pub fn resolve(&self, resp: RpcResponse) -> bool {
        let tx = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.calls.remove(&resp.id)
        };
        match tx {
            // best-effort: the caller may have timed out and gone.
            Some(tx) => tx.send(Ok(resp)).is_ok(),
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
            if let Some(error) = pending.closed {
                return Err(BridgeError::Transport(error.to_string()));
            }
            if self.writer.is_closed() {
                return Err(BridgeError::Transport(SessionFailure::Closed.to_string()));
            }
            if pending.calls.len() >= MAX_QUEUED_FRAMES {
                return Err(BridgeError::Transport(SessionFailure::Full.to_string()));
            }
            pending.calls.insert(id, tx);
        }
        let msg = RpcMessage {
            id: Some(id),
            method: PROVIDER_CALL_METHOD.to_string(),
            params: serde_json::json!({ "seam": seam, "op": op, "args": args }),
        };
        let sent = self.writer.send_json(&msg);
        if let Err(e) = sent {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.calls.remove(&id);
            return Err(BridgeError::Transport(e.to_string()));
        }
        let out = rx.recv_timeout(self.timeout);
        // Timed out or hung up: forget the id so a late reply is dropped.
        if out.is_err() {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.calls.remove(&id);
        }
        match out {
            Ok(Ok(RpcResponse { error: Some(e), .. })) => Err(BridgeError::Rpc(e)),
            Ok(Ok(resp)) => Ok(resp.result.unwrap_or(serde_json::Value::Null)),
            Ok(Err(error)) => Err(BridgeError::Transport(error.to_string())),
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

    fn checked_input<'a>(&self, id: &'a str) -> Result<&'a str, IssueError> {
        let prefix = format!("{}:", self.provider_id);
        let key = id.strip_prefix(&prefix).ok_or_else(|| {
            IssueError::Parse("plugin issue id does not match the exact plugin namespace".into())
        })?;
        crate::issue::identity::plugin_key(key).map_err(IssueError::Parse)
    }

    fn validate_issue(&self, issue: Issue) -> Result<Issue, IssueError> {
        let key = self.checked_input(&issue.id)?;
        crate::issue::identity::plugin_key(key).map_err(IssueError::Parse)?;
        if issue.provider != self.provider_id {
            return Err(IssueError::Parse(
                "plugin issue provider does not match its bridge".into(),
            ));
        }
        Ok(issue)
    }

    fn validate_detail(&self, mut detail: IssueDetail) -> Result<IssueDetail, IssueError> {
        detail.issue = self.validate_issue(detail.issue)?;
        Ok(detail)
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
            let rows: Vec<Issue> = self.op("list_issues", args)?;
            rows.into_iter().map(|i| self.validate_issue(i)).collect()
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            self.checked_input(id)?;
            self.validate_detail(self.op("get_issue", serde_json::json!({ "id": id }))?)
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let args = serde_json::to_value(draft).unwrap_or_default();
            self.validate_issue(self.op("create_issue", args)?)
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
            self.checked_input(id)?;
            self.validate_issue(self.op("update_issue", args)?)
        })
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let rows: Vec<Issue> = self.op(
                "search",
                serde_json::json!({ "query": query, "limit": limit }),
            )?;
            rows.into_iter().map(|i| self.validate_issue(i)).collect()
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
            self.checked_input(id)?;
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
            self.checked_input(id)?;
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
            self.checked_input(id)?;
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
    use crate::plugin::session::tests::FixtureSupervisor;
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

    fn live_bridge() -> (FixtureSupervisor, ResidentSession, Arc<ProviderBridge>) {
        let bridge_slot: Arc<Mutex<Option<Arc<ProviderBridge>>>> = Arc::new(Mutex::new(None));
        let route = bridge_slot.clone();
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(&sh(FAKE), &BTreeMap::new(), None, move |ev| {
                if let SessionEvent::Response(resp) = ev
                    && let Some(b) = route.lock().unwrap().as_ref()
                {
                    b.resolve(resp);
                }
            })
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(10));
        *bridge_slot.lock().unwrap() = Some(bridge.clone());
        (fixture, session, bridge)
    }

    #[test]
    fn issue_ops_round_trip_through_a_scripted_plugin() {
        let (_fixture, _session, bridge) = live_bridge();
        let backend = PluginIssueBackend::new(
            bridge.clone(),
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
        let before = bridge.next_id.load(std::sync::atomic::Ordering::Relaxed);
        let err = rt
            .block_on(backend.add_comment("plugin:demo:1", "hi"))
            .unwrap_err();
        let IssueError::Unsupported(op) = &err else {
            panic!("{err:?}")
        };
        assert_eq!(*op, "add_comment");
        // comments=true means the request reached the plugin, which then
        // exercised the second (upstream unsupported) degradation boundary.
        assert_eq!(
            bridge.next_id.load(std::sync::atomic::Ordering::Relaxed),
            before + 1
        );
    }

    #[test]
    fn omitted_caps_refuse_optional_ops_without_a_bridge_round_trip() {
        let (_fixture, _session, bridge) = live_bridge();
        let backend = PluginIssueBackend::new(bridge.clone(), "legacy", IssueCaps::default());
        let before = bridge.next_id.load(std::sync::atomic::Ordering::Relaxed);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt
            .block_on(backend.add_comment("plugin:legacy:1", "hi"))
            .unwrap_err();
        assert!(matches!(err, IssueError::Unsupported("add_comment")));
        // Omitted caps are the old-manifest all-false default; the adapter
        // must reject locally before allocating a provider request id.
        assert_eq!(
            bridge.next_id.load(std::sync::atomic::Ordering::Relaxed),
            before
        );
    }

    #[test]
    fn timeout_and_dead_session_degrade_to_errors() {
        // A plugin that never answers: the call times out.
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(
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
        let (_fixture, _session, bridge) = live_bridge();
        assert!(!bridge.resolve(RpcResponse::ok(424242, serde_json::Value::Null)));
    }
    #[test]
    fn resident_final_reply_before_exit_is_delivered_and_late_call_is_closed() {
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(
                &sh(r#"read -r _; echo '{"id":1000000,"result":{"final":true}}'"#),
                &BTreeMap::new(),
                None,
                |_| {},
            )
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(10));
        assert_eq!(
            bridge
                .call("fixture", "last", serde_json::Value::Null)
                .unwrap(),
            serde_json::json!({"final":true})
        );
        let report = fixture.shutdown();
        assert!(report.is_settled(), "{report:?}");
        let start = std::time::Instant::now();
        assert!(
            bridge
                .call("fixture", "late", serde_json::Value::Null)
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn resident_eof_fails_pending_calls_without_waiting_for_rpc_deadline() {
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(
                &sh("read -r _; exec 1>&-; sleep 30"),
                &BTreeMap::new(),
                None,
                |_| {},
            )
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(30));
        let start = std::time::Instant::now();
        assert!(
            bridge
                .call("fixture", "eof", serde_json::Value::Null)
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(bridge.pending.lock().unwrap().calls.is_empty());
    }
    #[test]
    fn resident_old_session_cannot_route_into_replacement_bridge() {
        let old = super::super::session::admission::Shared::new();
        let old_writer = SessionWriter(old.clone());
        let old_bridge = ProviderBridge::new(old_writer, Duration::from_secs(1));
        drop(old_bridge);
        let replacement = super::super::session::admission::Shared::new();
        let new_bridge =
            ProviderBridge::new(SessionWriter(replacement.clone()), Duration::from_secs(1));
        let (sender, receiver) = mpsc::channel();
        new_bridge
            .pending
            .lock()
            .unwrap()
            .calls
            .insert(1_000_000, sender);
        assert!(!old.route_response(&RpcResponse::ok(1_000_000, serde_json::json!("stale"))));
        assert!(receiver.try_recv().is_err());
        assert!(
            replacement.route_response(&RpcResponse::ok(1_000_000, serde_json::json!("current")))
        );
        assert_eq!(
            receiver.recv().unwrap().unwrap().result,
            Some(serde_json::json!("current"))
        );
    }
}
