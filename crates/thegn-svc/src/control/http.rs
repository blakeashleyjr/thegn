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
};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::{Arc, Mutex};

use thegn_core::control::{ScopeSet, TokenKind, Verb, required_scope};
use thegn_core::control_audit::{AuditOutcome, AuditRecord, is_audited};
use thegn_core::control_wire::{EventFrame, FeedFilter, Hello, PROTO_VERSION, PairingState};
use thegn_core::store::ControlStore;

use super::auth::{self, AuthCtx};
use super::{
    AttachKind, BrowserCommand, ControlApi, ControlError, ControlErrorCode, OpenSpec, RecordSpec,
    SplitDir, WaitCondition,
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
    /// `[serve] cors_origins`: exact origins a browser-hosted client may fetch
    /// from. Empty = no cross-origin access (the default). Applied as a CORS
    /// layer on the TCP listener only.
    pub cors_origins: Vec<String>,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build the control router bound to `state` — a fold over the
/// [`ROUTES`](super::routes::ROUTES) table, so every path names the
/// capability (and therefore the verb + scope) it serves and the catalog
/// coverage test can see it.
pub fn router(state: ControlState) -> Router {
    let cors = cors_layer(&state.cors_origins);
    let router = super::routes::ROUTES
        .iter()
        .fold(Router::new(), |r, route| {
            r.route(route.path, (route.build)())
        });
    let router = match cors {
        Some(layer) => router.layer(layer),
        None => router,
    };
    router.with_state(state)
}

/// Build the CORS layer for a `cors_origins` allowlist, or `None` when it is
/// empty (no cross-origin access — the default). Browsers preflight a
/// bearer-token `/v1` request (the `Authorization` header is non-simple), so
/// the layer must allow that header and the verbs the API uses; a wildcard
/// origin is refused at config validation, never reaching here.
fn cors_layer(origins: &[String]) -> Option<tower_http::cors::CorsLayer> {
    use axum::http::{HeaderValue, Method, header};
    use tower_http::cors::{AllowOrigin, CorsLayer};
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter(|o| o.trim() != "*")
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    if allowed.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(allowed))
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

fn error_json(status: StatusCode, code: ControlErrorCode, message: &str) -> Response {
    (
        status,
        axum::Json(super::ErrorBody {
            error: message.to_string(),
            code,
        }),
    )
        .into_response()
}

/// Maximum bytes read back from an in-process dispatch response body — the same
/// bound the reply-truncation cap sits behind, so a listing can't balloon here
/// either.
const DISPATCH_BODY_LIMIT: usize = 1024 * 1024;

/// Dispatch a control call in-process through the SAME axum router the control
/// API serves (`ServiceExt::oneshot`), returning `(status, json)`.
///
/// This is the push command inbox's executor: an admitted envelope
/// (`thegn_core::push_inbox` already enforced allowlist ∩ scope ∩
/// unconditional-admin-deny) is turned into `(method, path, body)` by
/// [`super::routes::build_call`] and run through the real handlers + `ControlApi`
/// — one capability dispatch, never a second policy table. `state` must be built
/// with `local_admin = true` (the inbox is the authenticator; the handler's
/// transport-auth is satisfied in-process), so callers outside the daemon must
/// not expose this.
pub async fn dispatch_local(
    state: ControlState,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    use tower::ServiceExt;
    let builder = axum::http::Request::builder().method(method).uri(path);
    let request = match body {
        Some(b) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(b.to_string())),
        None => builder.body(axum::body::Body::empty()),
    };
    let Ok(request) = request else {
        return (
            StatusCode::BAD_REQUEST,
            serde_json::to_value(super::ErrorBody {
                error: "could not build request".into(),
                code: ControlErrorCode::BadRequest,
            })
            .expect("ErrorBody serialization is infallible"),
        );
    };
    let response = match router(state).oneshot(request).await {
        Ok(r) => r,
        // The router service is infallible (`Error = Infallible`); this arm is
        // unreachable but keeps the call total rather than panicking the daemon.
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::to_value(super::ErrorBody {
                    error: "dispatch failed".into(),
                    code: ControlErrorCode::Internal,
                })
                .expect("ErrorBody serialization is infallible"),
            );
        }
    };
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), DISPATCH_BODY_LIMIT)
        .await
        .unwrap_or_default();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
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
        error_json(status, self.code(), &self.to_string())
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
/// chokepoint every authenticated handler goes through. Read handlers call
/// this; mutating handlers call [`authed_target`] with the acted-on resource so
/// the audit record names it.
// The Err IS the handler's whole response (a rejection short-circuits the
// request); it's produced once per request, so its size is irrelevant.
#[allow(clippy::result_large_err)]
fn authed(state: &ControlState, headers: &HeaderMap, verb: Verb) -> Result<AuthCtx, Response> {
    authed_target(state, headers, verb, "")
}

