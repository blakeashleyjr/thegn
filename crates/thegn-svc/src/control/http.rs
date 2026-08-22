//! The control API's axum HTTP + WebSocket/SSE surface — a thin adapter over
//! [`ControlApi`] (the same seam the gRPC surface and the CLI client use).
//!
//! Auth: every handler resolves the caller's [`AuthCtx`] and checks
//! [`required_scope`] through one helper (`authed`) *before* touching the
//! API, so an under-scoped request performs no action (the spec's "rejected
//! without performing the action"). On a unix-socket listener with
//! `local_admin`, same-uid peers get implicit admin; on TCP a bearer token is
//! always required. `/health` and `POST /v1/pair` (where the single-use code
//! IS the credential) are the only unauthenticated routes.

use axum::{
    Router,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response, sse},
    routing::{delete, get, post},
};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, Mutex};

use thegn_core::control::{ScopeSet, TokenKind, Verb, required_scope};
use thegn_core::control_wire::{EventFrame, Hello, PROTO_VERSION, PairingState};
use thegn_core::store::ControlStore;

use super::auth::{self, AuthCtx};
use super::{
    AttachKind, BrowserCommand, ControlApi, ControlError, OpenSpec, SplitDir, WaitCondition,
};

/// Shared state for the control router. One instance per listener, so the
/// unix-socket listener can carry `local_admin` while the TCP one never does.
#[derive(Clone)]
pub struct ControlState {
    pub api: Arc<dyn ControlApi>,
    pub store: Arc<Mutex<dyn ControlStore + Send>>,
    /// This listener's peers get implicit admin (unix socket, same uid).
    pub local_admin: bool,
    /// `[serve] require_approval`: redeemed tokens park until approved.
    pub require_approval: bool,
    /// Human-readable server identity for `Hello` frames.
    pub server_label: String,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build the control router bound to `state`.
pub fn router(state: ControlState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/pair", post(pair))
        .route("/v1/me", get(me))
        .route("/v1/sessions", get(list_sessions).post(open_session))
        .route("/v1/sessions/{s}/snapshot", get(snapshot))
        .route("/v1/sessions/{s}/input", post(send_input))
        .route("/v1/sessions/{s}/resize", post(resize))
        .route("/v1/sessions/{s}/wait", post(wait))
        .route("/v1/sessions/{s}/split", post(split))
        .route("/v1/sessions/{s}/detach", post(detach))
        .route("/v1/sessions/{s}/attach", get(attach_ws))
        .route("/v1/sessions/{s}", delete(kill))
        .route("/v1/events", get(events_ws))
        .route("/v1/events/sse", get(events_sse))
        .route("/v1/leases", get(leases))
        .route("/v1/worktrees/open", post(open_worktree))
        .route("/v1/browser", post(browser))
        .route("/v1/git/status", get(git_status))
        .route("/v1/git/stage", post(git_stage))
        .route("/v1/git/commit", post(git_commit))
        .route("/v1/merge/list", get(merge_list))
        .route("/v1/merge/add", post(merge_add))
        .route("/v1/merge/clear", post(merge_clear))
        .route("/v1/calendar/events", get(calendar_events))
        .route("/v1/calendar/clocks", get(calendar_clocks))
        .route(
            "/v1/calendar/sources/{account}/events",
            post(calendar_ingest),
        )
        .route("/v1/pairings", get(list_pairings).post(issue_pairing))
        .route("/v1/pairings/{id}", delete(revoke_pairing))
        .route("/v1/pairings/{id}/approve", post(approve_pairing))
        .with_state(state)
}

fn error_json(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(json!({ "error": message }))).into_response()
}

impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let status = match &self {
            ControlError::NotFound(_) => StatusCode::NOT_FOUND,
            ControlError::NoScope { .. } => StatusCode::FORBIDDEN,
            ControlError::Conflict(_) => StatusCode::CONFLICT,
            ControlError::Unimplemented(_) => StatusCode::NOT_IMPLEMENTED,
            ControlError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        error_json(status, &self.to_string())
    }
}

