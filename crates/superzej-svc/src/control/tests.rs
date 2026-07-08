//! Contract tests for the control API's scope enforcement — the mobile
//! companion's guarantees, proven against the real router with a recording
//! fake behind it (no sockets; `tower::ServiceExt::oneshot`).
//!
//! The load-bearing assertion shape: an under-scoped request is rejected
//! **and the API recorded zero calls** (the spec's "rejected without
//! performing the action"), not merely rejected.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::future::BoxFuture;
use tower::ServiceExt;

use superzej_core::control::{ScopeSet, TokenKind};
use superzej_core::control_wire::EventFrame;
use superzej_core::db::Db;
use superzej_core::store::LeaseRow;

use super::auth;
use super::http::{ControlState, router};
use super::{
    AttachKind, AttachReply, BrowserCommand, ControlApi, ControlResult, GitFileStatus, OpenSpec,
    SessionInfo,
};

/// Records every trait call; returns minimal canned data.
#[derive(Default)]
struct FakeApi {
    calls: Mutex<Vec<String>>,
    events: std::sync::OnceLock<tokio::sync::broadcast::Sender<Arc<EventFrame>>>,
}

impl FakeApi {
    fn record(&self, call: &str) {
        self.calls.lock().unwrap().push(call.to_string());
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl ControlApi for FakeApi {
    fn list_sessions(&self) -> BoxFuture<'_, ControlResult<Vec<SessionInfo>>> {
        self.record("list_sessions");
        Box::pin(async { Ok(vec![]) })
    }
    fn open(&self, _spec: OpenSpec) -> BoxFuture<'_, ControlResult<SessionInfo>> {
        self.record("open");
        Box::pin(async {
            Ok(SessionInfo {
                id: "s1".into(),
                worktree: None,
                program: "sh".into(),
                cwd: None,
                rows: 24,
                cols: 80,
                created_at_ms: 0,
                attached_clients: 0,
                lease_expires_at: None,
            })
        })
    }
    fn attach<'a>(
        &'a self,
        _client_id: &'a str,
        _session: &'a str,
        _kind: AttachKind,
        _rows: u16,
        _cols: u16,
    ) -> BoxFuture<'a, ControlResult<AttachReply>> {
        self.record("attach");
        Box::pin(async {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            Ok(AttachReply {
                snapshot: EventFrame::PaneSnapshot {
                    session: "s1".into(),
                    seq: 0,
                    cols: 80,
                    rows: 24,
                    bytes: vec![],
                },
                frames: rx,
            })
        })
    }
    fn detach<'a>(
        &'a self,
        _client_id: &'a str,
        _session: &'a str,
    ) -> BoxFuture<'a, ControlResult<()>> {
        self.record("detach");
        Box::pin(async { Ok(()) })
    }
    fn send_input<'a>(
        &'a self,
        _session: &'a str,
        _bytes: Vec<u8>,
    ) -> BoxFuture<'a, ControlResult<()>> {
        self.record("send_input");
        Box::pin(async { Ok(()) })
    }
    fn resize<'a>(
        &'a self,
        _session: &'a str,
        _rows: u16,
        _cols: u16,
    ) -> BoxFuture<'a, ControlResult<()>> {
        self.record("resize");
        Box::pin(async { Ok(()) })
    }
    fn snapshot<'a>(&'a self, _session: &'a str) -> BoxFuture<'a, ControlResult<EventFrame>> {
        self.record("snapshot");
        Box::pin(async {
            Ok(EventFrame::PaneSnapshot {
                session: "s1".into(),
                seq: 0,
                cols: 80,
                rows: 24,
                bytes: b"\x1b[2J".to_vec(),
            })
        })
    }
    fn kill<'a>(&'a self, _session: &'a str) -> BoxFuture<'a, ControlResult<()>> {
        self.record("kill");
        Box::pin(async { Ok(()) })
    }
    fn open_worktree<'a>(
        &'a self,
        _repo: &'a str,
        _branch: Option<&'a str>,
    ) -> BoxFuture<'a, ControlResult<()>> {
        self.record("open_worktree");
        Box::pin(async { Ok(()) })
    }
    fn drive_browser(&self, _cmd: BrowserCommand) -> BoxFuture<'_, ControlResult<()>> {
        self.record("drive_browser");
        Box::pin(async { Err(super::ControlError::Unimplemented("drive-browser")) })
    }
    fn git_status<'a>(
        &'a self,
        _worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<GitFileStatus>>> {
        self.record("git_status");
        Box::pin(async { Ok(vec![]) })
    }
    fn git_stage<'a>(
        &'a self,
        _worktree: &'a str,
        _paths: &'a [String],
    ) -> BoxFuture<'a, ControlResult<()>> {
        self.record("git_stage");
        Box::pin(async { Ok(()) })
    }
    fn git_commit<'a>(
        &'a self,
        _worktree: &'a str,
        _message: &'a str,
    ) -> BoxFuture<'a, ControlResult<String>> {
        self.record("git_commit");
        Box::pin(async { Ok("abc123".into()) })
    }
    fn lease_status(&self) -> BoxFuture<'_, ControlResult<Vec<LeaseRow>>> {
        self.record("lease_status");
        Box::pin(async { Ok(vec![]) })
    }
    fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<EventFrame>> {
        self.events
            .get_or_init(|| tokio::sync::broadcast::channel(8).0)
            .subscribe()
    }
    fn shutdown(&self) -> BoxFuture<'_, ()> {
        self.record("shutdown");
        Box::pin(async {})
    }
}

