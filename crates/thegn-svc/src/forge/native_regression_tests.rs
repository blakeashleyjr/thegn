use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use thegn_core::seam::SeamError;

pub(super) fn client(status: u16, body: &str) -> TrackerHttpClient {
    client_with_delay(status, body, None, Duration::from_secs(2))
}

pub(super) fn client_with_delay(
    status: u16,
    body: &str,
    delay: Option<Duration>,
    timeout: Duration,
) -> TrackerHttpClient {
    client_with_content_type(status, body, delay, timeout, Some("application/json"))
}

fn client_with_content_type(
    status: u16,
    body: &str,
    delay: Option<Duration>,
    timeout: Duration,
    content_type: Option<&'static str>,
) -> TrackerHttpClient {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;

    let body = body.to_owned();
    let app = Router::new().fallback(any(move |_: Request| {
        let body = body.clone();
        async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            let mut response =
                (StatusCode::from_u16(status).unwrap(), Body::from(body)).into_response();
            if let Some(content_type) = content_type {
                response.headers_mut().insert(
                    reqwest::header::CONTENT_TYPE,
                    HeaderValue::from_static(content_type),
                );
            }
            response
        }
    }));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TrackerHttpClient::new_with_operation_timeout(
        "github-test",
        &format!("http://{address}"),
        "Bearer fixture".into(),
        Arc::new(TrackerHttpBudget::with_permits(8)),
        timeout,
    )
    .unwrap()
}

fn client_with_header(
    status: u16,
    body: &str,
    header: (&'static str, &'static str),
) -> TrackerHttpClient {
    client_with_headers(status, body, &[header])
}