/// Extract the bearer token (`Authorization: Bearer` or `x-api-key` — the
/// proxy's convention).
fn bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        && let Some(rest) = v.strip_prefix("Bearer ")
    {
        return Some(rest.trim().to_string());
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

/// Authenticate the request and enforce the verb's required scope — the single
/// chokepoint every authenticated handler goes through.
// The Err IS the handler's whole response (a rejection short-circuits the
// request); it's produced once per request, so its size is irrelevant.
#[allow(clippy::result_large_err)]
fn authed(state: &ControlState, headers: &HeaderMap, verb: Verb) -> Result<AuthCtx, Response> {
    let ctx = if state.local_admin {
        AuthCtx::local_admin()
    } else {
        let token = bearer(headers)
            .ok_or_else(|| error_json(StatusCode::UNAUTHORIZED, "missing bearer token"))?;
        let store = state.store.lock().expect("control store lock");
        auth::verify(&*store, &token, now_ms())
            .ok_or_else(|| error_json(StatusCode::UNAUTHORIZED, "invalid or revoked token"))?
    };
    ctx.require(required_scope(verb))
        .map_err(|e| e.into_response())?;
    Ok(ctx)
}

async fn health() -> Response {
    axum::Json(json!({ "ok": true })).into_response()
}

// ── pairing ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PairBody {
    code: String,
    #[serde(default)]
    label: String,
}

/// Unauthenticated by design: possession of the single-use pairing code is the
/// credential. A wrong code neither reveals nor consumes anything.
async fn pair(State(state): State<ControlState>, body: axum::Json<PairBody>) -> Response {
    let minted = {
        let store = state.store.lock().expect("control store lock");
        auth::redeem(
            &*store,
            &body.code,
            &body.label,
            state.require_approval,
            now_ms(),
        )
    };
    match minted {
        Ok(Some(m)) => {
            let approved = m.row.approved_at.is_some();
            // Surface the redeem on the event feed: with `require_approval`
            // the token parks until approved, and without a `Requested` frame
            // the approval UX never learns a device is waiting.
            state.api.publish_pairing(
                &m.row.pairing_id,
                &m.row.label,
                &m.row.scope,
                if approved {
                    PairingState::Approved
                } else {
                    PairingState::Requested
                },
            );
            axum::Json(json!({
                "token": m.token,
                "pairing_id": m.row.pairing_id,
                "scopes": m.row.scope,
                "approved": approved,
            }))
            .into_response()
        }
        Ok(None) => error_json(
            StatusCode::UNAUTHORIZED,
            "invalid, expired, or already-redeemed pairing code",
        ),
        Err(e) => ControlError::Internal(e).into_response(),
    }
}

#[derive(Deserialize)]
struct IssueBody {
    #[serde(default = "default_scope")]
    scope: String,
    #[serde(default)]
    label: String,
    /// Code lifetime; `None` ⇒ 15 minutes.
    ttl_secs: Option<i64>,
}

fn default_scope() -> String {
    "read".into()
}

/// Resolve a caller-controlled `ttl_secs` into an absolute expiry (ms since
/// epoch). ttl is clamped to `[1s, 1 year]` before scaling to ms, and the add
/// saturates — an adversarial or buggy value can never overflow the multiply
/// (a debug panic) or wrap `now + ttl` into a negative, already-expired stamp.
fn expiry_ms(now: i64, ttl_secs: Option<i64>) -> i64 {
    let ttl_ms = ttl_secs.unwrap_or(15 * 60).clamp(1, 60 * 60 * 24 * 365) * 1000;
    now.saturating_add(ttl_ms)
}

