//! Actual SDK-to-global-state assertions run in one exact owned test child.
//! The ordinary parent is safe alongside other tests that report connectivity.
use super::regression_tests::{Layer, client, runtime};
use super::*;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use thegn_core::connectivity::{self, Connectivity};

#[path = "../../../../test/support/owned_test_child.rs"]
mod owned_test_child;
use owned_test_child::OwnedChild;

const CHILD_ENTRY: &str =
    "forge::native::global_connectivity_tests::isolated_sdk_connectivity_child";
const CHILD_MARKER: &str = "THEGN_TEST_FORGE_CONNECTIVITY_CHILD";
const RECEIPT_PREFIX: &str = "THE622_CONNECTIVITY_RECEIPT ";
const OUTPUT_LIMIT: u64 = 1024 * 1024;
const EXECUTION_BUDGET: Duration = Duration::from_secs(10);
const CLEANUP_BUDGET: Duration = Duration::from_secs(1);

fn expected_receipt() -> Value {
    serde_json::json!({
        "schema_version": 1,
        "cases": 11,
        "reachable_answers": 9,
        "transport_failures": 2,
        "native_calls": 11,
        "cli_calls": 2,
        "final_errors": 8,
        "native_successes": 1,
    })
}

fn bounded_text(path: &Path) -> String {
    let mut bytes = Vec::new();
    File::open(path)
        .unwrap()
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() as u64 <= OUTPUT_LIMIT, "fixture output bound");
    String::from_utf8(bytes).expect("fixture output is UTF-8")
}

