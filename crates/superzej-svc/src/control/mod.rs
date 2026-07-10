//! The control-plane service seam: the API a pane daemon exposes and every
//! transport adapts.
//!
//! [`ControlApi`] is implemented once (by the daemon's session table, host
//! side) and adapted thinly: the axum HTTP+WS surface ([`http`]), the tonic
//! gRPC surface (feature `control-grpc`), and the CLI's [`client`]. Auth is
//! NOT this trait's job — adapters resolve the caller's [`auth::AuthCtx`]
//! ([`auth`]) and check [`superzej_core::control::required_scope`] *before*
//! calling in, so a rejected request performs no action.
//!
//! Methods return [`BoxFuture`]s (not native `async fn`) so the trait stays
//! dyn-compatible — adapters hold an `Arc<dyn ControlApi>`.

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use superzej_core::control::Scope;
use superzej_core::control_wire::EventFrame;
use superzej_core::store::LeaseRow;

pub mod auth;
pub mod client;
#[cfg(feature = "control-grpc")]
pub mod grpc;
pub mod http;
#[cfg(test)]
mod tests;

/// One daemon-owned session (= one PTY + emulator). The compositor's tab/pane
/// layout stays client-side; the daemon's registry is flat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    /// Worktree hint (path) when the session was opened for one.
    pub worktree: Option<String>,
    pub program: String,
    pub cwd: Option<String>,
    pub rows: u16,
    pub cols: u16,
    pub created_at_ms: i64,
    pub attached_clients: u32,
    /// Set while a relay lease is keeping this detached session warm.
    pub lease_expires_at: Option<i64>,
}

/// What to run when opening a fresh session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenSpec {
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub rows: u16,
    pub cols: u16,
    /// Worktree this session belongs to (listing/grouping hint).
    #[serde(default)]
    pub worktree: Option<String>,
}

/// How a client attaches. `Observer` never resizes the PTY and never holds the
/// relay lease open (read-mostly thin clients); `Interactive` is the
/// compositor/CLI case — last interactive writer wins resizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachKind {
    Interactive,
    Observer,
}

/// A successful attach: the warm snapshot, then the live per-subscriber frame
/// stream (bounded; a lagging subscriber gets a fresh
/// [`EventFrame::PaneSnapshot`] resync instead of blocking the PTY).
pub struct AttachReply {
    /// [`EventFrame::PaneSnapshot`] of the current screen — apply first.
    pub snapshot: EventFrame,
    /// Live frames from the snapshot's `seq + 1` on (deltas, resyncs, exit is
    /// signaled by the channel closing after a `Lease`/exit event).
    pub frames: tokio::sync::mpsc::Receiver<EventFrame>,
}

/// The preview-browser verb payload — defined now so the contract is stable;
/// v1 always answers [`ControlError::Unimplemented`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserCommand {
    pub session: Option<String>,
    pub action: BrowserAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserAction {
    Navigate { url: String },
    Reload,
    Back,
}

/// One changed file in a worktree (the mobile stage/commit contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitFileStatus {
    pub path: String,
    /// Porcelain-style two-letter code (`"M "`, `" M"`, `"??"`, …).
    pub code: String,
}