/// [`authed`] carrying the target resource (session id / worktree path /
/// pairing id) for the audit record. Every mutating verb (write/git/admin) and
/// every auth/scope rejection emits one record on `thegn::control::audit`.
#[allow(clippy::result_large_err)]
fn authed_target(
    state: &ControlState,
    headers: &HeaderMap,
    verb: Verb,
    target: &str,
) -> Result<AuthCtx, Response> {
    let ctx = if state.local_admin {
        AuthCtx::local_admin()
    } else {
        let Some(token) = bearer(headers) else {
            audit_anon(verb, target, AuditOutcome::Unauthorized);
            return Err(error_json(
                StatusCode::UNAUTHORIZED,
                ControlErrorCode::Unauthorized,
                "missing bearer token",
            ));
        };
        let store = state.store.lock().expect("control store lock");
        match auth::verify(&*store, &token, now_ms()) {
            Some(ctx) => ctx,
            None => {
                drop(store);
                audit_anon(verb, target, AuditOutcome::Unauthorized);
                return Err(error_json(
                    StatusCode::UNAUTHORIZED,
                    ControlErrorCode::Unauthorized,
                    "invalid or revoked token",
                ));
            }
        }
    };
    if let Err(e) = ctx.require(required_scope(verb)) {
        audit(&ctx, verb, target, AuditOutcome::NoScope);
        return Err(e.into_response());
    }
    // A mutating call that authorized: record its attribution. (The action's
    // own errors surface in the handler's response; this record proves who was
    // allowed to invoke what, against which resource.)
    if is_audited(verb) {
        audit(&ctx, verb, target, AuditOutcome::Ok);
    }
    Ok(ctx)
}

/// Emit one audit record for an authenticated caller.
fn audit(ctx: &AuthCtx, verb: Verb, target: &str, outcome: AuditOutcome) {
    emit_audit(&ctx.pairing_id, &ctx.label, verb, target, outcome);
}

/// Emit an audit record for a rejected request with no resolvable caller (no
/// or invalid credential) — the pairing id/label are empty; no secret is ever
/// logged.
fn audit_anon(verb: Verb, target: &str, outcome: AuditOutcome) {
    emit_audit("", "", verb, target, outcome);
}

fn emit_audit(pairing_id: &str, label: &str, verb: Verb, target: &str, outcome: AuditOutcome) {
    let rec = AuditRecord::for_verb(pairing_id, label, verb, target, outcome);
    tracing::info!(
        target: "thegn::control::audit",
        pairing_id = rec.pairing_id,
        label = rec.label,
        capability = rec.capability,
        scope = rec.scope.as_str(),
        resource = rec.target,
        outcome = rec.outcome.as_str(),
    );
}

pub(super) async fn health() -> Response {
    axum::Json(json!({ "ok": true })).into_response()
}

/// A static, fully self-contained pairing-redeem page (the URL
/// `PairingUrl::web_form()` advertises: `…/pair#t=tgp1_…`). Unauthenticated
/// like `/health`: the single-use code IS the credential, and it rides in the
/// URL **fragment**, so it never reaches the server's request log. The page
/// reads the code from `location.hash`, POSTs it to `/v1/pair`, and shows the
/// minted `tgc1_` token exactly once (nothing is persisted). No external assets
/// — a restrictive CSP forbids them.
pub(super) async fn pair_page() -> Response {
    // CSP: no external anything; inline script/style only; fetch same-origin.
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; \
                 connect-src 'self'; base-uri 'none'; form-action 'none'",
            ),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        PAIR_PAGE_HTML,
    )
        .into_response()
}