#[test]
fn sdk_results_preserve_global_connectivity_and_fallback() {
    let root = Arc::new(tempfile::tempdir().unwrap());
    for name in [
        "cwd",
        "home",
        "app",
        "config",
        "state",
        "data",
        "cache",
        "runtime",
        "empty-path",
    ] {
        fs::create_dir(root.path().join(name)).unwrap();
    }
    let stdout_path = root.path().join("stdout");
    let stderr_path = root.path().join("stderr");
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args([
            "--exact",
            CHILD_ENTRY,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env(CHILD_MARKER, "matrix-v1")
        .env("HOME", root.path().join("home"))
        .env("USERPROFILE", root.path().join("home"))
        .env("THEGN_DIR", root.path().join("app"))
        .env("APPDATA", root.path().join("config"))
        .env("LOCALAPPDATA", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
        .env("PATH", root.path().join("empty-path"))
        .current_dir(root.path().join("cwd"))
        .stdin(Stdio::null())
        .stdout(File::create_new(&stdout_path).unwrap())
        .stderr(File::create_new(&stderr_path).unwrap());
    // Windows may need its standard loader root; no credential/config env is inherited.
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let mut child = OwnedChild::spawn(&mut command, Arc::clone(&root));
    drop(command);
    let deadline = Instant::now() + EXECUTION_BUDGET;
    let status = loop {
        let output_bytes = fs::metadata(&stdout_path)
            .unwrap()
            .len()
            .saturating_add(fs::metadata(&stderr_path).unwrap().len());
        if output_bytes > OUTPUT_LIMIT {
            assert!(
                child.terminate(CLEANUP_BUDGET),
                "overflow cleanup retained custody"
            );
            panic!("owned SDK fixture exceeded its monitored output limit");
        }
        if let Some(status) = child.poll() {
            break status;
        }
        if Instant::now() >= deadline {
            assert!(
                child.terminate(CLEANUP_BUDGET),
                "deadline cleanup retained custody"
            );
            panic!(
                "owned SDK fixture deadline; stdout={}, stderr={}",
                bounded_text(&stdout_path),
                bounded_text(&stderr_path)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    drop(child);
    let stdout = bounded_text(&stdout_path);
    let stderr = bounded_text(&stderr_path);
    assert!((stdout.len() + stderr.len()) as u64 <= OUTPUT_LIMIT);
    assert!(
        status.success(),
        "SDK fixture failed: {status}; stdout={stdout}; stderr={stderr}"
    );
    assert!(stdout.lines().any(|line| line == "running 1 test"));
    assert_eq!(
        stdout.matches(&format!("test {CHILD_ENTRY} ...")).count(),
        1
    );
    assert!(stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"));
    let receipts: Vec<_> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix(RECEIPT_PREFIX))
        .collect();
    assert_eq!(receipts.len(), 1, "one completed matrix receipt");
    assert_eq!(
        serde_json::from_str::<Value>(receipts[0]).unwrap(),
        expected_receipt()
    );
    Arc::try_unwrap(root)
        .expect("unsettled fixture retains its private directory")
        .close()
        .expect("strict owned SDK fixture cleanup");
}

fn header(number: u64) -> PrHeader {
    PrHeader {
        number,
        head_ref: format!("fixture-{number}"),
        state: "OPEN".into(),
        url: format!("https://github.com/fixture/repo/pull/{number}"),
        is_draft: false,
    }
}

fn assert_ladder(result: Result<Vec<PrHeader>, ForgeError>, fallback: bool) {
    let native_calls = Arc::new(AtomicUsize::new(0));
    let cli_calls = Arc::new(AtomicUsize::new(0));
    let fallback_rows = vec![header(99)];
    let expected = if fallback {
        Ok(fallback_rows.clone())
    } else {
        result.clone()
    };
    let ladder = crate::seam::Ladder::new(
        "isolated SDK fixture",
        vec![
            Box::new(Layer {
                result,
                calls: Arc::clone(&native_calls),
            }) as Box<dyn Forge>,
            Box::new(Layer {
                result: Ok(fallback_rows),
                calls: Arc::clone(&cli_calls),
            }) as Box<dyn Forge>,
        ],
    );
    assert_eq!(
        ladder.pr_list(&GitLoc::Local("unused-fixture".into()), 100),
        expected
    );
    assert_eq!(native_calls.load(Ordering::Relaxed), 1);
    assert_eq!(cli_calls.load(Ordering::Relaxed), usize::from(fallback));
}

fn seed(offline: bool) {
    connectivity::report_success();
    assert_eq!(connectivity::current(), Connectivity::Online);
    assert_eq!(connectivity::consecutive_failures(), 0);
    if offline {
        connectivity::report_failure();
        assert_eq!(connectivity::current(), Connectivity::Offline);
        assert_eq!(connectivity::consecutive_failures(), 1);
    }
}

#[test]
#[ignore = "helper only: exact owned reexec; global connectivity matrix"]
fn isolated_sdk_connectivity_child() {
    assert_eq!(
        std::env::var(CHILD_MARKER).as_deref(),
        Ok("matrix-v1"),
        "exact owned helper marker required"
    );
    connectivity::install_forced(None);
    connectivity::install_thresholds(1, 30_000);
    let data = serde_json::json!({"repository":{"pullRequests":{
        "nodes":[header(7)],
        "pageInfo":{"hasNextPage":false,"endCursor":null}
    }}});
    let cases = [
        (
            "errors-only",
            200,
            serde_json::json!({"errors":[{"message":"unknown connect/dns-tls repository"}]}),
            Some(ForgeError::NotConfigured("GraphQL errors".into())),
        ),
        (
            "partial",
            200,
            serde_json::json!({"data":data.clone(),"errors":[{"message":"partial connect/dns-tls failure"}]}),
            Some(ForgeError::NotConfigured("GraphQL errors".into())),
        ),
        (
            "authentication",
            401,
            serde_json::json!({"message":"Bad credentials connect/dns-tls"}),
            Some(ForgeError::NotAuthenticated),
        ),
        (
            "access",
            403,
            serde_json::json!({"message":"Resource not accessible"}),
            Some(ForgeError::NotAuthenticated),
        ),
        (
            "rate-403",
            403,
            serde_json::json!({"message":"API rate limit exceeded"}),
            Some(ForgeError::RateLimited),
        ),
        (
            "rate-429",
            429,
            serde_json::json!({"message":"slow down"}),
            Some(ForgeError::RateLimited),
        ),
        (
            "server",
            503,
            serde_json::json!({"message":"connect service unavailable"}),
            Some(ForgeError::Other(
                "GitHub API HTTP 503: connect service unavailable".into(),
            )),
        ),
        (
            "repository",
            404,
            serde_json::json!({"message":"repository connect/dns-tls missing"}),
            Some(ForgeError::Other(
                "GitHub API HTTP 404: repository connect/dns-tls missing".into(),
            )),
        ),
        ("success", 200, serde_json::json!({"data":data}), None),
    ];
    assert_eq!(cases.len(), 9);
    runtime().block_on(async {
        for (name, status, body, expected_error) in cases {
            seed(true);
            let health = GhCircuit::new();
            let result = graphql_request(
                &client(status, &body.to_string()),
                &serde_json::json!({"query":"private fixture"}),
                "fixture",
                Duration::from_secs(1),
                &health,
            )
            .await;
            assert_eq!(
                connectivity::current(),
                Connectivity::Online,
                "reachable {name} must publish global success"
            );
            assert_eq!(
                connectivity::consecutive_failures(),
                0,
                "global failures for {name}"
            );
            assert_eq!(
                health.failures.load(Ordering::Relaxed),
                0,
                "local failures for {name}"
            );
            if let Some(expected) = &expected_error {
                assert_eq!(result, Err(expected.clone()), "typed result for {name}");
            } else {
                assert_eq!(
                    parse_graphql_pr_list(result.as_ref().unwrap()),
                    vec![header(7)]
                );
            }
            let fallback = matches!(expected_error, Some(ForgeError::NotConfigured(_)));
            assert_ladder(result.map(|value| parse_graphql_pr_list(&value)), fallback);
        }
        for stall in [false, true] {
            seed(false);
            let service = tower::service_fn(
                move |_: axum::http::Request<octocrab::OctoBody>| async move {
                    if stall {
                        std::future::pending::<()>().await;
                    }
                    Err::<axum::http::Response<http_body_util::Full<axum::body::Bytes>>, _>(
                        std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            "private fixture transport",
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
                &serde_json::json!({"query":"private fixture"}),
                "fixture",
                Duration::from_millis(30),
                &health,
            )
            .await;
            assert_eq!(result, Err(ForgeError::Offline));
            assert_eq!(connectivity::current(), Connectivity::Offline);
            assert_eq!(connectivity::consecutive_failures(), 1);
            assert_eq!(health.failures.load(Ordering::Relaxed), 1);
            assert_ladder(result.map(|value| parse_graphql_pr_list(&value)), false);
        }
    });
    // Leading newline separates the receipt from libtest's test-name prefix.
    writeln!(
        std::io::stdout(),
        "\n{RECEIPT_PREFIX}{}",
        expected_receipt()
    )
    .unwrap();
    std::io::stdout().flush().unwrap();
}
