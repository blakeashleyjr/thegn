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
//! error, never a hang. Request ids use a checked increment, and calls never
//! replace pending waiters; exhaustion or collision refuses the call.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

use thegn_core::plugin_api::{
    PROVIDER_CALL_METHOD, RpcError, RpcErrorCode, RpcMessage, RpcResponse,
};

use super::session::{MAX_QUEUED_FRAMES, SessionFailure, SessionWriter};

/// The correlation half: pending requests waiting for their `RpcResponse`.
pub struct ProviderBridge {
    writer: SessionWriter,
    timeout: Duration,
    pending: Mutex<Pending>,
    counters: Counters,
}

/// Cumulative counters for one bridge (one plugin generation; a restart builds
/// a new bridge, so they restart at zero).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeStats {
    /// Calls admitted into the pending table.
    pub calls: u64,
    /// Calls that hit the plugin's reply deadline.
    pub timed_out: u64,
    /// Calls whose future was dropped before a terminal outcome (caller cancellation).
    pub cancelled: u64,
    /// Replies that matched no waiter (late after timeout or cancellation, duplicate, or unknown id).
    pub late_replies: u64,
    /// Replies carrying neither or both of `result` and `error`.
    pub malformed: u64,
    /// Pending calls woken with a closed-session failure.
    pub closed_wakeups: u64,
}

#[derive(Default)]
struct Counters {
    calls: AtomicU64,
    timed_out: AtomicU64,
    cancelled: AtomicU64,
    late_replies: AtomicU64,
    malformed: AtomicU64,
    closed_wakeups: AtomicU64,
}

struct Pending {
    closed: Option<SessionFailure>,
    calls: HashMap<u64, oneshot::Sender<Result<RpcResponse, SessionFailure>>>,
}

