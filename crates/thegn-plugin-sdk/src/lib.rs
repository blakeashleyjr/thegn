//! Terminal-free helpers for native plugins. Stdout is exclusively NDJSON.
mod canonical;
mod framing;
mod local;
mod platform;
#[cfg(test)]
mod platform_ratchet_tests;
mod requests;
mod runtime;
pub use framing::{FrameReader, FrameWriter, MAX_FRAME_BYTES};
pub use local::{
    Browser, CredentialStore, OsBrowser, OsCredentialStore, PrivateState, ProcessLease,
};
pub use requests::{MAX_PENDING_REQUESTS, RequestLimit, RequestTracker};
pub use runtime::{
    Cancellation, SessionContext, error_response, handle_app_request, success_response,
};
pub use thegn_core::plugin_api::{RpcError, RpcErrorCode, RpcMessage, RpcResponse};
pub use thegn_core::plugin_app;
