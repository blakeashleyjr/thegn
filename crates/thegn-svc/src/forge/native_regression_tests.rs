use super::*;
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use thegn_core::seam::SeamError;

pub(super) fn client(status: u16, body: &str) -> octocrab::Octocrab {
    let body = body.to_owned();
    let service = tower::service_fn(move |_: axum::http::Request<octocrab::OctoBody>| {
        let body = body.clone();
        async move {
            Ok::<_, std::io::Error>(
                axum::http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(http_body_util::Full::new(axum::body::Bytes::from(body)))
                    .unwrap(),
            )
        }
    });
    octocrab::OctocrabBuilder::new_empty()
        .with_service(service)
        .with_auth(octocrab::AuthState::None)
        .build()
        .unwrap()
}

pub(super) fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn runtime_failure_keeps_dynamic_message_in_owned_not_configured_payload() {
    let error = runtime_error("fixture runtime failure");
    let ForgeError::NotConfigured(message) = error else {
        panic!("runtime failure was not classified as not configured");
    };
    assert!(matches!(&message, Cow::Owned(_)));
    assert_eq!(message, "no runtime: fixture runtime failure");
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
                Duration::from_secs(1),
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
                Duration::from_secs(1),
                &health,
            )
            .await
            .unwrap_err();
            assert_eq!(error, expected);
            assert!(!error.falls_through());
            assert_eq!(health.failures.load(Ordering::Relaxed), 0);
        }
    });
}

#[test]
fn timeout_and_transport_failure_feed_native_circuit() {
    let rt = runtime();
    rt.block_on(async {
        for stall in [false, true] {
            let service = tower::service_fn(
                move |_: axum::http::Request<octocrab::OctoBody>| async move {
                    if stall {
                        std::future::pending::<()>().await;
                    }
                    Err::<axum::http::Response<http_body_util::Full<axum::body::Bytes>>, _>(
                        std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            "fixture transport",
                        ),
                    )
                },
            );
            let client = octocrab::OctocrabBuilder::new_empty()
                .with_service(service)
                .with_auth(octocrab::AuthState::None)
                .build()
                .unwrap();
            let health = GhCircuit::new();
            let result = graphql_request(
                &client,
                &serde_json::json!({"query":"fixture"}),
                "fixture",
                Duration::from_millis(30),
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
                if message.to_string() == "origin is not a public GitHub remote"),
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
            Duration::from_secs(1),
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
