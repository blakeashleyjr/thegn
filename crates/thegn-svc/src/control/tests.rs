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

use thegn_core::control::{ScopeSet, TokenKind};
use thegn_core::control_wire::EventFrame;
use thegn_core::db::Db;
use thegn_core::store::LeaseRow;

use super::auth;
use super::http::{ControlState, router};
use super::{
    AttachKind, AttachReply, BrowserCommand, ControlApi, ControlResult, GitFileStatus, OpenSpec,
    PreviewFetchReply, PreviewFetchRequest, SessionInfo,
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
    fn list_worktrees(&self) -> BoxFuture<'_, ControlResult<Vec<super::WorktreeInfo>>> {
        self.record("list_worktrees");
        Box::pin(async {
            Ok(vec![super::WorktreeInfo {
                path: "/w".into(),
                branch: "main".into(),
                repo_root: "/r".into(),
                location: String::new(),
                created_at: 0,
            }])
        })
    }
    fn list_skills(&self) -> BoxFuture<'_, ControlResult<super::SkillsList>> {
        self.record("list_skills");
        Box::pin(async {
            Ok(super::SkillsList {
                skills: vec![super::SkillInfo {
                    name: "mq".into(),
                    description: "merge queue".into(),
                    harnesses: vec!["claude".into()],
                    gate: "merge_queue".into(),
                    when: vec!["explicit".into()],
                    source: thegn_core::skills::SkillSource::embedded(
                        "extensions/skills/mq/SKILL.md",
                    ),
                }],
                diagnostics: vec![],
            })
        })
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
                pid: None,
                ..Default::default()
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
        _history: bool,
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
    fn preview_fetch(
        &self,
        req: PreviewFetchRequest,
    ) -> BoxFuture<'_, ControlResult<PreviewFetchReply>> {
        self.record("preview_fetch");
        Box::pin(async move {
            Ok(PreviewFetchReply {
                url: req.url,
                status: 200,
                content_type: Some("text/plain".into()),
                body: "ok".into(),
                truncated: false,
                console_errors: Vec::new(),
                diagnostics_source: "unavailable".into(),
            })
        })
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
    fn merge_add<'a>(&'a self, _worktree: &'a str) -> BoxFuture<'a, ControlResult<String>> {
        self.record("merge_add");
        Box::pin(async { Ok("queued feature-x".into()) })
    }
    fn merge_clear<'a>(&'a self, _worktree: &'a str) -> BoxFuture<'a, ControlResult<usize>> {
        self.record("merge_clear");
        Box::pin(async { Ok(0) })
    }
    fn merge_list<'a>(
        &'a self,
        _worktree: &'a str,
    ) -> BoxFuture<'a, ControlResult<Vec<thegn_core::db::MergeQueueRow>>> {
        self.record("merge_list");
        Box::pin(async { Ok(vec![]) })
    }
    fn pr_status(&self) -> BoxFuture<'_, ControlResult<Vec<super::PrStatusRow>>> {
        self.record("pr_status");
        Box::pin(async {
            Ok(vec![super::PrStatusRow {
                worktree: "/w".into(),
                branch: "feature-x".into(),
                number: 42,
                title: "a change".into(),
                state: "OPEN".into(),
                url: "https://forge/pr/42".into(),
                is_draft: false,
                fetched_at: 1,
            }])
        })
    }
    fn notify_push(&self, _note: super::PushedNote) -> BoxFuture<'_, ControlResult<i64>> {
        self.record("notify_push");
        Box::pin(async { Ok(7) })
    }
    fn lease_status(&self) -> BoxFuture<'_, ControlResult<Vec<LeaseRow>>> {
        self.record("lease_status");
        Box::pin(async { Ok(vec![]) })
    }
    fn publish_pairing(
        &self,
        _pairing_id: &str,
        _label: &str,
        _scope: &str,
        state: thegn_core::control_wire::PairingState,
    ) {
        self.record(&format!("publish_pairing:{state:?}"));
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
        server_label: "test thegn".into(),
        cors_origins: Vec::new(),
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
    use thegn_core::store::ControlStore;
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
    } else if path.contains("/preview/fetch") {
        r#"{"url":"http://localhost:3000/"}"#
    } else if path.contains("/git/stage") {
        r#"{"worktree":"/w","paths":["a"]}"#
    } else if path.contains("/git/commit") {
        r#"{"worktree":"/w","message":"m"}"#
    } else if path.contains("/merge/add") || path.contains("/merge/clear") {
        r#"{"worktree":"/w"}"#
    } else if path.ends_with("/v1/notify") {
        r#"{"title":"t","body":"b"}"#
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
        ("GET", "/v1/worktrees"),
        ("GET", "/v1/skills"),
        ("GET", "/v1/leases"),
        ("GET", "/v1/me"),
        ("GET", "/v1/sessions/s1/snapshot"),
        ("GET", "/v1/git/status?worktree=%2Fw"),
        ("GET", "/v1/merge/list?worktree=%2Fw"),
        ("GET", "/v1/pr/status"),
        ("POST", "/v1/preview/fetch"),
    ] {
        assert_eq!(
            call(&r, method, path, Some(&read)).await,
            StatusCode::OK,
            "{method} {path} must be readable with read scope"
        );
    }
}

