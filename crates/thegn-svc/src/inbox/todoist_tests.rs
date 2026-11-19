use super::*;
use thegn_core::config::InboxKind;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOKEN: &str = "sekrit-todoist-token-123";

/// Serve each canned raw HTTP response once, in order; returns (base, requests).
async fn fake(responses: Vec<String>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        for response in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            if sock.write_all(response.as_bytes()).await.is_err() {
                return;
            }
            if sock.shutdown().await.is_err() {
                return;
            }
        }
    });
    (base, seen)
}

fn http(status: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
}

fn backend(base: &str) -> TodoistBackend {
    // SAFETY-free: the env ref is resolved through the svc fallback.
    unsafe { std::env::set_var("TODOIST_TEST_TOKEN_B163", TOKEN) };
    TodoistBackend::new(&InboxAccount {
        name: "t".into(),
        kind: InboxKind::Todoist,
        token: "env:TODOIST_TEST_TOKEN_B163".into(),
        filter: "today".into(),
        base_url: base.into(),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test]
async fn two_pages_are_joined_and_mapped() {
    let p1 = r#"{"results":[{"id":"a1","content":"One","priority":4,"labels":["x"],"due":{"date":"2026-11-20"}}],"next_cursor":"c2"}"#;
    let p2 = r#"{"results":[{"id":"b2","content":"Two","priority":1,"labels":[],"due":null}],"next_cursor":null}"#;
    let (base, seen) = fake(vec![http("200 OK", "", p1), http("200 OK", "", p2)]).await;
    let page = backend(&base).fetch("", 10).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].id, "a1");
    assert_eq!(page.items[0].priority, Some(4));
    assert_eq!(page.items[0].due.as_deref(), Some("2026-11-20"));
    assert_eq!(page.items[0].url, "https://app.todoist.com/app/task/a1");
    assert_eq!(page.items[0].from, "");
    assert_eq!(page.items[1].priority, Some(1));
    assert_eq!(page.mode, PageMode::Snapshot);
    assert!(!page.truncated);
    let reqs = seen.lock().unwrap();
    assert!(
        reqs[0].contains("/api/v1/tasks/filter?query=today"),
        "{}",
        reqs[0]
    );
    assert!(reqs[1].contains("cursor=c2"));
    assert!(
        reqs[0]
            .to_lowercase()
            .contains(&format!("bearer {TOKEN}").to_lowercase())
    );
}

#[tokio::test]
async fn unauthorized_is_auth_and_hides_the_token() {
    let (base, _) = fake(vec![http("401 Unauthorized", "", "{}")]).await;
    let err = backend(&base).fetch("", 10).await.unwrap_err();
    assert!(matches!(err, InboxError::Auth(_)), "{err:?}");
    assert!(!err.to_string().contains(TOKEN));
    assert!(!format!("{err:?}").contains(TOKEN));
}

#[tokio::test]
async fn rate_limited_carries_retry_after() {
    let (base, _) = fake(vec![http(
        "429 Too Many Requests",
        "retry-after: 17\r\n",
        "{}",
    )])
    .await;
    let err = backend(&base).fetch("", 10).await.unwrap_err();
    assert_eq!(err, InboxError::RateLimited(Some(17)));
}

#[tokio::test]
async fn server_error_is_network() {
    let (base, _) = fake(vec![http("503 Service Unavailable", "", "{}")]).await;
    let err = backend(&base).fetch("", 10).await.unwrap_err();
    assert!(matches!(err, InboxError::Network(_)), "{err:?}");
}

#[tokio::test]
async fn oversize_body_is_body_limit() {
    let big = format!(
        r#"{{"results":[],"pad":"{}"}}"#,
        "x".repeat(MAX_BODY_BYTES + 10)
    );
    let (base, _) = fake(vec![http("200 OK", "", &big)]).await;
    let err = backend(&base).fetch("", 10).await.unwrap_err();
    assert_eq!(err, InboxError::BodyLimit);
}

#[tokio::test]
async fn max_items_truncates() {
    let p = r#"{"results":[{"id":"1","content":"a"},{"id":"2","content":"b"},{"id":"3","content":"c"}],"next_cursor":null}"#;
    let (base, _) = fake(vec![http("200 OK", "", p)]).await;
    let page = backend(&base).fetch("", 2).await.unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.truncated);
}

#[tokio::test]
async fn unresolved_token_is_auth_without_network() {
    let b = TodoistBackend::new(&InboxAccount {
        name: "t".into(),
        kind: InboxKind::Todoist,
        token: "env:THEGN_B163_DEFINITELY_UNSET".into(),
        base_url: "http://127.0.0.1:1".into(),
        ..Default::default()
    })
    .unwrap();
    let err = b.fetch("", 5).await.unwrap_err();
    assert!(matches!(err, InboxError::Auth(_)), "{err:?}");
}