/// The redeem page body. Self-contained (inline CSS/JS, no assets); the code is
/// read from the fragment and never placed anywhere it could be logged or sent
/// except the `/v1/pair` POST body.
const PAIR_PAGE_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="referrer" content="no-referrer">
<title>Pair with thegn</title>
<style>
  :root { color-scheme: light dark; }
  body { font: 16px/1.5 system-ui, sans-serif; max-width: 34rem; margin: 3rem auto; padding: 0 1rem; }
  h1 { font-size: 1.3rem; }
  label { display: block; margin: 1rem 0 .25rem; font-weight: 600; }
  input { width: 100%; box-sizing: border-box; padding: .5rem; font: inherit; }
  button { margin-top: 1rem; padding: .55rem 1rem; font: inherit; cursor: pointer; }
  .token { margin-top: 1rem; padding: .75rem; border: 1px solid currentColor; border-radius: .3rem; word-break: break-all; font-family: ui-monospace, monospace; }
  .muted { opacity: .7; font-size: .9rem; }
  .err { color: #b00020; }
  [hidden] { display: none; }
</style>
</head>
<body>
<h1>Pair a client with thegn</h1>
<p class="muted">This device redeems a single-use pairing code for a scoped access token.
The code is read from this page's URL and is never sent anywhere but the pairing endpoint.</p>
<div id="form">
  <label for="label">Device label</label>
  <input id="label" value="browser" autocomplete="off">
  <button id="go">Redeem code</button>
  <p id="nocode" class="err" hidden>No pairing code in the URL. Open the exact link the server printed.</p>
  <p id="failed" class="err" hidden></p>
</div>
<div id="done" hidden>
  <p>Paired. Your access token (shown once — copy it now):</p>
  <div class="token" id="token"></div>
  <p class="muted" id="scopes"></p>
</div>
<script>
(function () {
  function frag(name) {
    var h = (location.hash || "").replace(/^#/, "");
    var parts = h.split("&");
    for (var i = 0; i < parts.length; i++) {
      var kv = parts[i].split("=");
      if (decodeURIComponent(kv[0]) === name) return decodeURIComponent(kv[1] || "");
    }
    return "";
  }
  var code = frag("t");
  var go = document.getElementById("go");
  if (!code) { document.getElementById("nocode").hidden = false; go.disabled = true; return; }
  go.addEventListener("click", function () {
    go.disabled = true;
    document.getElementById("failed").hidden = true;
    fetch("/v1/pair", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ code: code, label: document.getElementById("label").value || "browser" })
    }).then(function (r) { return r.json().then(function (b) { return { ok: r.ok, body: b }; }); })
      .then(function (res) {
        if (!res.ok || !res.body.token) {
          var e = document.getElementById("failed");
          e.textContent = (res.body && res.body.error) || "Pairing failed.";
          e.hidden = false; go.disabled = false; return;
        }
        document.getElementById("form").hidden = true;
        document.getElementById("done").hidden = false;
        document.getElementById("token").textContent = res.body.token;
        var s = res.body.scopes ? ("Scopes: " + res.body.scopes) : "";
        if (res.body.approved === false) s += " — pending operator approval.";
        document.getElementById("scopes").textContent = s;
      })
      .catch(function () {
        var e = document.getElementById("failed");
        e.textContent = "Could not reach the server.";
        e.hidden = false; go.disabled = false;
      });
  });
})();
</script>
</body>
</html>
"#;

// ── pairing ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct PairBody {
    code: String,
    #[serde(default)]
    label: String,
}