#[tokio::test]
async fn preview_fetch_is_rejected_without_read_scope_before_the_api() {
    let r = rig(false);
    let none = token(&r, "");
    assert_eq!(
        call(&r, "POST", "/v1/preview/fetch", Some(&none)).await,
        StatusCode::FORBIDDEN
    );
    assert!(r.api.calls().is_empty());

    let read = token(&r, "read");
    assert_eq!(
        call(&r, "POST", "/v1/preview/fetch", Some(&read)).await,
        StatusCode::OK
    );
    assert_eq!(r.api.calls(), ["preview_fetch"]);
}

#[tokio::test]
async fn preview_fetch_authenticates_before_decoding_its_bounded_body() {
    let r = rig(false);
    let none = token(&r, "");
    let request = |bearer: &str| {
        Request::builder()
            .method("POST")
            .uri("/v1/preview/fetch")
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .body(Body::from("not json"))
            .unwrap()
    };
    let response = router(r.state.clone())
        .oneshot(request(&none))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(r.api.calls().is_empty());

    let read = token(&r, "read");
    let response = router(r.state.clone())
        .oneshot(request(&read))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(r.api.calls().is_empty());
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
        ("POST", "/v1/merge/add"),
        ("POST", "/v1/merge/clear"),
        ("POST", "/v1/notify"),
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
async fn worktrees_list_needs_read_and_is_rejected_before_the_api() {
    let r = rig(false);
    // A token with no scopes at all: forbidden, and the fake saw nothing.
    let none = token(&r, "");
    assert_eq!(
        call(&r, "GET", "/v1/worktrees", Some(&none)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(r.api.calls(), Vec::<String>::new());
    let read = token(&r, "read");
    assert_eq!(
        call(&r, "GET", "/v1/worktrees", Some(&read)).await,
        StatusCode::OK
    );
    assert_eq!(r.api.calls(), vec!["list_worktrees".to_string()]);
}

#[tokio::test]
async fn skills_list_is_metadata_only_and_read_scoped() {
    let r = rig(false);
    let none = token(&r, "");
    assert_eq!(
        call(&r, "GET", "/v1/skills", Some(&none)).await,
        StatusCode::FORBIDDEN
    );
    assert!(r.api.calls().is_empty());
    let read = token(&r, "read");
    let req = Request::builder()
        .method("GET")
        .uri("/v1/skills")
        .header("authorization", format!("Bearer {read}"))
        .body(Body::empty())
        .unwrap();
    let response = router(r.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["skills"][0]["name"], "mq");
    assert!(value.get("worktree").is_none());
    assert_eq!(r.api.calls(), ["list_skills"]);
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
    // …and the git-adjacent merge add/clear verbs.
    assert_eq!(
        call(&r, "POST", "/v1/merge/add", Some(&git)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&r, "POST", "/v1/merge/clear", Some(&git)).await,
        StatusCode::OK
    );
    assert_eq!(
        r.api.calls(),
        vec![
            "git_stage".to_string(),
            "git_commit".to_string(),
            "merge_add".to_string(),
            "merge_clear".to_string(),
        ]
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
    assert_eq!(r.api.calls().len(), 4, "rejections added no calls");
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
        use thegn_core::store::ControlStore;
        let (_, parts) = thegn_core::control::parse_token(&t).unwrap();
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
        use thegn_core::store::ControlStore;
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

/// The pairing lifecycle must be visible on the event feed: redeem announces
/// `Requested` (the `require_approval` park that used to be silent), approve
/// announces `Approved`, revoke announces `Revoked`.
#[tokio::test]
async fn pairing_lifecycle_publishes_feed_frames() {
    let api = Arc::new(FakeApi::default());
    let db = Arc::new(Mutex::new(Db::open_memory().unwrap()));
    let state = ControlState {
        api: api.clone(),
        store: db.clone(),
        local_admin: true,
        require_approval: true, // redeemed tokens park ⇒ Requested
        server_label: "test thegn".into(),
        cors_origins: Vec::new(),
    };
    let code = auth::mint(
        TokenKind::PairingCode,
        ScopeSet::parse("read"),
        "",
        None,
        None,
        1_000,
    );
    {
        use thegn_core::store::ControlStore;
        db.lock().unwrap().put_pairing(&code.row).unwrap();
    }

    // Redeem → Requested.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/pair")
        .header("content-type", "application/json")
        .body(Body::from(format!(
            r#"{{"code":"{}","label":"phone"}}"#,
            code.token
        )))
        .unwrap();
    let res = router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v.get("approved").and_then(|a| a.as_bool()), Some(false));
    let pid = v
        .get("pairing_id")
        .and_then(|p| p.as_str())
        .unwrap()
        .to_string();

    // Approve → Approved; revoke → Revoked (local_admin listener: no token).
    for (method, path) in [
        ("POST", format!("/v1/pairings/{pid}/approve")),
        ("DELETE", format!("/v1/pairings/{pid}")),
    ] {
        let req = Request::builder()
            .method(method)
            .uri(&path)
            .body(Body::empty())
            .unwrap();
        let res = router(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{method} {path}");
    }

    let published: Vec<String> = api
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("publish_pairing:"))
        .collect();
    assert_eq!(
        published,
        vec![
            "publish_pairing:Requested".to_string(),
            "publish_pairing:Approved".to_string(),
            "publish_pairing:Revoked".to_string(),
        ]
    );
}

/// The two client-API verbs that used to be SURFACE_GAPS excuses: the PR
/// status projection answers reads, and a pushed note reaches the API with
/// the parsed body and returns the stored row id.
#[tokio::test]
async fn pr_status_and_notify_push_route_to_the_api() {
    let r = rig(false);
    let read = token(&r, "read");
    let write = token(&r, "write");

    // GET /v1/pr/status with read scope: the fake's row comes back verbatim.
    let req = Request::builder()
        .method("GET")
        .uri("/v1/pr/status")
        .header("authorization", format!("Bearer {read}"))
        .body(Body::empty())
        .unwrap();
    let res = router(r.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let rows: Vec<super::PrStatusRow> = serde_json::from_value(v["prs"].clone()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, 42);
    assert_eq!(rows[0].worktree, "/w");
    assert_eq!(rows[0].state, "OPEN");

    // POST /v1/notify with write scope: 200 + the stored row id.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/notify")
        .header("authorization", format!("Bearer {write}"))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"title":"build done","body":"all green","urgency":"alert","source":"ci"}"#,
        ))
        .unwrap();
    let res = router(r.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["id"], 7);

    assert_eq!(
        r.api.calls(),
        vec!["pr_status".to_string(), "notify_push".to_string()]
    );
}

/// The new wire types survive a serde round-trip, and `PushedNote`'s optional
/// fields default when absent (the minimal `{"title": …}` push is valid).
#[test]
fn pr_status_row_and_pushed_note_serde_round_trip() {
    let row = super::PrStatusRow {
        worktree: "/w/x".into(),
        branch: "feat".into(),
        number: 7,
        title: "t".into(),
        state: "MERGED".into(),
        url: "https://forge/pr/7".into(),
        is_draft: true,
        fetched_at: 123,
    };
    let back: super::PrStatusRow =
        serde_json::from_str(&serde_json::to_string(&row).unwrap()).unwrap();
    assert_eq!(back, row);

    let note = super::PushedNote {
        title: "hi".into(),
        body: "there".into(),
        urgency: Some("alert".into()),
        source: Some("ci".into()),
    };
    let back: super::PushedNote =
        serde_json::from_str(&serde_json::to_string(&note).unwrap()).unwrap();
    assert_eq!(back, note);

    let minimal: super::PushedNote = serde_json::from_str(r#"{"title":"t","body":""}"#).unwrap();
    assert_eq!(minimal.title, "t");
    assert!(minimal.body.is_empty());
    assert_eq!(minimal.urgency, None);
    assert_eq!(minimal.source, None);
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

/// `daemon.shutdown` is admin-scoped: a read/git token is rejected before any
/// shutdown begins; an admin token (or a local_admin listener) reaches it.
#[tokio::test]
async fn daemon_shutdown_is_admin_scoped() {
    let r = rig(false);
    let git = token(&r, "git");
    assert_eq!(
        call(&r, "POST", "/v1/daemon/shutdown", Some(&git)).await,
        StatusCode::FORBIDDEN
    );
    assert!(
        r.api.calls().is_empty(),
        "no shutdown for a non-admin token"
    );
    let admin = token(&r, "admin");
    assert_eq!(
        call(&r, "POST", "/v1/daemon/shutdown", Some(&admin)).await,
        StatusCode::OK
    );
    assert_eq!(r.api.calls(), vec!["shutdown".to_string()]);
}

/// `GET /pair` serves an unauthenticated, self-contained redeem page with a
/// restrictive CSP and no external assets.
#[tokio::test]
async fn pair_page_is_self_contained_and_unauthenticated() {
    let r = rig(false);
    let req = Request::builder()
        .method("GET")
        .uri("/pair")
        .body(Body::empty())
        .unwrap();
    let res = router(r.state.clone()).oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let ctype = res
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(ctype.contains("text/html"), "{ctype}");
    let csp = res
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("Pair a client"), "{html}");
    // Self-contained: no external asset loads (no src=/href= to a remote URL).
    assert!(!html.contains("src=\"http"), "external script");
    assert!(!html.contains("href=\"http"), "external stylesheet");
    assert!(
        r.api.calls().is_empty(),
        "the page performs no control call"
    );
}

/// A minimal capturing subscriber for the `thegn::control::audit` target, so
/// the audit-record emission can be asserted without a tracing framework dep.
mod audit_capture {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use tracing::field::{Field, Visit};
    use tracing::span;
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Default, Clone)]
    pub struct Captured(pub Arc<Mutex<Vec<BTreeMap<String, String>>>>);

    impl Captured {
        pub fn records(&self) -> Vec<BTreeMap<String, String>> {
            self.0.lock().unwrap().clone()
        }
    }

    struct FieldVisitor(BTreeMap<String, String>);
    impl Visit for FieldVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    pub struct AuditSubscriber(pub Captured);
    impl Subscriber for AuditSubscriber {
        fn enabled(&self, meta: &Metadata<'_>) -> bool {
            meta.target() == "thegn::control::audit"
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            if event.metadata().target() != "thegn::control::audit" {
                return;
            }
            let mut v = FieldVisitor(BTreeMap::new());
            event.record(&mut v);
            self.0.0.lock().unwrap().push(v.0);
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }
}

/// Every mutating control call, the network-reaching read-scoped preview fetch,
/// and every scope rejection emits one structured audit record on
/// `thegn::control::audit`, naming the caller, capability, resource and outcome
/// — never a secret.
#[tokio::test]
async fn mutating_calls_and_rejections_emit_audit_records() {
    let captured = audit_capture::Captured::default();
    let _guard = tracing::subscriber::set_default(audit_capture::AuditSubscriber(captured.clone()));

    let r = rig(false);
    let git = token(&r, "git");
    // git.commit with git scope → an ok record naming the worktree ("/w").
    assert_eq!(
        call(&r, "POST", "/v1/git/commit", Some(&git)).await,
        StatusCode::OK
    );
    // sessions.input with git (lacks write) → a no_scope record.
    assert_eq!(
        call(&r, "POST", "/v1/sessions/s1/input", Some(&git)).await,
        StatusCode::FORBIDDEN
    );
    // A read GET emits nothing (not a mutating verb, and it authorized).
    let read = token(&r, "read");
    assert_eq!(
        call(&r, "GET", "/v1/sessions", Some(&read)).await,
        StatusCode::OK
    );
    assert_eq!(
        call(&r, "POST", "/v1/preview/fetch", Some(&read)).await,
        StatusCode::OK
    );

    let recs = captured.records();
    let ok = recs.iter().find(|m| {
        m.get("capability").map(String::as_str) == Some("git.commit")
            && m.get("outcome").map(String::as_str) == Some("ok")
    });
    let ok = ok.expect("git.commit ok record");
    assert_eq!(ok.get("resource").map(String::as_str), Some("/w"));
    assert!(ok.contains_key("pairing_id"));
    assert!(recs.iter().any(|m| {
        m.get("capability").map(String::as_str) == Some("sessions.input")
            && m.get("outcome").map(String::as_str) == Some("no_scope")
    }));
    assert!(recs.iter().any(|m| {
        m.get("capability").map(String::as_str) == Some("preview.fetch")
            && m.get("outcome").map(String::as_str) == Some("ok")
    }));
    // A read GET produced no record.
    assert!(
        !recs
            .iter()
            .any(|m| m.get("capability").map(String::as_str) == Some("sessions.list")),
        "read verbs are not audited: {recs:?}"
    );
    // No record carries a token secret (only the public pairing id half).
    for m in &recs {
        for v in m.values() {
            assert!(!v.contains(&git), "audit record leaked a token: {m:?}");
            assert!(
                !v.starts_with("tgc1_"),
                "audit record leaked a token: {m:?}"
            );
        }
    }
}
