use serde::Serialize;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use thegn_core::plugin_api::{RpcError, RpcErrorCode, RpcMessage, RpcResponse};
use thegn_core::plugin_app::{AppEventParams, AppEventResult};

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

/// Tracks the host-originating identity. This is not an authorization boundary:
/// native plugins remain trusted local executables, and domain preconditions
/// must still be checked by the application.
#[derive(Default)]
pub struct SessionContext {
    session: Option<String>,
    stopped: bool,
}
impl SessionContext {
    pub fn activate(&mut self, params: &Value) -> anyhow::Result<()> {
        let id = params["session"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .ok_or_else(|| anyhow::anyhow!("activation omitted session identity"))?;
        if self.session.as_deref().is_some_and(|old| old != id) {
            anyhow::bail!("plugin process cannot adopt a different session");
        }
        self.session = Some(id.to_owned());
        self.stopped = false;
        Ok(())
    }
    pub fn accepts(&self, event: &AppEventParams) -> bool {
        !self.stopped && self.session.as_deref() == Some(event.session.as_str())
    }
    pub fn stop(&mut self) {
        self.stopped = true;
    }
    pub fn identity(&self) -> Option<&str> {
        self.session.as_deref()
    }
    /// Durable receipt keys contain the originating session, so a recycled RPC
    /// integer from a new process cannot accidentally deduplicate a new edit.
    pub fn receipt_key(&self, request_id: &Value) -> anyhow::Result<String> {
        let session = self
            .session
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("plugin is inactive"))?;
        if self.stopped || !request_id.is_u64() {
            anyhow::bail!("invalid request identity");
        }
        Ok(format!("{session}:{}", serde_json::to_string(request_id)?))
    }
}

/// Typed `on_event` dispatch with ordinary RPC acknowledgments. The callback
/// must finish local acceptance (including durable receipts) before returning.
/// Notifications never acquire a synthetic acknowledgment or request identity.
pub fn handle_app_request(
    session: &SessionContext,
    request: &RpcMessage,
    handler: impl FnOnce(Option<u64>, &AppEventParams) -> Result<AppEventResult, RpcError>,
) -> Option<RpcResponse> {
    let result = (|| {
        if request.method != "on_event" {
            return Err(RpcError::new(
                RpcErrorCode::Unsupported,
                "unsupported callback",
            ));
        }
        if request.params["kind"] != "App" {
            return Err(RpcError::new(
                RpcErrorCode::Invalid,
                "unsupported event kind",
            ));
        }
        let event: AppEventParams = serde_json::from_value(request.params["payload"].clone())
            .map_err(|_| RpcError::new(RpcErrorCode::Invalid, "invalid app event"))?;
        event
            .validate()
            .map_err(|e| RpcError::new(RpcErrorCode::Invalid, e))?;
        if !session.accepts(&event) {
            return Err(RpcError::new(
                RpcErrorCode::Denied,
                "event belongs to another session",
            ));
        }
        if event.event.reliable() && request.id.is_none() {
            return Err(RpcError::new(
                RpcErrorCode::Invalid,
                "action requires request identity",
            ));
        }
        handler(request.id, &event).and_then(|r| {
            serde_json::to_value(r)
                .map_err(|_| RpcError::new(RpcErrorCode::Other, "cannot encode local result"))
        })
    })();
    request.id.map(|id| match result {
        Ok(result) => RpcResponse::ok(id, result),
        Err(error) => RpcResponse::err(id, error),
    })
}

pub fn success_response(id: Value, result: &impl Serialize) -> anyhow::Result<Value> {
    Ok(json!({"id":id,"result":serde_json::to_value(result)?}))
}
pub fn error_response(id: Value, code: RpcErrorCode, message: &str) -> Value {
    json!({"id":id,"error":{"code":code,"message":message}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sessions_and_stop_are_not_interchangeable() {
        let mut s = SessionContext::default();
        assert!(s.activate(&json!({})).is_err());
        s.activate(&json!({"session":"first"})).unwrap();
        assert!(s.activate(&json!({"session":"second"})).is_err());
        assert_eq!(s.receipt_key(&json!(3)).unwrap(), "first:3");
        s.stop();
        assert!(s.receipt_key(&json!(3)).is_err());
    }
    #[test]
    fn typed_dispatch_rejects_wrong_session_before_handler_and_acks_local_result() {
        let mut session = SessionContext::default();
        session.activate(&json!({"session":"current"})).unwrap();
        let mut request = RpcMessage {
            id: Some(7),
            method: "on_event".into(),
            params: json!({"kind":"App","payload":{"surface":"app.workspace","session":"old","event":"action","data":{"widget":"actions","id":"save"}}}),
        };
        let response = handle_app_request(&session, &request, |_, _| {
            panic!("stale input reached handler")
        })
        .unwrap();
        assert_eq!(response.error.unwrap().code, RpcErrorCode::Denied);
        request.params["payload"]["session"] = json!("current");
        let response = handle_app_request(&session, &request, |id, _| {
            assert_eq!(id, Some(7));
            Ok(AppEventResult::default())
        })
        .unwrap();
        assert!(response.error.is_none());
        request.id = None;
        assert!(
            handle_app_request(&session, &request, |_, _| panic!(
                "unidentified action reached handler"
            ))
            .is_none()
        );
    }
    #[test]
    fn cancellation_is_shared() {
        let cancel = Cancellation::default();
        let worker = cancel.clone();
        cancel.cancel();
        assert!(worker.is_cancelled());
    }
}