fn client_with_headers(status: u16, body: &str, headers: &[(&str, &str)]) -> TrackerHttpClient {
    use axum::Router;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::{HeaderName, HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;

    let body = body.to_owned();
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let app = Router::new().fallback(any(move |_: Request| {
        let body = body.clone();
        let headers = headers.clone();
        async move {
            let mut response =
                (StatusCode::from_u16(status).unwrap(), Body::from(body)).into_response();
            for (name, value) in headers {
                response.headers_mut().insert(
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(&value).unwrap(),
                );
            }
            response
        }
    }));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    TrackerHttpClient::new_with_operation_timeout(
        "github-test",
        &format!("http://{address}"),
        "Bearer fixture".into(),
        Arc::new(TrackerHttpBudget::with_permits(8)),
        Duration::from_secs(2),
    )
    .unwrap()
}

pub(super) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn graphql_http_request_preserves_method_path_auth_and_json_body() {
    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::extract::Request;
    use axum::http::{HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::any;

    let rt = runtime();
    rt.block_on(async {
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = Router::new().fallback(any(move |request: Request| {
            let seen_tx = seen_tx.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, 4096).await.expect("small GraphQL request");
                seen_tx
                    .send((
                        parts.method,
                        parts.uri.path().to_owned(),
                        parts.headers.get("authorization").cloned(),
                        parts.headers.get("content-type").cloned(),
                        parts.headers.get("accept").cloned(),
                        body,
                    ))
                    .expect("fixture receiver remains open");
                (
                    StatusCode::OK,
                    [(reqwest::header::CONTENT_TYPE, "application/json")],
                    Body::from(r#"{"data":{"ok":true}}"#),
                )
                    .into_response()
            }
        }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = TrackerHttpClient::new_with_operation_timeout(
            "github-test",
            &format!("http://{address}"),
            "Bearer fixture-token".into(),
            Arc::new(TrackerHttpBudget::with_permits(1)),
            Duration::from_secs(2),
        )
        .unwrap();
        let body = serde_json::json!({
            "query": "query($id: Int!) { node(id: $id) { id } }",
            "variables": { "id": 17 },
        });
        let health = GhCircuit::new();
        let response = graphql_request(&client, &body, "fixture", &health)
            .await
            .unwrap();
        assert_eq!(response, serde_json::json!({"data":{"ok":true}}));

        let (method, path, authorization, content_type, accept, request_body) = seen_rx
            .recv()
            .await
            .expect("GraphQL request reached fixture");
        assert_eq!(method, reqwest::Method::POST);
        assert_eq!(path, "/graphql");
        assert_eq!(
            authorization,
            Some(HeaderValue::from_static("Bearer fixture-token"))
        );
        assert_eq!(
            content_type,
            Some(HeaderValue::from_static("application/json"))
        );
        assert_eq!(accept, Some(HeaderValue::from_static("application/json")));
        assert_eq!(
            serde_json::from_slice::<Value>(&request_body).unwrap(),
            body
        );
    });
}

#[test]
fn sdk_graphql_envelopes_fall_through_without_offline_evidence() {
    let rt = runtime();
    rt.block_on(async {
        for body in [
            r#"{"errors":[{"message":"could not resolve repository connect/dns-tls"}]}"#,
            r#"{"data":{"repository":null},"errors":[{"message":"partial failure"}]}"#,
        ] {
            let health = GhCircuit::new();
            let result = graphql_request(
                &client(200, body),
                &serde_json::json!({"query":"fixture"}),
                "fixture",
                &health,
            )
            .await;
            assert_eq!(
                result,
                Err(ForgeError::NotConfigured("GraphQL errors".into()))
            );
            assert!(result.unwrap_err().falls_through());
            assert_eq!(health.failures.load(Ordering::Relaxed), 0);
        }
    });
}

#[test]
fn typed_http_answers_are_not_global_network_failures() {
    let rt = runtime();
    rt.block_on(async {
        for (status, message, expected) in [
            (401, "Bad credentials", ForgeError::NotAuthenticated),
            (403, "Resource not accessible", ForgeError::NotAuthenticated),
            (403, "API rate limit exceeded", ForgeError::RateLimited),
            (429, "slow down", ForgeError::RateLimited),
            (
                503,
                "connect service unavailable",
                ForgeError::Other("GitHub API HTTP 503: connect service unavailable".into()),
            ),
        ] {
            let health = GhCircuit::new();
            let body = serde_json::json!({"message":message}).to_string();
            let error = graphql_request(
                &client(status, &body),
                &serde_json::json!({"query":"fixture"}),
                "fixture",
                &health,
            )
            .await
            .unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.falls_through());
            assert_eq!(
                health.failures.load(Ordering::Relaxed),
                if matches!(status, 401 | 403 | 429 | 500..=599) {
                    1
                } else {
                    0
                }
            );
        }

        // GitHub error responses remain classifiable from their status and
        // body even when the server omits Content-Type.
        for (message, expected) in [
            ("API rate limit exceeded", ForgeError::RateLimited),
            ("Resource not accessible", ForgeError::NotAuthenticated),
        ] {
            let health = GhCircuit::new();
            let body = serde_json::json!({"message":message}).to_string();
            let error = graphql_request(
                &client_with_content_type(403, &body, None, Duration::from_secs(2), None),
                &serde_json::json!({"query":"fixture"}),
                "fixture",
                &health,
            )
            .await
            .unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.falls_through());
            assert_eq!(health.failures.load(Ordering::Relaxed), 1);
        }
    });
}

#[test]
fn non_json_403_is_classified_and_retry_after_suppresses_the_next_call() {
    let rt = runtime();
    rt.block_on(async {
        let client = client_with_header(403, "<html>denied</html>", ("retry-after", "30"));
        let health = GhCircuit::new();
        let body = serde_json::json!({"query":"fixture"});

        assert_eq!(
            graphql_request(&client, &body, "fixture", &health).await,
            Err(ForgeError::RateLimited)
        );
        assert!(!ForgeError::RateLimited.falls_through());
        assert_eq!(health.active_error(), Some(ForgeError::RateLimited));
        assert_eq!(
            graphql_request(&client, &body, "fixture", &health).await,
            Err(ForgeError::RateLimited)
        );
        assert_eq!(health.failures.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn rate_limit_retry_headers_set_backoff_and_200_remains_successful() {
    let rt = runtime();
    rt.block_on(async {
        let health = GhCircuit::new();
        let body = serde_json::json!({"query":"fixture"});
        let error = graphql_request(
            &client_with_header(429, "slow down", ("retry-after", "45")),
            &body,
            "fixture",
            &health,
        )
        .await
        .unwrap_err();
        assert_eq!(error, ForgeError::RateLimited);
        assert_eq!(health.active_error(), Some(ForgeError::RateLimited));
        let mut reset_headers = reqwest::header::HeaderMap::new();
        let reset_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 120;
        reset_headers.insert(
            reqwest::header::HeaderName::from_static("x-ratelimit-reset"),
            reqwest::header::HeaderValue::from_str(&reset_epoch.to_string()).unwrap(),
        );
        reset_headers.insert(
            reqwest::header::HeaderName::from_static("x-ratelimit-remaining"),
            reqwest::header::HeaderValue::from_static("0"),
        );
        let reset_delay = retry_delay(&reset_headers, true).unwrap();
        assert!(reset_delay.as_secs() >= 119);

        let success = graphql_request(
            &client(200, r#"{"data":{"ok":true}}"#),
            &body,
            "fixture",
            &GhCircuit::new(),
        )
        .await
        .unwrap();
        assert_eq!(success, serde_json::json!({"data":{"ok":true}}));
    });
}

#[test]
fn reset_header_with_remaining_quota_does_not_open_circuit_on_first_403() {
    let rt = runtime();
    rt.block_on(async {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reset = (now + 3600).to_string();
        let remaining = "4999";
        let headers = [
            ("x-ratelimit-reset", reset.as_str()),
            ("x-ratelimit-remaining", remaining),
        ];
        let client = client_with_headers(403, r#"{"message":"Resource not accessible"}"#, &headers);
        let health = GhCircuit::new();
        let error = graphql_request(
            &client,
            &serde_json::json!({"query":"fixture"}),
            "fixture",
            &health,
        )
        .await
        .unwrap_err();
        assert_eq!(error, ForgeError::NotAuthenticated);
        assert_eq!(health.failures.load(Ordering::Relaxed), 1);
        assert_eq!(health.active_error(), None);
    });
}

#[test]
fn gate_skips_token_resolution_for_open_status_circuit() {
    let token = "gate-regression-token-unique";
    let circuit = circuit_for(token);
    circuit.record_backoff(Duration::from_secs(1), ForgeError::NotAuthenticated);

    let dir = tempfile::tempdir().unwrap();
    assert!(
        thegn_core::util::git_cmd(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        thegn_core::util::git_cmd(dir.path())
            .args(["remote", "add", "origin", "https://github.com/org/repo.git"])
            .status()
            .unwrap()
            .success()
    );
    let calls = AtomicUsize::new(0);
    let result = GithubNative::new().gate_with_token(&GitLoc::Local(dir.path().into()), || {
        calls.fetch_add(1, Ordering::Relaxed);
        Some(token.to_owned())
    });
    circuit.record_success();

    let error = result.unwrap_err();
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(error, ForgeError::NotAuthenticated);
    assert!(!error.falls_through());
}

#[test]
fn timeout_and_transport_failure_feed_native_circuit() {
    let rt = runtime();
    rt.block_on(async {
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = unused.local_addr().unwrap();
        drop(unused);
        let unreachable = TrackerHttpClient::new_with_operation_timeout(
            "github-test",
            &format!("http://{address}"),
            "Bearer fixture".into(),
            Arc::new(TrackerHttpBudget::with_permits(8)),
            Duration::from_millis(200),
        )
        .unwrap();
        let stalled = client_with_delay(
            200,
            r#"{"data":{}}"#,
            Some(Duration::from_secs(1)),
            Duration::from_millis(30),
        );
        for client in [&unreachable, &stalled] {
            let health = GhCircuit::new();
            let result = graphql_request(
                client,
                &serde_json::json!({"query":"fixture"}),
                "fixture",
                &health,
            )
            .await;
            assert_eq!(result, Err(ForgeError::Offline));
            assert_eq!(health.failures.load(Ordering::Relaxed), 1);
        }
    });
}

#[test]
fn public_host_validation_precedes_credentials() {
    for url in [
        "https://ghe.example/org/repo.git",
        "git@gitlab.example:org/repo.git",
        "https://github.com.evil.example/org/repo",
        "https://github.com@evil.example/org/repo",
    ] {
        assert_eq!(parse_owner_repo(url), None);
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(
        thegn_core::util::git_cmd(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        thegn_core::util::git_cmd(dir.path())
            .args([
                "remote",
                "add",
                "origin",
                "https://ghe.example/org/repo.git"
            ])
            .status()
            .unwrap()
            .success()
    );
    let result = GithubNative::new().gate_with_token(&GitLoc::Local(dir.path().into()), || {
        panic!("must not ask for credentials")
    });
    assert!(matches!(result, Err(ForgeError::NotConfigured(_))));
    assert_eq!(
        super::super::github(true).layers.len(),
        1,
        "enterprise must go directly to its CLI"
    );
}

#[test]
fn strict_origin_identity_rejects_foreign_path_and_authority_confusion_before_tokens() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        thegn_core::util::git_cmd(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    let loc = GitLoc::Local(dir.path().into());
    let native = GithubNative::new();
    let set_origin = |origin: &str| {
        assert!(
            thegn_core::util::git_cmd(dir.path())
                .args(["config", "remote.origin.url", origin])
                .status()
                .unwrap()
                .success()
        );
    };
    for origin in [
        "https://evil.example/foo@github.com/bar",
        "https://evil.example/org/repo?next=@github.com/a/b",
        "https://evil.example/org/repo#@github.com/a/b",
        "ssh://git@evil.example/foo@github.com/bar",
        "https://github.com.evil.example/org/repo",
        "https://github.com@evil.example/org/repo",
        "https://github.com:443@evil.example/org/repo",
        "https://user@github.com/org/repo",
        "https://github.com:443/org/repo",
        "ssh://git@github.com:22/org/repo",
        "https://github.com/org/repo?query=value",
        "https://github.com/org/repo#fragment",
        "https://github.com/org/repo/extra",
        "https://github.com//repo",
        "https://github.com/../repo",
        "https://github.com/org/%2e%2e",
        "https://github.com/org/repo\nextra",
        "/private/github.com/org/repo",
    ] {
        assert_eq!(parse_owner_repo(origin), None, "{origin:?}");
        set_origin(origin);
        let token_calls = std::cell::Cell::new(0);
        let result = native.gate_with_token(&loc, || {
            token_calls.set(token_calls.get() + 1);
            Some("private-never-sent-token".into())
        });
        assert!(
            matches!(&result, Err(ForgeError::NotConfigured(message))
                if message.as_ref() == "origin is not a public GitHub remote"),
            "{origin:?}: {result:?}"
        );
        assert_eq!(token_calls.get(), 0, "foreign origin reached credentials");
    }
    // A circuit-only refusal cannot satisfy these controls: valid origins must
    // reach the injected token callback and preserve this same parsed identity.
    for origin in [
        "https://github.com/org/repo",
        "https://GitHub.com/org/repo.git",
        "ssh://git@github.com/org/repo.git",
        "git@github.com:org/repo.git",
    ] {
        set_origin(origin);
        let token_calls = std::cell::Cell::new(0);
        let result = native
            .gate_with_token(&loc, || {
                token_calls.set(token_calls.get() + 1);
                Some("private-never-sent-token".into())
            })
            .unwrap();
        assert_eq!(
            result,
            (
                "private-never-sent-token".into(),
                "org".into(),
                "repo".into()
            )
        );
        assert_eq!(token_calls.get(), 1);
    }
    // No request method is called; the injected token only proves admission.
    dir.close().expect("private origin fixture cleanup");
}

pub(super) struct Layer {
    pub(super) result: Result<Vec<PrHeader>, ForgeError>,
    pub(super) calls: Arc<AtomicUsize>,
}
impl Probe for Layer {
    fn probe(&self) -> ProbeReport {
        ProbeReport::new("forge", "fixture", Availability::Ready)
    }
}
impl Forge for Layer {
    fn repo_ref(&self, _: &GitLoc) -> Option<RepoRef> {
        None
    }
    fn pr_status(&self, _: &GitLoc, _: PrRef) -> Result<PrStatus, ForgeError> {
        Err(ForgeError::Unsupported("fixture pr_status"))
    }
    fn id(&self) -> &'static str {
        "fixture"
    }
    fn caps(&self) -> ForgeCaps {
        ForgeCaps {
            pr_list: true,
            ..Default::default()
        }
    }
    fn pr_list(&self, _: &GitLoc, _: usize) -> Result<Vec<PrHeader>, ForgeError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.result.clone()
    }
}

#[test]
fn sdk_error_classification_drives_the_real_ladder() {
    let rt = runtime();
    let error = rt.block_on(async {
        graphql_request(
            &client(200, r#"{"errors":[{"message":"unknown repository"}]}"#),
            &serde_json::json!({"query":"fixture"}),
            "fixture",
            &GhCircuit::new(),
        )
        .await
        .unwrap_err()
    });
    for (error, fallback_count) in [
        (error, 1),
        (ForgeError::NotAuthenticated, 0),
        (ForgeError::Offline, 0),
    ] {
        let native_calls = Arc::new(AtomicUsize::new(0));
        let cli_calls = Arc::new(AtomicUsize::new(0));
        let ladder = crate::seam::Ladder::new(
            "fixture",
            vec![
                Box::new(Layer {
                    result: Err(error),
                    calls: native_calls.clone(),
                }) as Box<dyn Forge>,
                Box::new(Layer {
                    result: Ok(vec![]),
                    calls: cli_calls.clone(),
                }) as Box<dyn Forge>,
            ],
        );
        assert_eq!(
            ladder
                .pr_list(&GitLoc::Local("/unused".into()), 100)
                .is_ok(),
            fallback_count == 1
        );
        assert_eq!(native_calls.load(Ordering::Relaxed), 1);
        assert_eq!(cli_calls.load(Ordering::Relaxed), fallback_count);
    }
}

fn shell(script: &str) -> std::process::Command {
    let mut command = std::process::Command::new("sh");
    command.args(["-c", script]);
    command
}

#[test]
fn token_helper_success_failure_and_output_bound() {
    assert_eq!(
        token_command(shell("printf ' fixture-token \\n'"), Duration::from_secs(1)),
        Some("fixture-token".into())
    );
    assert_eq!(
        token_command(
            shell("printf 'do-not-return'; exit 3"),
            Duration::from_secs(1)
        ),
        None
    );
    assert_eq!(
        token_command(shell("printf '   '"), Duration::from_secs(1)),
        None
    );
    assert_eq!(
        token_command(shell("head -c 17000 /dev/zero"), Duration::from_secs(1)),
        None
    );
}

#[test]
fn token_timeout_kills_descendants_even_after_parent_exit() {
    for parent_exits in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut command = shell(if parent_exits {
            "echo $$ > parent.pid; sleep 30 & echo $! > descendant.pid; exit 0"
        } else {
            "echo $$ > parent.pid; sleep 30 & echo $! > descendant.pid; wait"
        });
        command.current_dir(dir.path());
        let start = std::time::Instant::now();
        assert_eq!(token_command(command, Duration::from_millis(200)), None);
        assert!(start.elapsed() < Duration::from_secs(3));
        for file in ["parent.pid", "descendant.pid"] {
            let pid = std::fs::read_to_string(dir.path().join(file)).unwrap();
            // A killed orphan may briefly await the OS's reaper (Z), but must
            // have no running process and no inherited credential pipe left.
            for attempt in 0..50 {
                let out = std::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", pid.trim()])
                    .output()
                    .unwrap();
                let state = String::from_utf8_lossy(&out.stdout);
                if state.trim().is_empty() || state.trim().starts_with('Z') {
                    break;
                }
                assert!(
                    attempt < 49,
                    "credential descendant {} still running: {}",
                    pid.trim(),
                    state
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            let fds = std::path::PathBuf::from(format!("/proc/{}/fd", pid.trim()));
            if let Ok(fds) = std::fs::read_dir(fds) {
                assert_eq!(fds.count(), 0);
            }
        }
    }
}

#[test]
fn helper_capacity_stays_reserved_until_ownership_is_released() {
    static BUDGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let permits: Vec<_> = (0..MAX_TOKEN_HELPERS)
        .map(|_| TokenPermit::acquire(&BUDGET).unwrap())
        .collect();
    assert!(TokenPermit::acquire(&BUDGET).is_none());
    let (release, receive) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        receive.recv().unwrap();
        drop(permits);
    });
    assert!(
        TokenPermit::acquire(&BUDGET).is_none(),
        "delayed ownership still occupies capacity"
    );
    release.send(()).unwrap();
    owner.join().unwrap();
    assert!(TokenPermit::acquire(&BUDGET).is_some());
}

#[test]
fn cancelling_token_future_terminates_owned_group() {
    let dir = tempfile::tempdir().unwrap();
    let mut command = shell("echo $$ > parent.pid; sleep 30 & echo $! > descendant.pid; wait");
    command.current_dir(dir.path());
    runtime().block_on(async {
        assert!(
            tokio::time::timeout(
                Duration::from_millis(200),
                token_command_async(command, Duration::from_secs(30))
            )
            .await
            .is_err()
        );
    });
    for file in ["parent.pid", "descendant.pid"] {
        let pid = std::fs::read_to_string(dir.path().join(file)).unwrap();
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&out.stdout);
        assert!(
            state.trim().is_empty() || state.trim().starts_with('Z'),
            "running helper after cancellation: {state}"
        );
        let fds = std::path::PathBuf::from(format!("/proc/{}/fd", pid.trim()));
        if let Ok(fds) = std::fs::read_dir(fds) {
            assert_eq!(fds.count(), 0);
        }
    }
}

#[test]
fn reaper_spawn_failure_retains_child_and_capacity() {
    static BUDGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    static PENDING: std::sync::Mutex<Vec<ReapJob>> = std::sync::Mutex::new(Vec::new());
    let child = shell("exit 0").spawn().unwrap();
    defer_reap_with(
        ReapJob {
            child,
            _permit: Some(TokenPermit::acquire(&BUDGET).unwrap()),
        },
        &PENDING,
        |_| Err(std::io::Error::other("injected thread creation failure")),
    );
    assert_eq!(BUDGET.load(Ordering::Acquire), 1);
    let mut pending = PENDING.lock().unwrap();
    assert_eq!(pending.len(), 1);
    let mut job = pending.pop().unwrap();
    drop(pending);
    assert!(job.child.wait().unwrap().success());
    drop(job);
    assert_eq!(BUDGET.load(Ordering::Acquire), 0);
}

#[test]
fn unknown_wait_ownership_is_reaped_without_group_signal() {
    static BUDGET: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = tempfile::tempdir().unwrap();
    let mut command = shell("sleep 0.1; touch completed");
    command.current_dir(dir.path());
    crate::plugin::proc::set_process_group(&mut command);
    let mut owned = TokenProcess {
        child: Some(command.spawn().unwrap()),
        permit: Some(TokenPermit::acquire(&BUDGET).unwrap()),
        can_signal: true,
    };
    assert!(
        owned
            .observe_exit(Err(std::io::Error::other("injected ownership uncertainty")))
            .is_err()
    );
    assert!(!owned.can_signal);
    drop(owned);
    for attempt in 0..100 {
        if BUDGET.load(Ordering::Acquire) == 0 {
            break;
        }
        assert!(attempt < 99, "reaper did not finish");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        dir.path().join("completed").exists(),
        "unknown ownership must never trigger a group signal"
    );
}

#[test]
fn failure_windows_grow_with_deterministic_jitter_and_cap() {
    for failures in 0..CIRCUIT_OPEN_AFTER {
        assert_eq!(failure_open_window(failures, 7), None);
    }
    for (failures, base) in [(3, 60), (4, 120), (6, 480), (8, 900), (u32::MAX, 900)] {
        assert_eq!(
            failure_open_window(failures, 0),
            Some(Duration::from_secs(base))
        );
        let first = failure_open_window(failures, 17).unwrap();
        let second = failure_open_window(failures, 23).unwrap();
        assert_ne!(first, second);
        assert!(first.as_secs() <= MAX_RETRY_AFTER_SECS);
        assert!(second.as_secs() <= MAX_RETRY_AFTER_SECS);
    }
}

#[test]
fn malformed_and_server_error_responses_open_the_circuit_and_suppress_requests() {
    let rt = runtime();
    rt.block_on(async {
        let fixtures = [
            (502, "<html>bad gateway</html>", None),
            (200, "not json", Some("application/json")),
            (200, "<html>proxy error</html>", Some("text/html")),
        ];
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = unused.local_addr().unwrap();
        drop(unused);
        let unreachable = TrackerHttpClient::new_with_operation_timeout(
            "github-test",
            &format!("http://{address}"),
            "Bearer fixture".into(),
            Arc::new(TrackerHttpBudget::with_permits(8)),
            Duration::from_millis(200),
        )
        .unwrap();
        for (status, text, content_type) in fixtures {
            let health = GhCircuit::new();
            let client =
                client_with_content_type(status, text, None, Duration::from_secs(2), content_type);
            for _ in 0..3 {
                let result = graphql_request(
                    &client,
                    &serde_json::json!({"query":"fixture"}),
                    "fixture",
                    &health,
                )
                .await;
                assert!(result.is_err());
            }
            let held = health.active_error().expect("third failure opens circuit");
            assert_eq!(health.failures.load(Ordering::Relaxed), 3);
            assert_eq!(
                graphql_request(
                    &unreachable,
                    &serde_json::json!({"query":"fixture"}),
                    "fixture",
                    &health,
                )
                .await,
                Err(held)
            );
            assert_eq!(health.failures.load(Ordering::Relaxed), 3);
        }
    });
}

#[test]
fn server_retry_after_opens_immediately_and_success_resets_failures() {
    let rt = runtime();
    rt.block_on(async {
        let health = GhCircuit::new();
        let server_error =
            client_with_header(503, "<html>unavailable</html>", ("retry-after", "30"));
        assert!(
            graphql_request(
                &server_error,
                &serde_json::json!({"query":"fixture"}),
                "fixture",
                &health,
            )
            .await
            .is_err()
        );
        assert!(health.active_error().is_some());
        assert_eq!(health.failures.load(Ordering::Relaxed), 1);

        health.record_success();
        assert_eq!(health.active_error(), None);
        assert_eq!(health.failures.load(Ordering::Relaxed), 0);
        let retryable = client(503, "<html>unavailable</html>");
        assert!(
            graphql_request(
                &retryable,
                &serde_json::json!({"query":"fixture"}),
                "fixture",
                &health,
            )
            .await
            .is_err()
        );
        assert_eq!(health.active_error(), None);
        assert_eq!(health.failures.load(Ordering::Relaxed), 1);
    });
}

#[test]
fn http_failure_window_grows_after_expiration() {
    let health = GhCircuit::new();
    for _ in 0..3 {
        health.record_http_failure(ForgeError::Other("failure".into()));
    }
    health.open_for(Duration::ZERO, ForgeError::Other("expired".into()));
    health.record_http_failure(ForgeError::Other("failure".into()));
    let guard = health.open_until.lock().unwrap();
    let remaining = guard
        .unwrap()
        .saturating_duration_since(std::time::Instant::now());
    assert!(remaining > Duration::from_secs(60));
}

#[test]
fn poll_backoff_only_passes_final_api_errors() {
    assert_eq!(
        poll_backoff_from(Some(ForgeError::RateLimited)),
        Some(ForgeError::RateLimited)
    );
    let other = ForgeError::Other("bad response".into());
    assert_eq!(poll_backoff_from(Some(other.clone())), Some(other));
    assert_eq!(poll_backoff_from(Some(ForgeError::NotAuthenticated)), None);
    assert_eq!(
        poll_backoff_from(Some(ForgeError::NotConfigured("offline".into()))),
        None
    );
    assert_eq!(poll_backoff_from(None), None);
}

#[test]
fn open_circuit_gates_github_cli_poll_ladder() {
    let token = "poll-gate-regression-token-unique";
    let circuit = circuit_for(token);
    circuit.record_backoff(Duration::from_secs(30), ForgeError::RateLimited);
    let dir = tempfile::tempdir().unwrap();
    assert!(
        thegn_core::util::git_cmd(dir.path())
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        thegn_core::util::git_cmd(dir.path())
            .args(["remote", "add", "origin", "https://github.com/org/repo.git"])
            .status()
            .unwrap()
            .success()
    );
    let loc = GitLoc::Local(dir.path().into());
    let forge = super::super::github(false);
    let issue_result = forge.issue_rows(&loc, 10);
    let search_result = forge.search_prs(&loc, thegn_core::forge::PrRole::Author, None, 10);
    circuit.record_success();
    assert!(matches!(issue_result, Err(ForgeError::RateLimited)));
    assert!(matches!(search_result, Err(ForgeError::RateLimited)));

    let remote = GitLoc::Remote {
        ssh: thegn_core::remote::SshTarget::plain("host".into(), 22, false),
        path: "/repo".into(),
    };
    assert!(matches!(
        GithubNative::new().issue_rows(&remote, 10),
        Err(ForgeError::Unsupported(_))
    ));
}