async fn issue_pairing(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<IssueBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::IssuePairing) {
        return r;
    }
    let now = now_ms();
    let minted = auth::mint(
        TokenKind::PairingCode,
        ScopeSet::parse(&body.scope),
        &body.label,
        None,
        Some(expiry_ms(now, body.ttl_secs)),
        now,
    );
    let put = {
        let store = state.store.lock().expect("control store lock");
        store.put_pairing(&minted.row)
    };
    match put {
        Ok(()) => axum::Json(json!({
            "pairing_id": minted.row.pairing_id,
            "code": minted.token,
            "scopes": minted.row.scope,
            "expires_at": minted.row.expires_at,
        }))
        .into_response(),
        Err(e) => ControlError::Internal(e).into_response(),
    }
}

async fn list_pairings(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::ListPairings) {
        return r;
    }
    let rows = {
        let store = state.store.lock().expect("control store lock");
        store.pairings()
    };
    match rows {
        Ok(rows) => {
            let out: Vec<_> = rows
                .into_iter()
                .map(|p| {
                    json!({
                        "pairing_id": p.pairing_id,
                        "kind": p.kind,
                        "scopes": p.scope,
                        "label": p.label,
                        "created_at": p.created_at,
                        "expires_at": p.expires_at,
                        "redeemed_at": p.redeemed_at,
                        "approved_at": p.approved_at,
                        "revoked_at": p.revoked_at,
                    })
                })
                .collect();
            axum::Json(json!({ "pairings": out })).into_response()
        }
        Err(e) => ControlError::Internal(e).into_response(),
    }
}

/// Broadcast a pairing lifecycle frame for `pairing_id`, best-effort filling
/// label/scope from the store (the approve/revoke handlers only carry the id).
fn publish_pairing_state(state: &ControlState, pairing_id: &str, ps: PairingState) {
    let row = {
        let store = state.store.lock().expect("control store lock");
        store
            .pairings()
            .ok()
            .and_then(|rows| rows.into_iter().find(|p| p.pairing_id == pairing_id))
    };
    let (label, scope) = row.map(|r| (r.label, r.scope)).unwrap_or_default();
    state.api.publish_pairing(pairing_id, &label, &scope, ps);
}

async fn revoke_pairing(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::RevokePairing) {
        return r;
    }
    let res = {
        let store = state.store.lock().expect("control store lock");
        store.revoke_pairing(&id, now_ms())
    };
    match res {
        Ok(()) => {
            publish_pairing_state(&state, &id, PairingState::Revoked);
            axum::Json(json!({ "revoked": id })).into_response()
        }
        Err(e) => ControlError::Internal(e).into_response(),
    }
}

async fn approve_pairing(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::ApprovePairing) {
        return r;
    }
    let res = {
        let store = state.store.lock().expect("control store lock");
        store.approve_pairing(&id, now_ms())
    };
    match res {
        Ok(()) => {
            publish_pairing_state(&state, &id, PairingState::Approved);
            axum::Json(json!({ "approved": id })).into_response()
        }
        Err(e) => ControlError::Internal(e).into_response(),
    }
}

// ── identity & listing ──────────────────────────────────────────────────────

async fn me(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    match authed(&state, &headers, Verb::Me) {
        Ok(ctx) => axum::Json(json!({
            "pairing_id": ctx.pairing_id,
            "label": ctx.label,
            "scopes": ctx.scopes.to_csv(),
        }))
        .into_response(),
        Err(r) => r,
    }
}