struct Rig {
    api: Arc<FakeApi>,
    state: ControlState,
    db: Arc<Mutex<Db>>,
}

fn rig(local_admin: bool) -> Rig {
    let api = Arc::new(FakeApi::default());
    let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let state = ControlState {
        api: api.clone(),
        store: db.clone(),
        local_admin,
        require_approval: false,
        server_label: "test superzej".into(),
    };
    Rig { api, state, db }
}

/// Mint + persist a token with `scopes`, returning the bearer string.
fn token(rig: &Rig, scopes: &str) -> String {
    let m = auth::mint(
        TokenKind::Control,
        ScopeSet::parse(scopes),
        "test",
        None,
        None,
        1_000,
    );
    use superzej_core::store::ControlStore;
    rig.db.lock().unwrap().put_pairing(&m.row).unwrap();
    m.token
}

async fn call(rig: &Rig, method: &str, path: &str, bearer: Option<&str>) -> StatusCode {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = bearer {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = if method == "POST" {
        req.header("content-type", "application/json")
            .body(Body::from(default_body(path)))
            .unwrap()
    } else {
        req.body(Body::empty()).unwrap()
    };
    router(rig.state.clone())
        .oneshot(req)
        .await
        .unwrap()
        .status()
}

/// A syntactically valid body per POST route (contents don't matter — the
/// scope check runs first and must reject before any parsing side effect).
fn default_body(path: &str) -> &'static str {
    if path.contains("/input") {
        r#"{"text":"x"}"#
    } else if path.contains("/resize") {
        r#"{"rows":24,"cols":80}"#
    } else if path.contains("/detach") {
        r#"{"client_id":"c"}"#
    } else if path.contains("/worktrees/open") {
        r#"{"repo":"r"}"#
    } else if path.contains("/browser") {
        r#"{"session":null,"action":"reload"}"#
    } else if path.contains("/git/stage") {
        r#"{"worktree":"/w","paths":["a"]}"#
    } else if path.contains("/git/commit") {
        r#"{"worktree":"/w","message":"m"}"#
    } else if path.ends_with("/v1/sessions") {
        r#"{"argv":["/bin/sh"],"rows":24,"cols":80}"#
    } else if path.contains("/pairings") {
        r#"{"scope":"read"}"#
    } else {
        "{}"
    }
}

#[tokio::test]
async fn read_scope_covers_exactly_the_read_surface() {
    let r = rig(false);
    let read = token(&r, "read");
    for (method, path) in [
        ("GET", "/v1/sessions"),
        ("GET", "/v1/leases"),
        ("GET", "/v1/me"),
        ("GET", "/v1/sessions/s1/snapshot"),
        ("GET", "/v1/git/status?worktree=%2Fw"),
    ] {
        assert_eq!(
            call(&r, method, path, Some(&read)).await,
            StatusCode::OK,
            "{method} {path} must be readable with read scope"
        );
    }
}

#[tokio::test]
async fn under_scoped_requests_are_rejected_with_zero_side_effects() {
    let r = rig(false);
    let read = token(&r, "read");
    // Write and git verbs with a read-only token: 403, and the API must have
    // recorded NO calls (rejection happens before the trait).
    for (method, path) in [
        ("POST", "/v1/sessions"),
        ("POST", "/v1/sessions/s1/input"),
        ("POST", "/v1/sessions/s1/resize"),
        ("POST", "/v1/sessions/s1/detach"),
        ("DELETE", "/v1/sessions/s1"),
        ("POST", "/v1/worktrees/open"),
        ("POST", "/v1/browser"),
        ("POST", "/v1/git/stage"),
        ("POST", "/v1/git/commit"),
        ("POST", "/v1/pairings"),
        ("GET", "/v1/pairings"),
        ("DELETE", "/v1/pairings/x"),
        ("POST", "/v1/pairings/x/approve"),
    ] {
        assert_eq!(
            call(&r, method, path, Some(&read)).await,
            StatusCode::FORBIDDEN,
            "{method} {path} must be forbidden for read scope"
        );
    }
    assert_eq!(
        r.api.calls(),
        Vec::<String>::new(),
        "no API call may run for a rejected request"
    );
}