struct PendingGuard<'a> {
    bridge: &'a ProviderBridge,
    id: u64,
    finished: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.bridge
                .counters
                .cancelled
                .fetch_add(1, Ordering::Relaxed);
        }
        drop(
            self.bridge
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .calls
                .remove(&self.id),
        );
    }
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
            pending: Mutex::new(Pending {
                closed: None,
                calls: HashMap::new(),
            }),
            counters: Counters::default(),
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
        let woken = pending.len();
        if woken > 0 {
            self.counters
                .closed_wakeups
                .fetch_add(woken as u64, Ordering::Relaxed);
        }
        for (_, sender) in pending {
            drop(sender.send(Err(error)));
        }
    }

    /// Read the cumulative counters for this bridge generation without locking
    /// the pending table or doing I/O.
    pub fn stats(&self) -> BridgeStats {
        BridgeStats {
            calls: self.counters.calls.load(Ordering::Relaxed),
            timed_out: self.counters.timed_out.load(Ordering::Relaxed),
            cancelled: self.counters.cancelled.load(Ordering::Relaxed),
            late_replies: self.counters.late_replies.load(Ordering::Relaxed),
            malformed: self.counters.malformed.load(Ordering::Relaxed),
            closed_wakeups: self.counters.closed_wakeups.load(Ordering::Relaxed),
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
            Some(tx) => {
                if tx.send(Ok(resp)).is_err() {
                    self.counters.late_replies.fetch_add(1, Ordering::Relaxed);
                    false
                } else {
                    true
                }
            }
            None => {
                self.counters.late_replies.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// One non-blocking `provider.call` round-trip. Must be awaited on a Tokio
    /// runtime with time enabled (`enable_all`). Dropping this future forgets
    /// its pending id; any late reply is discarded by `resolve`.
    pub async fn call(
        &self,
        seam: &str,
        op: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, BridgeError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let Ok(id) = self.writer.next_request_id() else {
            return Err(BridgeError::Transport(
                "provider request ids are exhausted".into(),
            ));
        };
        let (tx, rx) = oneshot::channel();
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
            match pending.calls.entry(id) {
                Entry::Occupied(_) => {
                    return Err(BridgeError::Transport(format!(
                        "provider request id {id} is still pending"
                    )));
                }
                Entry::Vacant(slot) => {
                    slot.insert(tx);
                    self.counters.calls.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let mut pending_guard = PendingGuard {
            bridge: self,
            id,
            finished: false,
        };
        let msg = RpcMessage {
            id: Some(id),
            method: PROVIDER_CALL_METHOD.to_string(),
            params: serde_json::json!({ "seam": seam, "op": op, "args": args }),
        };
        let sent = self.writer.send_json(&msg);
        if let Err(e) = sent {
            pending_guard.finished = true;
            return Err(BridgeError::Transport(e.to_string()));
        }
        let outcome = tokio::time::timeout_at(deadline, rx).await;
        pending_guard.finished = true;
        match outcome {
            Ok(Ok(Ok(resp))) => {
                if resp.result.is_some() == resp.error.is_some() {
                    self.counters.malformed.fetch_add(1, Ordering::Relaxed);
                }
                classify(resp, seam, op)
            }
            Ok(Ok(Err(error))) => Err(BridgeError::Transport(error.to_string())),
            Ok(Err(_)) => Err(BridgeError::Transport("bridge dropped".into())),
            Err(_) => {
                self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
                Err(BridgeError::Transport(format!(
                    "no reply to {seam}.{op} within {:?}",
                    self.timeout
                )))
            }
        }
    }
}

fn classify(resp: RpcResponse, seam: &str, op: &str) -> Result<serde_json::Value, BridgeError> {
    match (resp.result, resp.error) {
        (Some(_), Some(_)) => Err(BridgeError::Transport(format!(
            "plugin reply to {seam}.{op} carried both result and error"
        ))),
        (None, Some(error)) => Err(BridgeError::Rpc(error)),
        (Some(value), None) => Ok(value),
        (None, None) => Err(BridgeError::Transport(format!(
            "plugin reply to {seam}.{op} carried neither result nor error"
        ))),
    }
}

fn require_mutation_success(op: &'static str, value: serde_json::Value) -> Result<(), IssueError> {
    if value.is_null() {
        Err(IssueError::Api(format!(
            "plugin {op} reply has no success result"
        )))
    } else {
        Ok(())
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
    provider_id: Arc<str>,
}

impl PluginIssueBackend {
    /// The adapter owns its immutable provider identity so router rebuilds can
    /// release an old generation when the adapter is dropped.
    pub fn new(
        bridge: Arc<ProviderBridge>,
        plugin_id: &str,
        caps: IssueCaps,
    ) -> Result<Self, IssueError> {
        crate::issue::identity::plugin_namespace(plugin_id).map_err(IssueError::Parse)?;
        let provider_id: Arc<str> = Arc::from(format!("plugin:{plugin_id}"));
        Ok(Self {
            bridge,
            caps,
            provider_id,
        })
    }

    async fn op<T: serde::de::DeserializeOwned>(
        &self,
        op: &'static str,
        args: serde_json::Value,
    ) -> Result<T, IssueError> {
        let out = self.bridge.call("issues", op, args).await.map_err(|e| {
            if e.is_unsupported() {
                IssueError::unsupported(op)
            } else {
                IssueError::Api(e.to_string())
            }
        })?;
        serde_json::from_value(out).map_err(|e| IssueError::Api(format!("bad {op} reply: {e}")))
    }

    fn checked_input<'a>(&self, id: &'a str) -> Result<&'a str, IssueError> {
        let prefix = format!("{}:", self.provider_id.as_ref());
        let key = id.strip_prefix(&prefix).ok_or_else(|| {
            IssueError::Parse("plugin issue id does not match the exact plugin namespace".into())
        })?;
        crate::issue::identity::plugin_key(key).map_err(IssueError::Parse)
    }

    fn validate_issue(&self, issue: Issue) -> Result<Issue, IssueError> {
        let key = self.checked_input(&issue.id)?;
        crate::issue::identity::plugin_key(key).map_err(IssueError::Parse)?;
        if issue.provider != self.provider_id.as_ref() {
            return Err(IssueError::Parse(
                "plugin issue provider does not match its bridge".into(),
            ));
        }
        crate::issue::validate_issue_identity(&issue)?;
        Ok(issue)
    }

    fn validate_detail(&self, mut detail: IssueDetail) -> Result<IssueDetail, IssueError> {
        detail.issue = self.validate_issue(detail.issue)?;
        Ok(detail)
    }
}

impl IssueBackend for PluginIssueBackend {
    fn provider_id(&self) -> &str {
        self.provider_id.as_ref()
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
            let rows: Vec<Issue> = self.op("list_issues", args).await?;
            rows.into_iter().map(|i| self.validate_issue(i)).collect()
        })
    }

    fn get_issue<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<IssueDetail, IssueError>> {
        Box::pin(async move {
            self.checked_input(id)?;
            self.validate_detail(
                self.op("get_issue", serde_json::json!({ "id": id }))
                    .await?,
            )
        })
    }

    fn create_issue<'a>(
        &'a self,
        draft: &'a IssueDraft,
    ) -> BoxFuture<'a, Result<Issue, IssueError>> {
        Box::pin(async move {
            let args = serde_json::to_value(draft).unwrap_or_default();
            self.validate_issue(self.op("create_issue", args).await?)
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
            self.validate_issue(self.op("update_issue", args).await?)
        })
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Issue>, IssueError>> {
        Box::pin(async move {
            let rows: Vec<Issue> = self
                .op(
                    "search",
                    serde_json::json!({ "query": query, "limit": limit }),
                )
                .await?;
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
            require_mutation_success(
                "add_comment",
                self.op::<serde_json::Value>(
                    "add_comment",
                    serde_json::json!({ "id": id, "body": body }),
                )
                .await?,
            )
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
            require_mutation_success(
                "attach_label",
                self.op::<serde_json::Value>(
                    "attach_label",
                    serde_json::json!({ "id": id, "label": label }),
                )
                .await?,
            )
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
            require_mutation_success(
                "detach_label",
                self.op::<serde_json::Value>(
                    "detach_label",
                    serde_json::json!({ "id": id, "label": label }),
                )
                .await?,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::issue::{IssueBackend, IssueRouter};
    use crate::plugin::session::tests::FixtureSupervisor;
    use crate::plugin::session::{ResidentSession, SessionEvent, SessionWriter};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use thegn_core::config::IssuesConfig;

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
    *'"op":"update_issue"'*)
      printf '{"id":%s,"result":{"id":"plugin:demo:1","number":"1","provider":"plugin:demo","title":"updated by plugin","status":"todo","priority":"low","url":"","updated_at_ms":0}}\n' "$id" ;;
    *)
      printf '{"id":%s,"result":null}\n' "$id" ;;
  esac
done
"#;

    #[test]
    fn reply_shape_requires_exactly_one_of_result_or_error() {
        let success = RpcResponse {
            id: 1,
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        assert_eq!(
            classify(success, "issues", "get_issue"),
            Ok(serde_json::json!({"ok": true}))
        );

        let rpc_error = RpcError::new(RpcErrorCode::Unsupported, "unsupported");
        let failure = RpcResponse {
            id: 2,
            result: None,
            error: Some(rpc_error.clone()),
        };
        assert_eq!(
            classify(failure, "issues", "get_issue"),
            Err(BridgeError::Rpc(rpc_error))
        );

        for (response, expected) in [
            (
                RpcResponse {
                    id: 3,
                    result: Some(serde_json::Value::Null),
                    error: Some(RpcError::new(RpcErrorCode::Other, "ignored")),
                },
                "both result and error",
            ),
            (
                RpcResponse {
                    id: 4,
                    result: None,
                    error: None,
                },
                "neither result nor error",
            ),
        ] {
            assert!(matches!(
                classify(response, "issues", "get_issue"),
                Err(BridgeError::Transport(message)) if message.contains(expected)
            ));
        }
        assert!(matches!(
            require_mutation_success("add_comment", serde_json::Value::Null),
            Err(IssueError::Api(message)) if message == "plugin add_comment reply has no success result"
        ));
        assert!(require_mutation_success("add_comment", serde_json::json!(true)).is_ok());
    }

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
    fn provider_and_app_requests_never_share_an_id() {
        let (_fixture, session, bridge) = live_bridge();
        let writer = session.writer();
        let app_id = writer
            .request(
                thegn_core::plugin_api::PluginCallback::OnEvent,
                serde_json::json!({"event":"save"}),
            )
            .unwrap();
        assert_eq!(app_id, 1_000_000);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(bridge.call("issues", "get_issue", serde_json::Value::Null));

        assert!(result.is_ok());
        assert_eq!(writer.next_request_peek(), app_id + 2);
    }

    #[test]
    fn plugin_detail_keeps_partial_comments_marker() {
        let (_fixture, _session, bridge) = live_bridge();
        let backend = PluginIssueBackend::new(bridge, "demo", IssueCaps::default()).unwrap();
        let detail = IssueDetail {
            issue: Issue {
                id: "plugin:demo:7".into(),
                number: "7".into(),
                provider: "plugin:demo".into(),
                ..Default::default()
            },
            comments: vec![],
            comments_complete: false,
        };

        let validated = backend.validate_detail(detail).unwrap();
        assert!(!validated.comments_complete);
    }

    #[test]
    fn plugin_detail_get_issue_keeps_partial_comments_marker_from_json() {
        let script = r#"
while read -r line; do
  id=${line#*'"id":'}; id=${id%%,*}
  printf '{"id":%s,"result":{"id":"plugin:demo:7","number":"7","provider":"plugin:demo","title":"partial issue","status":"todo","priority":"low","url":"","updated_at_ms":0,"comments":[],"comments_complete":false}}\n' "$id"
done
"#;
        let bridge_slot: Arc<Mutex<Option<Arc<ProviderBridge>>>> = Arc::new(Mutex::new(None));
        let route = bridge_slot.clone();
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(&sh(script), &BTreeMap::new(), None, move |event| {
                if let SessionEvent::Response(response) = event
                    && let Some(bridge) = route.lock().unwrap().as_ref()
                {
                    bridge.resolve(response);
                }
            })
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(2));
        *bridge_slot.lock().unwrap() = Some(bridge.clone());
        let backend = PluginIssueBackend::new(bridge, "demo", IssueCaps::default()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let detail = rt.block_on(backend.get_issue("plugin:demo:7")).unwrap();
        assert!(!detail.comments_complete);
    }

    #[test]
    fn mutation_with_null_or_missing_result_is_not_success() {
        let script = r#"
while read -r line; do
  id=${line#*'"id":'}; id=${id%%,*}
  case "$line" in
    *'"op":"add_comment"'*) printf '{"id":%s,"result":null}\n' "$id" ;;
    *'"op":"attach_label"'*) printf '{"id":%s}\n' "$id" ;;
    *'"op":"detach_label"'*) printf '{"id":%s,"result":true}\n' "$id" ;;
  esac
done
"#;
        let bridge_slot: Arc<Mutex<Option<Arc<ProviderBridge>>>> = Arc::new(Mutex::new(None));
        let route = bridge_slot.clone();
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(&sh(script), &BTreeMap::new(), None, move |event| {
                if let SessionEvent::Response(response) = event
                    && let Some(bridge) = route.lock().unwrap().as_ref()
                {
                    bridge.resolve(response);
                }
            })
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(2));
        *bridge_slot.lock().unwrap() = Some(bridge);
        let backend = PluginIssueBackend::new(
            bridge_slot.lock().unwrap().as_ref().unwrap().clone(),
            "demo",
            IssueCaps {
                comments: true,
                labels: true,
                ..Default::default()
            },
        )
        .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        for result in [
            rt.block_on(backend.add_comment("plugin:demo:1", "comment")),
            rt.block_on(backend.attach_label("plugin:demo:1", "label")),
        ] {
            assert!(matches!(result, Err(IssueError::Api(_))));
        }
        assert!(
            rt.block_on(backend.detach_label("plugin:demo:1", "label"))
                .is_ok()
        );
        session.kill();
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
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(backend.provider_id(), "plugin:demo");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let issues = rt
            .block_on(backend.list_issues(&IssueFilter::my_open(10)))
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].title, "from plugin");
        assert_eq!(issues[0].provider, "plugin:demo");
        // An unsupported op surfaces as a classified error.
        let before = bridge.writer.next_request_peek();
        let err = rt
            .block_on(backend.add_comment("plugin:demo:1", "hi"))
            .unwrap_err();
        let IssueError::Unsupported(op) = &err else {
            panic!("{err:?}")
        };
        assert_eq!(*op, "add_comment");
        // comments=true means the request reached the plugin, which then
        // exercised the second (upstream unsupported) degradation boundary.
        assert_eq!(bridge.writer.next_request_peek(), before + 1);
    }

    #[test]
    fn omitted_caps_refuse_optional_ops_without_a_bridge_round_trip() {
        let (_fixture, _session, bridge) = live_bridge();
        let backend =
            PluginIssueBackend::new(bridge.clone(), "legacy", IssueCaps::default()).unwrap();
        let before = bridge.writer.next_request_peek();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(backend.add_comment("plugin:legacy:1", "hi"))
            .unwrap_err();
        assert!(matches!(err, IssueError::Unsupported("add_comment")));
        // Omitted caps are the old-manifest all-false default; the adapter
        // must reject locally before allocating a provider request id.
        assert_eq!(bridge.writer.next_request_peek(), before);
    }

    #[test]
    fn plugin_without_declared_patch_caps_can_update_required_operation() {
        let (_fixture, _session, bridge) = live_bridge();
        let caps = IssueCaps::from_json(&serde_json::Value::Null).unwrap();
        let backend = PluginIssueBackend::new(bridge.clone(), "demo", caps).unwrap();
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        router
            .push_backend("demo".into(), Box::new(backend))
            .unwrap();
        let before = bridge.writer.next_request_peek();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let updated = rt
            .block_on(router.update_issue(
                "plugin:demo:1",
                &IssuePatch {
                    status: Some(thegn_core::issue::IssueStatus::Todo),
                    ..Default::default()
                },
            ))
            .unwrap();
        assert_eq!(updated.title, "updated by plugin");
        assert_eq!(bridge.writer.next_request_peek(), before + 1);
    }

    #[test]
    fn plugin_response_identity_must_match_namespace_and_opaque_fields() {
        let (_fixture, _session, bridge) = live_bridge();
        assert!(PluginIssueBackend::new(bridge.clone(), "bad/id", IssueCaps::default()).is_err());
        let backend = PluginIssueBackend::new(bridge, "demo", IssueCaps::default()).unwrap();
        let valid = thegn_core::issue::Issue {
            id: "plugin:demo:opaque/key#7".into(),
            number: "opaque/key#7".into(),
            provider: "plugin:demo".into(),
            url: "https://plugin.example/issues/opaque/key?view=full#top".into(),
            project_ids: vec!["project / one".into()],
            ..Default::default()
        };
        assert!(backend.validate_issue(valid).is_ok());

        let mut wrong_provider = thegn_core::issue::Issue {
            id: "plugin:other:7".into(),
            number: "7".into(),
            provider: "plugin:other".into(),
            ..Default::default()
        };
        assert!(backend.validate_issue(wrong_provider.clone()).is_err());
        let foreign_provider_id = thegn_core::issue::Issue {
            id: "linear:ABC-1".into(),
            number: "ABC-1".into(),
            provider: "linear".into(),
            ..Default::default()
        };
        assert!(backend.validate_issue(foreign_provider_id).is_err());
        wrong_provider.id = "plugin:demo:7".into();
        wrong_provider.provider = "plugin:demo".into();
        wrong_provider.project_ids = vec!["bad\nproject".into()];
        assert!(backend.validate_issue(wrong_provider).is_err());

        let malformed_namespace = thegn_core::issue::Issue {
            id: "plugin:demo:extra:key".into(),
            number: "key".into(),
            provider: "plugin:demo:extra".into(),
            ..Default::default()
        };
        assert!(crate::issue::validate_issue_identity(&malformed_namespace).is_err());
    }

    #[test]
    fn adapter_identity_churn_is_owned_and_router_replacements_route_by_generation() {
        let bridge = ProviderBridge::new(
            SessionWriter(super::super::session::admission::Shared::new()),
            Duration::from_secs(1),
        );
        let caps = IssueCaps::default();

        // Router rebuilds happen during hydration and plugin reloads. Keep the
        // loop large enough to exercise thousands of generations while using
        // Arc's count as a deterministic release check instead of a flaky RSS
        // threshold.
        for generation in 0..4_096 {
            let plugin_id = format!("generation-{generation}");
            let expected = format!("plugin:{plugin_id}");
            let backend = PluginIssueBackend::new(bridge.clone(), &plugin_id, caps).unwrap();
            let identity = backend.provider_id.clone();
            assert_eq!(Arc::strong_count(&identity), 2);
            assert_eq!(backend.provider_id(), expected);
            assert!(
                backend
                    .checked_input(&format!("{expected}:issue-1"))
                    .is_ok()
            );

            let mut router = IssueRouter::from_config(&IssuesConfig::default());
            router
                .push_backend("reload-generation".into(), Box::new(backend))
                .unwrap();
            assert_eq!(router.provider_id(), expected);
            assert_eq!(router.provider_ids(), vec![expected.as_str()]);
            drop(router);

            // The router was the last adapter owner; the only remaining Arc
            // is this test observer, so the generation's allocation is now
            // reclaimable before the next replacement is built.
            assert_eq!(Arc::strong_count(&identity), 1);
        }
    }

    #[test]
    fn router_replacement_keeps_dynamic_identity_on_async_route() {
        let (_fixture, _session, bridge) = live_bridge();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let mut first = IssueRouter::from_config(&IssuesConfig::default());
        first
            .push_backend(
                "first".into(),
                Box::new(
                    PluginIssueBackend::new(
                        bridge.clone(),
                        "generation-first",
                        IssueCaps::default(),
                    )
                    .unwrap(),
                ),
            )
            .unwrap();
        let first_error = rt
            .block_on(first.get_issue("plugin:generation-first:issue-1"))
            .unwrap_err();
        assert!(matches!(
            first_error,
            IssueError::Api(message) if message.contains("bad get_issue reply")
        ));
        drop(first);

        let mut replacement = IssueRouter::from_config(&IssuesConfig::default());
        replacement
            .push_backend(
                "replacement".into(),
                Box::new(
                    PluginIssueBackend::new(bridge, "generation-replacement", IssueCaps::default())
                        .unwrap(),
                ),
            )
            .unwrap();
        let replacement_error = rt
            .block_on(replacement.get_issue("plugin:generation-replacement:issue-1"))
            .unwrap_err();
        assert!(matches!(
            replacement_error,
            IssueError::Api(message) if message.contains("bad get_issue reply")
        ));
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt
            .block_on(bridge.call("issues", "list_issues", serde_json::json!({})))
            .unwrap_err();
        assert!(matches!(err, BridgeError::Transport(_)), "{err:?}");
        session.kill();
        // A dead session errors on send, not by timeout.
        std::thread::sleep(Duration::from_millis(100));
        let err = rt
            .block_on(bridge.call("issues", "list_issues", serde_json::json!({})))
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(
            rt.block_on(bridge.call("fixture", "last", serde_json::Value::Null))
                .unwrap(),
            serde_json::json!({"final":true})
        );
        let report = fixture.shutdown();
        assert!(report.is_settled(), "{report:?}");
        let start = std::time::Instant::now();
        assert!(
            rt.block_on(bridge.call("fixture", "late", serde_json::Value::Null))
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
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            rt.block_on(bridge.call("fixture", "eof", serde_json::Value::Null))
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(bridge.pending.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn stalled_call_does_not_block_a_current_thread_runtime() {
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(
                &sh("while read -r _; do :; done"),
                &BTreeMap::new(),
                None,
                |_| {},
            )
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_millis(300));
        let beats = Arc::new(AtomicU64::new(0));
        let heartbeat_beats = beats.clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = rt.block_on(async {
            let heartbeat = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    heartbeat_beats.fetch_add(1, Ordering::Relaxed);
                }
            });
            let result = bridge
                .call("issues", "never", serde_json::Value::Null)
                .await;
            heartbeat.abort();
            result
        });
        assert!(matches!(result, Err(BridgeError::Transport(_))));
        assert!(beats.load(Ordering::Relaxed) >= 1);
        assert!(bridge.pending.lock().unwrap().calls.is_empty());
        session.kill();
    }

    #[test]
    fn dropped_call_future_leaves_no_pending_entry() {
        let fixture = FixtureSupervisor::new();
        let session = fixture
            .spawn(
                &sh("while read -r _; do :; done"),
                &BTreeMap::new(),
                None,
                |_| {},
            )
            .unwrap();
        let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(30));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            rt.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(1),
                    bridge.call("issues", "never", serde_json::Value::Null),
                )
                .await
            })
            .is_err()
        );
        assert!(bridge.pending.lock().unwrap().calls.is_empty());
        session.kill();
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
        let (sender, mut receiver) = oneshot::channel();
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
            receiver.blocking_recv().unwrap().unwrap().result,
            Some(serde_json::json!("current"))
        );
    }

    #[test]
    fn colliding_request_id_never_replaces_a_pending_waiter() {
        let bridge = ProviderBridge::new(
            SessionWriter(super::super::session::admission::Shared::new()),
            Duration::from_millis(200),
        );
        let (sender, mut receiver) = oneshot::channel();
        bridge
            .pending
            .lock()
            .unwrap()
            .calls
            .insert(1_000_000, sender);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let result = rt.block_on(bridge.call("issues", "get_issue", serde_json::Value::Null));

        assert!(matches!(
            result,
            Err(BridgeError::Transport(message)) if message.contains("still pending")
        ));
        assert!(
            bridge
                .pending
                .lock()
                .unwrap()
                .calls
                .contains_key(&1_000_000)
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(bridge.resolve(RpcResponse::ok(1_000_000, serde_json::json!("original"))));
        assert!(matches!(
            receiver.try_recv(),
            Ok(Ok(resp)) if resp.result == Some(serde_json::json!("original"))
        ));
    }

    #[test]
    fn exhausted_request_ids_refuse_without_wrapping() {
        let bridge = ProviderBridge::new(
            SessionWriter(super::super::session::admission::Shared::new()),
            Duration::from_millis(200),
        );
        bridge
            .writer
            .0
            .next_request
            .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let result = rt.block_on(bridge.call("issues", "get_issue", serde_json::Value::Null));

        assert!(matches!(
            result,
            Err(BridgeError::Transport(message)) if message.contains("exhausted")
        ));
        assert_eq!(bridge.writer.next_request_peek(), u64::MAX);
        assert!(bridge.pending.lock().unwrap().calls.is_empty());
    }

    fn detached_bridge(
        timeout: Duration,
    ) -> (
        Arc<super::super::session::admission::Shared>,
        Arc<ProviderBridge>,
    ) {
        let shared = super::super::session::admission::Shared::new();
        let bridge = ProviderBridge::new(SessionWriter(shared.clone()), timeout);
        (shared, bridge)
    }

    /// Model a live session writer without a plugin reply. Frames leave the
    /// bounded egress queue after successful writes, while bridge calls stay
    /// pending until a response or close arrives.
    struct DrainFixture {
        stop: Arc<AtomicBool>,
        written: Arc<AtomicUsize>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl DrainFixture {
        fn start(shared: Arc<super::super::session::admission::Shared>) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = stop.clone();
            let written = Arc::new(AtomicUsize::new(0));
            let thread_written = written.clone();
            let thread = std::thread::spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    if let Some(frame) = shared.pop() {
                        shared.written(frame.len());
                        thread_written.fetch_add(1, Ordering::Release);
                    } else {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            });
            Self {
                stop,
                written,
                thread: Some(thread),
            }
        }
    }

    impl Drop for DrainFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("drain fixture thread panicked");
            }
        }
    }

    async fn start_stalled_calls(
        bridge: Arc<ProviderBridge>,
        written: Arc<AtomicUsize>,
        count: usize,
    ) -> Vec<tokio::task::JoinHandle<Result<serde_json::Value, BridgeError>>> {
        let mut calls = Vec::with_capacity(count);
        for started in 1..=count {
            let bridge = bridge.clone();
            calls.push(tokio::spawn(async move {
                bridge
                    .call("issues", "stall", serde_json::Value::Null)
                    .await
            }));
            // Model its complete pipe write before starting another call.
            // The plugin still never replies, so its pending entry remains.
            while written.load(Ordering::Acquire) < started {
                tokio::task::yield_now().await;
            }
        }
        calls
    }

    async fn wait_for_pending(bridge: &ProviderBridge, count: usize) {
        // Detached transport fixtures drain frames on a real writer thread;
        // allow that thread to run between polls instead of busy-yielding.
        for _ in 0..10_000 {
            if bridge.pending.lock().unwrap().calls.len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        panic!("bridge never reached {count} pending calls");
    }

    fn close_now(shared: &super::super::session::admission::Shared) {
        shared.close(
            crate::plugin::session::CloseReason::Requested,
            tokio::time::Instant::now(),
        );
    }

    #[test]
    fn saturated_bridge_refuses_the_extra_call_without_growing_pending() {
        let (shared, bridge) = detached_bridge(Duration::from_secs(30));
        let _drain = DrainFixture::start(shared.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (results, extra, after_close) = rt.block_on(async {
            let calls = async {
                futures_util::future::join_all(
                    start_stalled_calls(bridge.clone(), _drain.written.clone(), MAX_QUEUED_FRAMES)
                        .await,
                )
                .await
                .into_iter()
                .map(|result| result.unwrap())
                .collect::<Vec<_>>()
            };
            let probe = async {
                wait_for_pending(&bridge, MAX_QUEUED_FRAMES).await;
                let extra = bridge
                    .call("issues", "overflow", serde_json::Value::Null)
                    .await;
                assert_eq!(
                    bridge.pending.lock().unwrap().calls.len(),
                    MAX_QUEUED_FRAMES
                );
                close_now(&shared);
                let after_close = tokio::time::timeout(
                    Duration::from_secs(5),
                    bridge.call("issues", "closed", serde_json::Value::Null),
                )
                .await;
                (extra, after_close)
            };
            let (results, (extra, after_close)) = tokio::join!(calls, probe);
            (results, extra, after_close)
        });

        assert!(
            matches!(
                &extra,
                Err(BridgeError::Transport(message)) if message == &SessionFailure::Full.to_string()
            ),
            "unexpected overflow result: {extra:?}"
        );
        assert_eq!(results.len(), MAX_QUEUED_FRAMES);
        assert!(results.iter().all(|result| matches!(
            result,
            Err(BridgeError::Transport(message)) if message == &SessionFailure::Closed.to_string()
        )), "pending calls did not all fail as closed: {results:?}");
        assert!(bridge.pending.lock().unwrap().calls.is_empty());
        assert!(
            matches!(
                &after_close,
                Ok(Err(BridgeError::Transport(message))) if message == &SessionFailure::Closed.to_string()
            ),
            "unexpected post-close result: {after_close:?}"
        );
    }

    #[test]
    fn close_wakes_every_pending_call_across_repeated_generations() {
        const GENERATIONS: usize = 64;
        const CALLS: usize = 8;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        rt.block_on(async {
            for _generation in 0..GENERATIONS {
                let (shared, bridge) = detached_bridge(Duration::from_secs(30));
                let calls = futures_util::future::join_all((0..CALLS).map(|_| {
                    bridge.call("issues", "stall", serde_json::Value::Null)
                }));
                let closer = async {
                    wait_for_pending(&bridge, CALLS).await;
                    close_now(&shared);
                };
                let (results, ()) = tokio::join!(calls, closer);

                assert_eq!(results.len(), CALLS);
                assert!(results.iter().all(|result| matches!(
                    result,
                    Err(BridgeError::Transport(message)) if message == &SessionFailure::Closed.to_string()
                )));
                assert!(bridge.pending.lock().unwrap().calls.is_empty());
                assert_eq!(Arc::strong_count(&bridge), 1);
                assert!(!shared.route_response(&RpcResponse::ok(
                    1_000_000,
                    serde_json::json!("late"),
                )));
                drop(bridge);
                assert!(!shared.route_response(&RpcResponse::ok(
                    1_000_001,
                    serde_json::json!("late"),
                )));
            }
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "64 close generations took {:?}; pending calls should wake before their 30 s deadlines",
            started.elapsed()
        );
    }

    #[test]
    fn independent_provider_bridges_have_independent_pending_capacity() {
        let (shared_a, bridge_a) = detached_bridge(Duration::from_secs(30));
        let (shared_b, bridge_b) = detached_bridge(Duration::from_secs(30));
        let _drain_a = DrainFixture::start(shared_a.clone());
        let _drain_b = DrainFixture::start(shared_b.clone());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (results_a, extra_a, result_b) = rt.block_on(async {
            let calls_a = async {
                futures_util::future::join_all(
                    start_stalled_calls(
                        bridge_a.clone(),
                        _drain_a.written.clone(),
                        MAX_QUEUED_FRAMES,
                    )
                    .await,
                )
                .await
                .into_iter()
                .map(|result| result.unwrap())
                .collect::<Vec<_>>()
            };
            let probe_a = async {
                wait_for_pending(&bridge_a, MAX_QUEUED_FRAMES).await;
                let extra_a = bridge_a
                    .call("issues", "overflow", serde_json::Value::Null)
                    .await;
                assert_eq!(
                    bridge_a.pending.lock().unwrap().calls.len(),
                    MAX_QUEUED_FRAMES
                );

                let calls_b = futures_util::future::join_all(std::iter::once(bridge_b.call(
                    "issues",
                    "admitted",
                    serde_json::Value::Null,
                )));
                let closer_b = async {
                    wait_for_pending(&bridge_b, 1).await;
                    assert_eq!(
                        bridge_a.pending.lock().unwrap().calls.len(),
                        MAX_QUEUED_FRAMES
                    );
                    close_now(&shared_b);
                };
                let (mut results_b, ()) = tokio::join!(calls_b, closer_b);
                close_now(&shared_a);
                (extra_a, results_b.pop().unwrap())
            };
            let (results_a, (extra_a, result_b)) = tokio::join!(calls_a, probe_a);
            (results_a, extra_a, result_b)
        });

        assert!(
            matches!(
                &extra_a,
                Err(BridgeError::Transport(message)) if message == &SessionFailure::Full.to_string()
            ),
            "unexpected saturated bridge result: {extra_a:?}"
        );
        assert!(
            matches!(
                &result_b,
                Err(BridgeError::Transport(message)) if message == &SessionFailure::Closed.to_string()
            ),
            "independent bridge did not admit its call: {result_b:?}"
        );
        assert_eq!(results_a.len(), MAX_QUEUED_FRAMES);
        assert!(results_a.iter().all(|result| matches!(
            result,
            Err(BridgeError::Transport(message)) if message == &SessionFailure::Closed.to_string()
        )), "saturated bridge calls did not all fail as closed: {results_a:?}");
        assert!(bridge_a.pending.lock().unwrap().calls.is_empty());
        assert!(bridge_b.pending.lock().unwrap().calls.is_empty());
    }

    #[test]
    fn concurrent_mixed_ops_keep_replies_with_their_callers() {
        let (_fixture, session, bridge) = live_bridge();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            for _round in 0..3 {
                let results = futures_util::future::join_all((0_usize..24).map(|call| {
                    let op = if call.is_multiple_of(2) {
                        "list_issues"
                    } else {
                        "update_issue"
                    };
                    bridge.call("issues", op, serde_json::json!({}))
                }))
                .await;
                assert_eq!(results.len(), 24);
                for (call, result) in results.into_iter().enumerate() {
                    let result = result.unwrap();
                    if call.is_multiple_of(2) {
                        assert!(result.is_array());
                        assert_eq!(result.as_array().unwrap().len(), 1);
                    } else {
                        assert_eq!(result["title"], "updated by plugin");
                    }
                }
                assert!(bridge.pending.lock().unwrap().calls.is_empty());
            }
        });
        session.kill();
    }

    #[test]
    fn oversized_request_is_refused_as_too_large_without_a_pending_entry() {
        let (_shared, bridge) = detached_bridge(Duration::from_secs(30));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let big = serde_json::Value::String("x".repeat(crate::plugin::proc::MAX_LINE_BYTES));
        let result = rt.block_on(bridge.call("issues", "create_issue", big));
        assert_eq!(
            result,
            Err(BridgeError::Transport(SessionFailure::TooLarge.to_string()))
        );
        assert!(bridge.pending.lock().unwrap().calls.is_empty());
        assert!(
            !bridge.writer.is_closed(),
            "a refused request must not close the session"
        );
    }

    #[test]
    fn oversized_request_refusals_never_consume_the_pending_table() {
        let (_shared, bridge) = detached_bridge(Duration::from_secs(30));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let big = serde_json::Value::String("x".repeat(crate::plugin::proc::MAX_LINE_BYTES));
        for _ in 0..(MAX_QUEUED_FRAMES + 8) {
            let result = rt.block_on(bridge.call("issues", "create_issue", big.clone()));
            assert_eq!(
                result,
                Err(BridgeError::Transport(SessionFailure::TooLarge.to_string())),
                "an oversized request must be TooLarge every time, never Full"
            );
            assert!(bridge.pending.lock().unwrap().calls.is_empty());
        }
    }

    mod bridge_stats {
        use super::*;

        fn routed_bridge(
            script: &str,
            timeout: Duration,
        ) -> (FixtureSupervisor, ResidentSession, Arc<ProviderBridge>) {
            let bridge_slot: Arc<Mutex<Option<Arc<ProviderBridge>>>> = Arc::new(Mutex::new(None));
            let route = Arc::clone(&bridge_slot);
            let fixture = FixtureSupervisor::new();
            let session = fixture
                .spawn(&sh(script), &BTreeMap::new(), None, move |event| {
                    if let SessionEvent::Response(response) = event
                        && let Some(bridge) = route.lock().unwrap().as_ref()
                    {
                        bridge.resolve(response);
                    }
                })
                .unwrap();
            let bridge = ProviderBridge::new(session.writer(), timeout);
            *bridge_slot.lock().unwrap() = Some(Arc::clone(&bridge));
            (fixture, session, bridge)
        }

        #[test]
        fn bridge_stats_count_calls_and_timeouts() {
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
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            assert!(
                rt.block_on(bridge.call("issues", "list_issues", serde_json::json!({})))
                    .is_err()
            );
            assert_eq!(
                bridge.stats(),
                BridgeStats {
                    calls: 1,
                    timed_out: 1,
                    ..Default::default()
                }
            );
            session.kill();
        }

        #[test]
        fn bridge_stats_count_cancelled_call_futures() {
            let fixture = FixtureSupervisor::new();
            let session = fixture
                .spawn(
                    &sh("while read -r _; do :; done"),
                    &BTreeMap::new(),
                    None,
                    |_| {},
                )
                .unwrap();
            let bridge = ProviderBridge::new(session.writer(), Duration::from_secs(30));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            assert!(
                rt.block_on(tokio::time::timeout(
                    Duration::from_millis(300),
                    bridge.call("issues", "list_issues", serde_json::json!({})),
                ))
                .is_err()
            );
            assert_eq!(bridge.stats().calls, 1);
            assert_eq!(bridge.stats().cancelled, 1);
            assert_eq!(bridge.stats().timed_out, 0);
            session.kill();
        }

        #[test]
        fn bridge_stats_count_malformed_replies() {
            let script = r#"read -r a; id=${a#*'"id":'}; id=${id%%,*}; printf '{"id":%s}\n' "$id"; read -r b; id=${b#*'"id":'}; id=${id%%,*}; printf '{"id":%s,"result":1,"error":{"code":"other","message":"x"}}\n' "$id"; while read -r _; do :; done"#;
            let (_fixture, _session, bridge) = routed_bridge(script, Duration::from_secs(5));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            for _ in 0..2 {
                assert!(matches!(
                    rt.block_on(bridge.call("issues", "list_issues", serde_json::json!({}))),
                    Err(BridgeError::Transport(_))
                ));
            }
            assert_eq!(bridge.stats().calls, 2);
            assert_eq!(bridge.stats().malformed, 2);
        }

        #[test]
        fn bridge_stats_count_late_and_unknown_replies() {
            let (_fixture, _session, bridge) = live_bridge();
            assert!(!bridge.resolve(RpcResponse::ok(424242, serde_json::Value::Null)));
            assert_eq!(bridge.stats().late_replies, 1);

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert!(
                rt.block_on(bridge.call("issues", "list_issues", serde_json::json!({})))
                    .is_ok()
            );
            assert_eq!(bridge.stats().late_replies, 1);
            assert_eq!(bridge.stats().calls, 1);
            assert_eq!(bridge.stats().malformed, 0);

            let (sender, receiver) = oneshot::channel();
            drop(receiver);
            assert!(
                bridge
                    .pending
                    .lock()
                    .unwrap()
                    .calls
                    .insert(424243, sender)
                    .is_none()
            );
            assert!(!bridge.resolve(RpcResponse::ok(424243, serde_json::Value::Null)));
            assert_eq!(bridge.stats().late_replies, 2);
        }

        #[test]
        fn bridge_stats_count_closed_wakeups() {
            let (_fixture, _session, bridge) =
                routed_bridge("read -r _; exec 1>&-; sleep 30", Duration::from_secs(30));
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            assert!(
                rt.block_on(bridge.call("fixture", "eof", serde_json::Value::Null))
                    .is_err()
            );
            assert_eq!(bridge.stats().closed_wakeups, 1);
            assert_eq!(bridge.stats().timed_out, 0);
        }
    }
}