async fn list_sessions(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::ListSessions) {
        return r;
    }
    match state.api.list_sessions().await {
        Ok(sessions) => axum::Json(json!({ "sessions": sessions })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn leases(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::LeaseStatus) {
        return r;
    }
    match state.api.lease_status().await {
        Ok(rows) => {
            let out: Vec<_> = rows
                .into_iter()
                .map(|l| {
                    json!({
                        "lease_id": l.lease_id,
                        "session": l.session_id,
                        "kind": l.kind,
                        "client": l.client_id,
                        "expires_at": l.expires_at,
                    })
                })
                .collect();
            axum::Json(json!({ "leases": out })).into_response()
        }
        Err(e) => e.into_response(),
    }
}

// ── session lifecycle & I/O ─────────────────────────────────────────────────

async fn open_session(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<OpenSpec>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::OpenSession) {
        return r;
    }
    match state.api.open(body.0).await {
        Ok(info) => axum::Json(info).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn snapshot(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Snapshot) {
        return r;
    }
    match state.api.snapshot(&s).await {
        Ok(EventFrame::PaneSnapshot {
            session,
            seq,
            cols,
            rows,
            bytes,
        }) => axum::Json(json!({
            "session": session,
            "seq": seq,
            "cols": cols,
            "rows": rows,
            "ansi_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }))
        .into_response(),
        Ok(_) => ControlError::Internal(anyhow::anyhow!("snapshot returned a non-snapshot frame"))
            .into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct InputBody {
    /// Raw bytes, base64. Exactly one of `b64`/`text` must be present.
    b64: Option<String>,
    text: Option<String>,
    /// Append a carriage return (send-and-run).
    #[serde(default)]
    enter: bool,
}

async fn send_input(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<InputBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::SendInput) {
        return r;
    }
    let mut bytes = match (&body.b64, &body.text) {
        (Some(b64), None) => match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(b) => b,
            Err(_) => return error_json(StatusCode::BAD_REQUEST, "invalid base64"),
        },
        (None, Some(text)) => text.clone().into_bytes(),
        _ => {
            return error_json(
                StatusCode::BAD_REQUEST,
                "exactly one of `b64` or `text` is required",
            );
        }
    };
    if body.enter {
        bytes.push(b'\r');
    }
    match state.api.send_input(&s, bytes).await {
        Ok(()) => axum::Json(json!({ "sent": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct ResizeBody {
    rows: u16,
    cols: u16,
}

async fn resize(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<ResizeBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Resize) {
        return r;
    }
    match state.api.resize(&s, body.rows, body.cols).await {
        Ok(()) => axum::Json(json!({ "resized": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Default program for a `split` with no argv: the daemon's login shell.
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

#[derive(Deserialize)]
struct WaitBody {
    condition: WaitCondition,
    /// Milliseconds before giving up (`matched=false`). Omit to wait forever.
    #[serde(default)]
    timeout_ms: Option<i64>,
}

async fn wait(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<WaitBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Wait) {
        return r;
    }
    match state
        .api
        .wait(&s, body.0.condition, body.0.timeout_ms)
        .await
    {
        Ok(outcome) => axum::Json(outcome).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct SplitBody {
    #[serde(default)]
    dir: SplitDir,
    /// Program for the new pane; a login shell when empty.
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: Vec<(String, String)>,
    #[serde(default)]
    rows: u16,
    #[serde(default)]
    cols: u16,
}

async fn split(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<SplitBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Split) {
        return r;
    }
    let b = body.0;
    // Inherit the target session's worktree, cwd and geometry so the sibling
    // lands in the same project at the same size.
    let target = match state.api.list_sessions().await {
        Ok(list) => list.into_iter().find(|si| si.id == s),
        Err(e) => return e.into_response(),
    };
    let Some(target) = target else {
        return ControlError::NotFound(format!("session {s}")).into_response();
    };
    let argv = if b.argv.is_empty() {
        vec![default_shell()]
    } else {
        b.argv
    };
    let spec = OpenSpec {
        argv,
        cwd: b.cwd.or(target.cwd),
        env: b.env,
        rows: if b.rows == 0 { target.rows } else { b.rows },
        cols: if b.cols == 0 { target.cols } else { b.cols },
        worktree: target.worktree,
    };
    match state.api.split(&s, b.dir, spec).await {
        Ok(info) => axum::Json(info).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct DetachBody {
    client_id: String,
}

async fn detach(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<DetachBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Detach) {
        return r;
    }
    match state.api.detach(&body.client_id, &s).await {
        Ok(()) => axum::Json(json!({ "detached": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn kill(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::KillSession) {
        return r;
    }
    match state.api.kill(&s).await {
        Ok(()) => axum::Json(json!({ "killed": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── worktrees / browser / git ───────────────────────────────────────────────

#[derive(Deserialize)]
struct OpenWorktreeBody {
    repo: String,
    branch: Option<String>,
}

async fn open_worktree(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<OpenWorktreeBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::OpenWorktree) {
        return r;
    }
    match state
        .api
        .open_worktree(&body.repo, body.branch.as_deref())
        .await
    {
        Ok(()) => axum::Json(json!({ "opened": body.repo })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn browser(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<BrowserCommand>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::DriveBrowser) {
        return r;
    }
    match state.api.drive_browser(body.0).await {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct WorktreeQuery {
    worktree: String,
}

async fn git_status(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(q): Query<WorktreeQuery>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::GitStatus) {
        return r;
    }
    match state.api.git_status(&q.worktree).await {
        Ok(files) => axum::Json(json!({ "files": files })).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct StageBody {
    worktree: String,
    paths: Vec<String>,
}

async fn git_stage(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<StageBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::GitStage) {
        return r;
    }
    match state.api.git_stage(&body.worktree, &body.paths).await {
        Ok(()) => axum::Json(json!({ "staged": body.paths.len() })).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
struct CommitBody {
    worktree: String,
    message: String,
}

async fn git_commit(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<CommitBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::GitCommit) {
        return r;
    }
    match state.api.git_commit(&body.worktree, &body.message).await {
        Ok(commit) => axum::Json(json!({ "commit": commit })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── merge queue ───────────────────────────────────────────────────────────────

/// POST body for the merge add/clear verbs — scoped to one worktree's repo.
#[derive(Deserialize)]
struct MergeBody {
    worktree: String,
}

async fn merge_list(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(q): Query<WorktreeQuery>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::MergeList) {
        return r;
    }
    match state.api.merge_list(&q.worktree).await {
        Ok(queue) => axum::Json(json!({ "queue": queue })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── calendar ──────────────────────────────────────────────────────────────────

/// `?from=2026-08-01&to=2026-08-31` — inclusive ISO dates.
#[derive(Deserialize)]
struct CalendarQuery {
    from: String,
    to: String,
}

/// The ingest body: the same `CalEvent` shape a `command` plugin emits, so one
/// contract serves both a polled plugin and a pushing daemon.
#[derive(Deserialize)]
struct CalendarIngestBody {
    events: Vec<thegn_core::calendar::CalEvent>,
}

async fn calendar_events(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(q): Query<CalendarQuery>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::CalendarEvents) {
        return r;
    }
    match state.api.calendar_events(&q.from, &q.to).await {
        Ok(events) => axum::Json(json!({ "events": events })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn calendar_clocks(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::CalendarClocks) {
        return r;
    }
    match state.api.calendar_clocks().await {
        Ok(clocks) => axum::Json(json!({ "clocks": clocks })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn calendar_ingest(
    State(state): State<ControlState>,
    headers: HeaderMap,
    axum::extract::Path(account): axum::extract::Path<String>,
    body: axum::Json<CalendarIngestBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::CalendarIngest) {
        return r;
    }
    match state.api.calendar_ingest(&account, body.0.events).await {
        Ok(stored) => axum::Json(json!({ "stored": stored })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn merge_add(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<MergeBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::MergeAdd) {
        return r;
    }
    match state.api.merge_add(&body.worktree).await {
        Ok(message) => axum::Json(json!({ "queued": true, "message": message })).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn merge_clear(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<MergeBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::MergeClear) {
        return r;
    }
    match state.api.merge_clear(&body.worktree).await {
        Ok(cleared) => axum::Json(json!({ "cleared": cleared })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── streams ─────────────────────────────────────────────────────────────────

fn hello_frame(state: &ControlState, ctx: &AuthCtx) -> EventFrame {
    let mut scopes = Vec::new();
    for s in [
        thegn_core::control::Scope::Read,
        thegn_core::control::Scope::Write,
        thegn_core::control::Scope::Git,
        thegn_core::control::Scope::Admin,
    ] {
        if ctx.scopes.contains(s) {
            scopes.push(s);
        }
    }
    EventFrame::Hello(Hello {
        proto: PROTO_VERSION,
        server: state.server_label.clone(),
        scopes,
    })
}

/// The broadcast event feed over WebSocket: one binary message per encoded
/// [`EventFrame`]. Read scope.
async fn events_ws(
    State(state): State<ControlState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let ctx = match authed(&state, &headers, Verb::Events) {
        Ok(c) => c,
        Err(r) => return r,
    };
    ws.on_upgrade(move |socket| pump_events(socket, state, ctx))
}

async fn pump_events(mut socket: WebSocket, state: ControlState, ctx: AuthCtx) {
    let hello = hello_frame(&state, &ctx);
    if socket
        .send(Message::Binary(hello.encode().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut rx = state.api.subscribe();
    loop {
        match rx.recv().await {
            Ok(frame) => {
                if socket
                    .send(Message::Binary(frame.encode().into()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // Slow consumer skipped `n` events — that's fine for a monitor
            // feed (pane bytes ride attach streams, not this one).
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The same feed as JSON server-sent events (curl-friendly; pane bytes as
/// base64). WS is the primary transport — this is a convenience surface.
async fn events_sse(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Events) {
        return r;
    }
    let rx = state.api.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let ev = sse::Event::default().data(frame_json(&frame).to_string());
                    return Some((Ok::<_, std::convert::Infallible>(ev), rx));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    sse::Sse::new(stream).into_response()
}

/// The JSON envelope of an [`EventFrame`] for SSE / `--json` consumers.
pub fn frame_json(frame: &EventFrame) -> serde_json::Value {
    match frame {
        EventFrame::Hello(h) => json!({
            "kind": "hello", "proto": h.proto, "server": h.server,
            "scopes": h.scopes,
        }),
        EventFrame::PaneSnapshot {
            session,
            seq,
            cols,
            rows,
            bytes,
        } => json!({
            "kind": "snapshot", "session": session, "seq": seq,
            "cols": cols, "rows": rows,
            "ansi_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
        EventFrame::PaneDelta {
            session,
            seq,
            bytes,
        } => json!({
            "kind": "delta", "session": session, "seq": seq,
            "b64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }),
        EventFrame::Activity { json: j } => json!({
            "kind": "activity",
            "event": serde_json::from_str::<serde_json::Value>(j)
                .unwrap_or_else(|_| serde_json::Value::String(j.clone())),
        }),
        EventFrame::Lease {
            session,
            kind,
            expires_at,
        } => json!({
            "kind": "lease", "session": session, "event": kind,
            "expires_at": expires_at,
        }),
        EventFrame::Pairing {
            pairing_id,
            label,
            scope,
            state,
        } => json!({
            "kind": "pairing", "pairing_id": pairing_id, "label": label,
            "scopes": scope, "state": state,
        }),
        EventFrame::Sessions => json!({ "kind": "sessions" }),
        EventFrame::SessionExit { session, code } => json!({
            "kind": "exit", "session": session, "code": code,
        }),
    }
}

fn default_history() -> bool {
    true
}

#[derive(Deserialize)]
struct AttachQuery {
    client_id: String,
    #[serde(default)]
    observer: bool,
    rows: Option<u16>,
    cols: Option<u16>,
    /// Include the scrollback history tail in the warm-attach snapshot.
    /// Defaults to true (a fresh client emulator wants the context); reconnect
    /// paths pass `false` so the tail isn't duplicated into scrollback the
    /// client already holds.
    #[serde(default = "default_history")]
    history: bool,
}

/// Client → daemon messages on an attach WebSocket (JSON text frames; the
/// daemon → client direction is binary [`EventFrame`]s).
#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum AttachClientMsg {
    Input { b64: String },
    Resize { rows: u16, cols: u16 },
}

/// Warm-attach over WebSocket: the snapshot frame arrives first, then live
/// deltas; input/resize ride back as JSON text frames. Write scope (an
/// attached client holds the session and can resize it); observers should
/// still hold write for now — the read-only view is `snapshot` + the event
/// feed.
async fn attach_ws(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    Query(q): Query<AttachQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let ctx = match authed(&state, &headers, Verb::Attach) {
        Ok(c) => c,
        Err(r) => return r,
    };
    ws.on_upgrade(move |socket| pump_attach(socket, state, ctx, s, q))
}

async fn pump_attach(
    mut socket: WebSocket,
    state: ControlState,
    ctx: AuthCtx,
    session: String,
    q: AttachQuery,
) {
    let kind = if q.observer {
        AttachKind::Observer
    } else {
        AttachKind::Interactive
    };
    let reply = match state
        .api
        .attach(
            &q.client_id,
            &session,
            kind,
            q.rows.unwrap_or(24),
            q.cols.unwrap_or(80),
            q.history,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    json!({ "error": e.to_string() }).to_string().into(),
                ))
                .await;
            return;
        }
    };
    let hello = hello_frame(&state, &ctx);
    let _ = socket.send(Message::Binary(hello.encode().into())).await;
    if socket
        .send(Message::Binary(reply.snapshot.encode().into()))
        .await
        .is_err()
    {
        let _ = state.api.detach(&q.client_id, &session).await;
        return;
    }
    let mut frames = reply.frames;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(f) => {
                    if socket.send(Message::Binary(f.encode().into())).await.is_err() {
                        break;
                    }
                }
                None => break, // session ended / daemon dropped the subscriber
            },
            msg = socket.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<AttachClientMsg>(&text) {
                        Ok(AttachClientMsg::Input { b64 }) => {
                            if let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(&b64)
                            {
                                let _ = state.api.send_input(&session, bytes).await;
                            }
                        }
                        Ok(AttachClientMsg::Resize { rows, cols }) => {
                            let _ = state.api.resize(&session, rows, cols).await;
                        }
                        Err(_) => {} // ignore malformed client frames
                    }
                }
                // Raw binary from the client = stdin bytes (the CLI's path).
                Some(Ok(Message::Binary(bytes))) => {
                    let _ = state.api.send_input(&session, bytes.to_vec()).await;
                }
                Some(Ok(_)) => {} // ping/pong handled by axum
                Some(Err(_)) | None => break, // client gone
            },
        }
    }
    let _ = state.api.detach(&q.client_id, &session).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_defaults_to_15_minutes() {
        assert_eq!(expiry_ms(1_000, None), 1_000 + 15 * 60 * 1000);
    }

    #[test]
    fn expiry_honours_a_sane_ttl() {
        assert_eq!(expiry_ms(0, Some(30)), 30 * 1000);
    }

    #[test]
    fn expiry_clamps_nonpositive_ttl_up_to_one_second() {
        assert_eq!(expiry_ms(0, Some(0)), 1000);
        assert_eq!(expiry_ms(0, Some(-5)), 1000);
    }

    #[test]
    fn expiry_does_not_overflow_on_adversarial_ttl() {
        // ttl_secs above i64::MAX/1000 would overflow the *1000 multiply
        // (panic in debug, wrap to a negative already-expired stamp in
        // release) without the clamp. Clamped to one year, the result stays a
        // sane future stamp and the saturating add never wraps.
        let one_year_ms = 60 * 60 * 24 * 365 * 1000;
        assert_eq!(expiry_ms(0, Some(i64::MAX)), one_year_ms);
        assert_eq!(expiry_ms(0, Some(10i64.pow(18))), one_year_ms);
        // Even at the max `now`, the add saturates instead of overflowing.
        assert_eq!(expiry_ms(i64::MAX, Some(i64::MAX)), i64::MAX);
        // The stamp is always strictly in the future (never already-expired).
        assert!(expiry_ms(1_000_000, Some(i64::MAX)) > 1_000_000);
    }
}