/// Unauthenticated by design: possession of the single-use pairing code is the
/// credential. A wrong code neither reveals nor consumes anything.
pub(super) async fn pair(
    State(state): State<ControlState>,
    body: axum::Json<PairBody>,
) -> Response {
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
            ControlErrorCode::Unauthorized,
            "invalid, expired, or already-redeemed pairing code",
        ),
        Err(e) => ControlError::Internal(e).into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct IssueBody {
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

pub(super) async fn issue_pairing(
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

pub(super) async fn list_pairings(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Response {
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

pub(super) async fn revoke_pairing(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::RevokePairing, &id) {
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

pub(super) async fn approve_pairing(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::ApprovePairing, &id) {
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

pub(super) async fn me(State(state): State<ControlState>, headers: HeaderMap) -> Response {
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

pub(super) async fn list_sessions(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::ListSessions) {
        return r;
    }
    match state.api.list_sessions().await {
        Ok(sessions) => axum::Json(json!({ "sessions": sessions })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn list_worktrees(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::ListWorktrees) {
        return r;
    }
    match state.api.list_worktrees().await {
        Ok(worktrees) => axum::Json(json!({ "worktrees": worktrees })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn leases(State(state): State<ControlState>, headers: HeaderMap) -> Response {
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

pub(super) async fn open_session(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<OpenSpec>,
) -> Response {
    if let Err(r) = authed_target(
        &state,
        &headers,
        Verb::OpenSession,
        body.worktree.as_deref().unwrap_or_default(),
    ) {
        return r;
    }
    match state.api.open(body.0).await {
        Ok(info) => axum::Json(info).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn snapshot(
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
pub(super) struct InputBody {
    /// Raw bytes, base64. Exactly one of `b64`/`text` must be present.
    b64: Option<String>,
    text: Option<String>,
    /// Append a carriage return (send-and-run).
    #[serde(default)]
    enter: bool,
}

pub(super) async fn send_input(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<InputBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::SendInput, &s) {
        return r;
    }
    let mut bytes = match (&body.b64, &body.text) {
        (Some(b64), None) => match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(b) => b,
            Err(_) => {
                return error_json(
                    StatusCode::BAD_REQUEST,
                    ControlErrorCode::BadRequest,
                    "invalid base64",
                );
            }
        },
        (None, Some(text)) => text.clone().into_bytes(),
        _ => {
            return error_json(
                StatusCode::BAD_REQUEST,
                ControlErrorCode::BadRequest,
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
pub(super) struct ResizeBody {
    rows: u16,
    cols: u16,
}

pub(super) async fn resize(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<ResizeBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::Resize, &s) {
        return r;
    }
    match state.api.resize(&s, body.rows, body.cols).await {
        Ok(()) => axum::Json(json!({ "resized": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Start/stop/query a daemon-side asciicast recording. Body: `{"op":"start"}`,
/// `{"op":"stop"}` or `{"op":"status"}`. Returns [`super::RecordStatus`] —
/// status and file path only, never the recorded bytes.
pub(super) async fn record(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<RecordSpec>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::RecordSession) {
        return r;
    }
    match state.api.record_session(&s, body.0).await {
        Ok(status) => axum::Json(status).into_response(),
        Err(e) => e.into_response(),
    }
}

/// Default program for a `split` with no argv: the daemon's login shell.
fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

#[derive(Deserialize)]
pub(super) struct WaitBody {
    condition: WaitCondition,
    /// Milliseconds before giving up (`matched=false`). Omit to wait forever.
    #[serde(default)]
    timeout_ms: Option<i64>,
}

pub(super) async fn wait(
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
pub(super) struct SplitBody {
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

pub(super) async fn split(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<SplitBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::Split, &s) {
        return r;
    }
    let b = body.0;
    // Inherit the target session's worktree, cwd and geometry so the sibling
    // lands in the same project at the same size.
    // Live sessions only: the listing now also carries recently-finished ones,
    // and splitting a sibling off a corpse would inherit a dead session's
    // geometry and cwd.
    let target = match state.api.list_sessions().await {
        Ok(list) => list
            .into_iter()
            .find(|si| si.id == s && si.exited_at_ms.is_none()),
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
        ..Default::default()
    };
    match state.api.split(&s, b.dir, spec).await {
        Ok(info) => axum::Json(info).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct DetachBody {
    client_id: String,
}

pub(super) async fn detach(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    body: axum::Json<DetachBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::Detach, &s) {
        return r;
    }
    match state.api.detach(&body.client_id, &s).await {
        Ok(()) => axum::Json(json!({ "detached": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn kill(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::KillSession, &s) {
        return r;
    }
    match state.api.kill(&s).await {
        Ok(()) => axum::Json(json!({ "killed": true })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── worktrees / browser / git ───────────────────────────────────────────────

#[derive(Deserialize)]
pub(super) struct OpenWorktreeBody {
    repo: String,
    branch: Option<String>,
}

pub(super) async fn open_worktree(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<OpenWorktreeBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::OpenWorktree, &body.repo) {
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

pub(super) async fn browser(
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

/// `worktrees.create` (git scope): the POST arm of `/v1/worktrees`.
pub(super) async fn create_worktree(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<super::WorktreeCreateReq>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::WorktreeCreate) {
        return r;
    }
    match state.api.worktree_create(body.0).await {
        Ok(info) => axum::Json(info).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── agent orchestration: issues (THE-57) ─────────────────────────────────────

/// Query params for `issues.list` — a subset of `IssueFilter` a supervisor
/// filters a batch by. Statuses is a comma-separated list of the snake_case
/// status ids (`todo,in_progress`); unknown names are dropped.
#[derive(Deserialize)]
pub(super) struct IssuesQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    query: Option<String>,
}

/// Parse a comma-separated status list into `IssueStatus`es (unknowns dropped).
pub(crate) fn parse_issue_statuses(s: &str) -> Vec<thegn_core::issue::IssueStatus> {
    use thegn_core::issue::IssueStatus::*;
    s.split(',')
        .filter_map(|part| match part.trim() {
            "backlog" => Some(Backlog),
            "todo" => Some(Todo),
            "in_progress" => Some(InProgress),
            "done" => Some(Done),
            "cancelled" => Some(Cancelled),
            _ => None,
        })
        .collect()
}

pub(super) async fn issues_list(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(q): Query<IssuesQuery>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::IssuesList) {
        return r;
    }
    let filter = thegn_core::issue::IssueFilter {
        statuses: q
            .status
            .as_deref()
            .map(parse_issue_statuses)
            .unwrap_or_default(),
        project_id: q.project,
        query: q.query,
        limit: q.limit.unwrap_or(0),
        ..Default::default()
    };
    match state.api.issues_list(&filter).await {
        Ok(issues) => axum::Json(json!({ "issues": issues })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn issue_get(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::IssuesGet) {
        return r;
    }
    match state.api.issues_get(&id).await {
        Ok(detail) => axum::Json(detail).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn issue_update(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::Json<thegn_core::issue::IssuePatch>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::IssuesUpdate) {
        return r;
    }
    match state.api.issues_update(&id, &body.0).await {
        Ok(issue) => axum::Json(issue).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct CommentBody {
    body: String,
}

pub(super) async fn issue_comment(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: axum::Json<CommentBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::IssuesComment) {
        return r;
    }
    match state.api.issues_comment(&id, &body.0.body).await {
        Ok(()) => axum::Json(json!({ "commented": id })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── agent orchestration: dispatch roster (THE-57) ────────────────────────────

pub(super) async fn dispatches_list(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::DispatchesList) {
        return r;
    }
    match state.api.dispatches_list().await {
        Ok(rows) => axum::Json(json!({ "dispatches": rows })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn dispatch_put(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<super::DispatchPutReq>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::DispatchesPut) {
        return r;
    }
    match state.api.dispatch_put(body.0).await {
        Ok(row) => axum::Json(row).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct DispatchStatusBody {
    /// A member of the closed dispatch-status set (snake_case).
    status: String,
}

pub(super) async fn dispatch_set_status(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    body: axum::Json<DispatchStatusBody>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::DispatchesSetStatus) {
        return r;
    }
    let status = thegn_core::issue::AgentDispatchStatus::parse(&body.0.status);
    match state.api.dispatch_set_status(id, status).await {
        Ok(()) => axum::Json(json!({ "id": id, "status": status.as_str() })).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct WorktreeQuery {
    worktree: String,
}

pub(super) async fn git_status(
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
pub(super) struct StageBody {
    worktree: String,
    paths: Vec<String>,
}

pub(super) async fn git_stage(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<StageBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::GitStage, &body.worktree) {
        return r;
    }
    match state.api.git_stage(&body.worktree, &body.paths).await {
        Ok(()) => axum::Json(json!({ "staged": body.paths.len() })).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct CommitBody {
    worktree: String,
    message: String,
}

pub(super) async fn git_commit(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<CommitBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::GitCommit, &body.worktree) {
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
pub(super) struct MergeBody {
    worktree: String,
}

pub(super) async fn merge_list(
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
pub(super) struct CalendarQuery {
    from: String,
    to: String,
}

/// The ingest body: the same `CalEvent` shape a `command` plugin emits, so one
/// contract serves both a polled plugin and a pushing daemon.
#[derive(Deserialize)]
pub(super) struct CalendarIngestBody {
    events: Vec<thegn_core::calendar::CalEvent>,
}

pub(super) async fn calendar_events(
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

pub(super) async fn calendar_clocks(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::CalendarClocks) {
        return r;
    }
    match state.api.calendar_clocks().await {
        Ok(clocks) => axum::Json(json!({ "clocks": clocks })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn calendar_ingest(
    State(state): State<ControlState>,
    headers: HeaderMap,
    axum::extract::Path(account): axum::extract::Path<String>,
    body: axum::Json<CalendarIngestBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::CalendarIngest, &account) {
        return r;
    }
    match state.api.calendar_ingest(&account, body.0.events).await {
        Ok(stored) => axum::Json(json!({ "stored": stored })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn merge_add(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<MergeBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::MergeAdd, &body.worktree) {
        return r;
    }
    match state.api.merge_add(&body.worktree).await {
        Ok(message) => axum::Json(json!({ "queued": true, "message": message })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn merge_clear(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<MergeBody>,
) -> Response {
    if let Err(r) = authed_target(&state, &headers, Verb::MergeClear, &body.worktree) {
        return r;
    }
    match state.api.merge_clear(&body.worktree).await {
        Ok(cleared) => axum::Json(json!({ "cleared": cleared })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── PR status / notifications ───────────────────────────────────────────────

pub(super) async fn pr_status(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::PrStatus) {
        return r;
    }
    match state.api.pr_status().await {
        Ok(rows) => axum::Json(json!({ "prs": rows })).into_response(),
        Err(e) => e.into_response(),
    }
}

#[derive(Deserialize)]
pub(super) struct AgentSessionsQuery {
    #[serde(default)]
    worktree: Option<String>,
    #[serde(default)]
    harness: Option<String>,
}

pub(super) async fn agent_sessions(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(q): Query<AgentSessionsQuery>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::AgentSessions) {
        return r;
    }
    match state
        .api
        .agent_sessions(q.worktree.as_deref(), q.harness.as_deref())
        .await
    {
        Ok(sessions) => axum::Json(json!({ "sessions": sessions })).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn notify_push(
    State(state): State<ControlState>,
    headers: HeaderMap,
    body: axum::Json<super::PushedNote>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::NotifyPush) {
        return r;
    }
    match state.api.notify_push(body.0).await {
        Ok(id) => axum::Json(json!({ "id": id })).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── mcp proxy hub ────────────────────────────────────────────────────────────

pub(super) async fn mcp_proxy_status(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::McpProxyStatus) {
        return r;
    }
    match state.api.mcp_proxy_status().await {
        Ok(s) => axum::Json(s).into_response(),
        Err(e) => e.into_response(),
    }
}

pub(super) async fn mcp_proxy_reload(
    State(state): State<ControlState>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::McpProxyReload) {
        return r;
    }
    match state.api.mcp_proxy_reload().await {
        Ok(r) => axum::Json(r).into_response(),
        Err(e) => e.into_response(),
    }
}

// ── daemon lifecycle ─────────────────────────────────────────────────────────

/// `POST /v1/daemon/shutdown` — gracefully stop the daemon (admin scope). Local
/// unix-socket peers reach it through implicit admin; TCP callers need an
/// admin-scoped token. The response is sent before the graceful drain
/// completes (the shutdown only notifies; axum finishes in-flight requests).
pub(super) async fn shutdown(State(state): State<ControlState>, headers: HeaderMap) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Shutdown) {
        return r;
    }
    state.api.shutdown().await;
    axum::Json(json!({ "shutdown": true })).into_response()
}

// ── streams ─────────────────────────────────────────────────────────────────

fn hello_frame(state: &ControlState, ctx: &AuthCtx) -> EventFrame {
    let mut scopes = Vec::new();
    for s in [
        thegn_core::control::Scope::Read,
        thegn_core::control::Scope::Write,
        thegn_core::control::Scope::Git,
        thegn_core::control::Scope::Exec,
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
#[derive(Debug, Deserialize, Default)]
pub(super) struct EventsQuery {
    /// Comma-separated [`thegn_core::control_wire::FEED_KINDS`].
    kinds: Option<String>,
    session: Option<String>,
    /// `1`, `true`, `0`, or `false`; omitted means false.
    signal_lag: Option<String>,
}

impl EventsQuery {
    fn into_filter(self) -> Result<FeedFilter, String> {
        let signal_lag = match self.signal_lag.as_deref() {
            None | Some("0") | Some("false") => false,
            Some("1") | Some("true") => true,
            Some(value) => {
                return Err(format!(
                    "invalid signal_lag {value:?}; expected 0, 1, true, or false"
                ));
            }
        };
        FeedFilter::parse(self.kinds.as_deref(), self.session.as_deref(), signal_lag)
            .map_err(|e| e.to_string())
    }
}

pub(super) async fn events_ws(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let ctx = match authed(&state, &headers, Verb::Events) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let filter = match q.into_filter() {
        Ok(filter) => filter,
        Err(message) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                ControlErrorCode::BadRequest,
                &message,
            );
        }
    };
    ws.on_upgrade(move |socket| pump_events(socket, state, ctx, filter))
}

async fn pump_events(mut socket: WebSocket, state: ControlState, ctx: AuthCtx, filter: FeedFilter) {
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
                if filter.matches(&frame)
                    && socket
                        .send(Message::Binary(frame.encode().into()))
                        .await
                        .is_err()
                {
                    return;
                }
            }
            // Slow consumer skipped `n` events — that's fine for a monitor
            // feed (pane bytes ride attach streams, not this one).
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                if filter.signal_lag {
                    let frame = EventFrame::Lagged { missed };
                    if socket
                        .send(Message::Binary(frame.encode().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// The same feed as JSON server-sent events (curl-friendly; pane bytes as
/// base64). WS is the primary transport — this is a convenience surface.
pub(super) async fn events_sse(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Query(q): Query<EventsQuery>,
) -> Response {
    if let Err(r) = authed(&state, &headers, Verb::Events) {
        return r;
    }
    let filter = match q.into_filter() {
        Ok(filter) => filter,
        Err(message) => {
            return error_json(
                StatusCode::BAD_REQUEST,
                ControlErrorCode::BadRequest,
                &message,
            );
        }
    };
    let rx = state.api.subscribe();
    let stream = futures_util::stream::unfold((rx, filter), |(mut rx, filter)| async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    if !filter.matches(&frame) {
                        continue;
                    }
                    let ev = sse::Event::default()
                        .event(frame.kind())
                        .data(frame_json(&frame).to_string());
                    return Some((Ok::<_, std::convert::Infallible>(ev), (rx, filter)));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    if filter.signal_lag {
                        let frame = EventFrame::Lagged { missed };
                        let ev = sse::Event::default()
                            .event(frame.kind())
                            .data(frame_json(&frame).to_string());
                        return Some((Ok::<_, std::convert::Infallible>(ev), (rx, filter)));
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    sse::Sse::new(stream).into_response()
}

/// The JSON envelope of an [`EventFrame`] for SSE / `--json` consumers.
pub use super::client::frame_json;

fn default_history() -> bool {
    true
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[tokio::test]
    async fn control_error_http_envelope_preserves_status_message_and_code() {
        let cases = [
            (
                ControlError::NotFound("session s1".into()),
                StatusCode::NOT_FOUND,
                ControlErrorCode::NotFound,
                "not found: session s1",
            ),
            (
                ControlError::NoScope {
                    need: thegn_core::control::Scope::Read,
                },
                StatusCode::FORBIDDEN,
                ControlErrorCode::NoScope,
                "missing required scope: read",
            ),
            (
                ControlError::Conflict("session s1".into()),
                StatusCode::CONFLICT,
                ControlErrorCode::Conflict,
                "conflict: session s1",
            ),
            (
                ControlError::Unimplemented("wait"),
                StatusCode::NOT_IMPLEMENTED,
                ControlErrorCode::Unimplemented,
                "not implemented: wait",
            ),
            (
                ControlError::Internal(anyhow::anyhow!("database failed")),
                StatusCode::INTERNAL_SERVER_ERROR,
                ControlErrorCode::Internal,
                "database failed",
            ),
        ];

        for (error, status, code, message) in cases {
            let response = error.into_response();
            assert_eq!(response.status(), status);
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            let body: super::super::ErrorBody = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body.error, message);
            assert_eq!(body.code, code);
        }
    }

    #[tokio::test]
    async fn adapter_error_codes_are_structured() {
        for (status, code, message) in [
            (
                StatusCode::UNAUTHORIZED,
                ControlErrorCode::Unauthorized,
                "missing bearer token",
            ),
            (
                StatusCode::BAD_REQUEST,
                ControlErrorCode::BadRequest,
                "invalid base64",
            ),
        ] {
            let response = error_json(status, code, message);
            assert_eq!(response.status(), status);
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .unwrap();
            let body: super::super::ErrorBody = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body.error, message);
            assert_eq!(body.code, code);
        }
    }
}

#[cfg(test)]
mod control_events {
    use super::*;

    #[test]
    fn event_query_uses_the_bounded_core_filter() {
        let filter = EventsQuery {
            kinds: Some("activity, exit".into()),
            session: Some("s1".into()),
            signal_lag: Some("1".into()),
        }
        .into_filter()
        .unwrap();
        assert_eq!(filter.kinds, Some(vec!["activity".into(), "exit".into()]));
        assert_eq!(filter.session.as_deref(), Some("s1"));
        assert!(filter.signal_lag);
    }

    #[test]
    fn event_query_rejects_typos_and_bad_lag_values() {
        let typo = EventsQuery {
            kinds: Some("activty".into()),
            ..Default::default()
        }
        .into_filter()
        .unwrap_err();
        assert!(typo.contains("unknown event kind"));

        let bad_lag = EventsQuery {
            signal_lag: Some("sometimes".into()),
            ..Default::default()
        }
        .into_filter()
        .unwrap_err();
        assert!(bad_lag.contains("invalid signal_lag"));
    }

    #[test]
    fn json_formatter_uses_frame_kind_and_lag_count() {
        let json = frame_json(&EventFrame::Lagged { missed: 12 });
        assert_eq!(json["kind"], "lagged");
        assert_eq!(json["missed"], 12);
    }
}

#[derive(Deserialize)]
pub(super) struct AttachQuery {
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
pub(super) async fn attach_ws(
    State(state): State<ControlState>,
    headers: HeaderMap,
    Path(s): Path<String>,
    Query(q): Query<AttachQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let ctx = match authed_target(&state, &headers, Verb::Attach, &s) {
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
            let _ = socket // best-effort: best we can do is try to report; client may be gone
                .send(Message::Text(
                    json!({ "error": e.to_string() }).to_string().into(),
                ))
                .await;
            return;
        }
    };
    let hello = hello_frame(&state, &ctx);
    let _ = socket.send(Message::Binary(hello.encode().into())).await; // best-effort: client may have disconnected already
    if socket
        .send(Message::Binary(reply.snapshot.encode().into()))
        .await
        .is_err()
    {
        let _ = state.api.detach(&q.client_id, &session).await; // best-effort: socket already broken
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
                                let _ = state.api.send_input(&session, bytes).await; // best-effort: session may be gone; loop exits on next frame
                            }
                        }
                        Ok(AttachClientMsg::Resize { rows, cols }) => {
                            let _ = state.api.resize(&session, rows, cols).await; // best-effort: session may be gone; loop exits on next frame
                        }
                        Err(_) => {} // ignore malformed client frames
                    }
                }
                // Raw binary from the client = stdin bytes (the CLI's path).
                Some(Ok(Message::Binary(bytes))) => {
                    let _ = state.api.send_input(&session, bytes.to_vec()).await; // best-effort: session may be gone; loop exits on next frame
                }
                Some(Ok(_)) => {} // ping/pong handled by axum
                Some(Err(_)) | None => break, // client gone
            },
        }
    }
    let _ = state.api.detach(&q.client_id, &session).await; // best-effort: detach on the way out; nothing to surface to a closing socket
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