/// Why a control call failed. Adapters map these to transport status codes
/// (HTTP 404/403/409/501/500; gRPC NotFound/PermissionDenied/…).
#[derive(Debug)]
pub enum ControlError {
    NotFound(String),
    /// The caller's token lacks the required scope. Produced by adapters (the
    /// trait impl never sees an under-scoped call).
    NoScope {
        need: Scope,
    },
    Conflict(String),
    Unimplemented(&'static str),
    Internal(anyhow::Error),
}

impl std::fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlError::NotFound(what) => write!(f, "not found: {what}"),
            ControlError::NoScope { need } => {
                write!(f, "missing required scope: {}", need.as_str())
            }
            ControlError::Conflict(what) => write!(f, "conflict: {what}"),
            ControlError::Unimplemented(what) => write!(f, "not implemented: {what}"),
            ControlError::Internal(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for ControlError {}

impl From<anyhow::Error> for ControlError {
    fn from(e: anyhow::Error) -> Self {
        ControlError::Internal(e)
    }
}

pub type ControlResult<T> = Result<T, ControlError>;

/// The service trait. One impl (the daemon), many thin adapters.
///
/// Everything here is async-off-the-render-loop by construction: impls run on
/// the daemon's tokio runtime; the compositor only ever consumes results
/// through its mpsc + `TerminalWaker` path.
pub trait ControlApi: Send + Sync + 'static {
    fn list_sessions(&self) -> BoxFuture<'_, ControlResult<Vec<SessionInfo>>>;

    /// Open a fresh session (a PTY running `spec.argv`).
    fn open(&self, spec: OpenSpec) -> BoxFuture<'_, ControlResult<SessionInfo>>;

    /// Warm-attach: registers `client_id` as a subscriber and returns the
    /// current screen snapshot + live stream. Cancels any relay lease.
    fn attach<'a>(
        &'a self,
        client_id: &'a str,
        session: &'a str,
        kind: AttachKind,
        rows: u16,
        cols: u16,
    ) -> BoxFuture<'a, ControlResult<AttachReply>>;

    /// Detach without killing the PTY; the last client out opens a relay lease.
    fn detach<'a>(
        &'a self,
        client_id: &'a str,
        session: &'a str,
    ) -> BoxFuture<'a, ControlResult<()>>;

    fn send_input<'a>(
        &'a self,
        session: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, ControlResult<()>>;

    fn resize<'a>(
        &'a self,
        session: &'a str,
        rows: u16,
        cols: u16,
    ) -> BoxFuture<'a, ControlResult<()>>;

    /// One-shot screen snapshot ([`EventFrame::PaneSnapshot`]) without attaching.
    fn snapshot<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<EventFrame>>;

    /// Kill the session's PTY and drop it from the registry.
    fn kill<'a>(&'a self, session: &'a str) -> BoxFuture<'a, ControlResult<()>>;

    /// Open/focus a worktree in the owning instance (the `szhost open` verb).
    fn open_worktree<'a>(
        &'a self,
        repo: &'a str,
        branch: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<()>>;

    /// Command the preview browser. v1: always `Err(Unimplemented)`.
    fn drive_browser(&self, cmd: BrowserCommand) -> BoxFuture<'_, ControlResult<()>>;

    // Git verbs (the mobile stage/commit contract) — impls route through the
    // GitBackend seam on spawn_blocking; git stays the source of truth.
    fn git_status<'a>(
        &'a self,
        worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<GitFileStatus>>>;

    fn git_stage<'a>(
        &'a self,
        worktree: &'a str,
        paths: &'a [String],
    ) -> BoxFuture<'a, ControlResult<()>>;

    /// Returns the new commit id.
    fn git_commit<'a>(
        &'a self,
        worktree: &'a str,
        message: &'a str,
    ) -> BoxFuture<'a, ControlResult<String>>;

    // Merge-queue verbs — add the worktree's branch / clear / list the queue for
    // the worktree's repo. Impls reuse the host `merge_ops` primitive so behavior
    // matches the CLI and MCP surfaces.
    /// Add the worktree's current branch; returns a status message.
    fn merge_add<'a>(&'a self, worktree: &'a str) -> BoxFuture<'a, ControlResult<String>>;

    /// Clear the queue for the worktree's repo; returns the number removed.
    fn merge_clear<'a>(&'a self, worktree: &'a str) -> BoxFuture<'a, ControlResult<usize>>;

    /// The queue rows for the worktree's repo.
    fn merge_list<'a>(
        &'a self,
        worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<superzej_core::db::MergeQueueRow>>>;

    fn lease_status(&self) -> BoxFuture<'_, ControlResult<Vec<LeaseRow>>>;

    /// The broadcast event feed (activity, lease, pairing, session-list
    /// events). Pane bytes ride attach streams, not this feed.
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<EventFrame>>;

    /// Graceful daemon shutdown (admin).
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}