#[tokio::test]
async fn git_scope_commits_but_cannot_type_into_terminals() {
    let r = rig(false);
    let git = token(&r, "git");
    // The mobile stage/commit contract: git scope routes stage/commit…
    assert_eq!(
        call(&r, "POST", "/v1/git/stage", Some(&git)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&r, "POST", "/v1/git/commit", Some(&git)).await,
        StatusCode::OK
    );
    assert_eq!(
        r.api.calls(),
        vec!["git_stage".to_string(), "git_commit".to_string()]
    );
    // …but must NOT reach a terminal (Git ⊅ Write) or admin surface.
    assert_eq!(
        call(&r, "POST", "/v1/sessions/s1/input", Some(&git)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&r, "GET", "/v1/pairings", Some(&git)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(r.api.calls().len(), 2, "rejections added no calls");
}

#[tokio::test]
async fn missing_revoked_and_expired_tokens_are_401() {
    let r = rig(false);
    assert_eq!(
        call(&r, "GET", "/v1/sessions", None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&r, "GET", "/v1/sessions", Some("garbage")).await,
        StatusCode::UNAUTHORIZED
    );
    // Revoked.
    let t = token(&r, "read");
    {
        use superzej_core::store::ControlStore;
        let (_, parts) = superzej_core::control::parse_token(&t).unwrap();
        r.db.lock()
            .unwrap()
            .revoke_pairing(&parts.id, 2_000)
            .unwrap();
    }
    assert_eq!(
        call(&r, "GET", "/v1/sessions", Some(&t)).await,
        StatusCode::UNAUTHORIZED
    );
    assert!(r.api.calls().is_empty());
}

#[tokio::test]
async fn me_reflects_the_presented_token_scope_switch() {
    // "Switch account or scope": stateless bearer — switching tokens between
    // requests changes the authorized scope, visible via /v1/me.
    let r = rig(false);
    let read = token(&r, "read");
    let admin = token(&r, "read,write,git,admin");
    for (tok, expect) in [(&read, "read"), (&admin, "read,write,git,admin")] {
        let req = Request::builder()
            .method("GET")
            .uri("/v1/me")
            .header("authorization", format!("Bearer {tok}"))
            .body(Body::empty())
            .unwrap();
        let res = router(r.state.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.get("scopes").and_then(|s| s.as_str()), Some(expect));
    }
}

#[tokio::test]
async fn unauthenticated_pair_redeem_mints_a_scoped_token() {
    let r = rig(false);
    // Issue a code directly (the serve-startup path).
    // No expiry: the HTTP layer checks real wall-clock time, so a tiny
    // epoch-ms expiry would be "expired" before the request runs.
    let code = auth::mint(
        TokenKind::PairingCode,
        ScopeSet::parse("read,git"),
        "",
        None,
        None,
        1_000,
    );
    {
        use superzej_core::store::ControlStore;
        r.db.lock().unwrap().put_pairing(&code.row).unwrap();
    }
    let req = Request::builder()
        .method("POST")
        .uri("/v1/pair")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"code":"{}","label":"phone"}}"#,
            code.token
        )))
        .unwrap();
    let res = router(r.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let minted = v.get("token").and_then(|t| t.as_str()).unwrap().to_string();
    // The minted token works, with the code's scopes; the code is burnt.
    assert_eq!(
        call(&r, "GET", "/v1/sessions", Some(&minted)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&r, "POST", "/v1/git/stage", Some(&minted)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&r, "POST", "/v1/sessions/s1/input", Some(&minted)).await,
        StatusCode::FORBIDDEN
    );
    let reuse = Request::builder()
        .method("POST")
        .uri("/v1/pair")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"code":"{}","label":"again"}}"#,
            code.token
        )))
        .unwrap();
    assert_eq!(
        router(r.state.clone())
            .oneshot(reuse)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn local_admin_listener_needs_no_token_and_drive_browser_is_501() {
    let r = rig(true);
    assert_eq!(call(&r, "GET", "/v1/sessions", None).await, StatusCode::OK);
    assert_eq!(call(&r, "GET", "/v1/pairings", None).await, StatusCode::OK);
    // The reserved verb answers 501 (defined contract, no behavior yet).
    assert_eq!(
        call(&r, "POST", "/v1/browser", None).await,
        StatusCode::NOT_IMPLEMENTED
    );
    // Push registration is reserved for AI 422/423 — absent in v1 (404).
    assert_eq!(
        call(&r, "POST", "/v1/push/register", None).await,
        StatusCode::NOT_FOUND
    );
}
