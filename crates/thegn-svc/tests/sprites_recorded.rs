//! Replay the documented Sprites OpenAPI example bodies without network access.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use thegn_svc::provider::{InventoryReport, SpritesProvider};
use thegn_svc::sprites::inspect::parse_info;
use thegn_svc::sprites::usage::{gb_hours_from_byte_seconds, parse_usage_reading};

const SPRITE: &str = include_str!("fixtures/sprites/sprite.json");
const LIST_PAGE_1: &str = include_str!("fixtures/sprites/list_page_1.json");
const LIST_PAGE_2: &str = include_str!("fixtures/sprites/list_page_2.json");
const USAGE_UNREPORTED: &str = include_str!("fixtures/sprites/usage_unreported.json");
const USAGE_RECORDED: &str = include_str!("fixtures/sprites/usage_recorded.json");

struct Mock {
    base_url: String,
}

fn start_mock(usage_body: &'static str, omit_has_more: bool) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || handle(stream, usage_body, omit_has_more));
        }
    });
    Mock {
        base_url: format!("http://127.0.0.1:{port}/v1"),
    }
}

fn read_line(reader: &mut TcpStream, out: &mut String) -> Option<()> {
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => return if out.is_empty() { None } else { Some(()) },
            Ok(_) => {
                out.push(byte[0] as char);
                if byte[0] == b'\n' {
                    return Some(());
                }
            }
            Err(_) => return None,
        }
    }
}

fn handle(mut stream: TcpStream, usage_body: &'static str, omit_has_more: bool) {
    let mut line = String::new();
    if read_line(&mut stream, &mut line).is_none() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    loop {
        let mut header = String::new();
        if read_line(&mut stream, &mut header).is_none() || header.trim_end().is_empty() {
            break;
        }
    }

    let path = target.split('?').next().unwrap_or(target);
    let (status, body) = match (method, path) {
        ("GET", "/v1/sprites") if target.contains("continuation_token=") => {
            ("200 OK", LIST_PAGE_2.as_bytes().to_vec())
        }
        ("GET", "/v1/sprites") if omit_has_more => {
            let mut page: serde_json::Value = serde_json::from_str(LIST_PAGE_1).unwrap();
            page.as_object_mut().unwrap().remove("has_more");
            ("200 OK", serde_json::to_vec(&page).unwrap())
        }
        ("GET", "/v1/sprites") => ("200 OK", LIST_PAGE_1.as_bytes().to_vec()),
        ("GET", "/v1/sprites/my-dev-sprite") => ("200 OK", SPRITE.as_bytes().to_vec()),
        ("GET", "/v1/usage/demo") => ("200 OK", usage_body.as_bytes().to_vec()),
        _ => ("404 Not Found", b"not found".to_vec()),
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

#[test]
fn documented_sprite_parses() {
    let value: serde_json::Value = serde_json::from_str(SPRITE).unwrap();
    let info = parse_info(&value).unwrap();
    assert_eq!(info.name, "my-dev-sprite");
    assert_eq!(info.status, "cold");
    assert_eq!(info.created_at.as_deref(), Some("2024-01-15T10:30:00Z"));
}

#[tokio::test]
async fn documented_list_pages_give_a_complete_inventory_with_states() {
    let mock = start_mock(USAGE_UNREPORTED, false);
    let provider = SpritesProvider::new(&mock.base_url, "test-token", "my-dev-sprite");
    let InventoryReport::Complete(rows) = provider.sandbox_inventory().await.unwrap() else {
        panic!("documented pages should produce a complete inventory");
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "my-dev-sprite");
    assert_eq!(rows[0].name.as_deref(), Some("sprite-abc123"));
    assert_eq!(rows[0].state.as_deref(), Some("running"));
    assert_eq!(rows[1].id, "my-cold-sprite");
    assert_eq!(rows[1].state.as_deref(), Some("cold"));
    assert_eq!(rows[1].created_at_ms, Some(1_705_314_600_000));
}

#[tokio::test]
async fn inspect_over_http_reads_the_documented_body() {
    let mock = start_mock(USAGE_UNREPORTED, false);
    let provider = SpritesProvider::new(&mock.base_url, "test-token", "my-dev-sprite");
    let info = provider.inspect("my-dev-sprite").await.unwrap().unwrap();
    assert_eq!(info.name, "my-dev-sprite");
    assert_eq!(info.status, "cold");
    assert_eq!(info.created_at.as_deref(), Some("2024-01-15T10:30:00Z"));
}

#[tokio::test]
async fn usage_without_observation_is_not_reported() {
    let value: serde_json::Value = serde_json::from_str(USAGE_UNREPORTED).unwrap();
    let pure = parse_usage_reading(&value).unwrap();
    assert!(!pure.reported);
    assert_eq!(pure.usage.cpu_seconds, 0.0);

    let mock = start_mock(USAGE_UNREPORTED, false);
    let provider = SpritesProvider::new(&mock.base_url, "test-token", "demo");
    let http = provider
        .usage_reading_since("demo", "2024-01-15T10:00:00Z")
        .await
        .unwrap()
        .unwrap();
    assert!(!http.reported);
    assert_eq!(http.usage.cpu_seconds, 0.0);
}

#[tokio::test]
async fn usage_with_observation_is_reported() {
    let value: serde_json::Value = serde_json::from_str(USAGE_RECORDED).unwrap();
    let pure = parse_usage_reading(&value).unwrap();
    assert!(pure.reported);
    assert_eq!(pure.usage.cpu_seconds, 123.45);
    assert_eq!(
        pure.usage.reserved_gb_hours,
        gb_hours_from_byte_seconds(3865470566400.0)
    );
    assert_eq!(
        pure.last_recorded_at.as_deref(),
        Some("2024-01-15T10:59:00Z")
    );

    let mock = start_mock(USAGE_RECORDED, false);
    let provider = SpritesProvider::new(&mock.base_url, "test-token", "demo");
    let http = provider
        .usage_reading_since("demo", "2024-01-15T10:00:00Z")
        .await
        .unwrap()
        .unwrap();
    assert!(http.reported);
    assert_eq!(http.usage.cpu_seconds, 123.45);
    assert_eq!(
        http.usage.reserved_gb_hours,
        gb_hours_from_byte_seconds(3865470566400.0)
    );
    assert_eq!(
        http.last_recorded_at.as_deref(),
        Some("2024-01-15T10:59:00Z")
    );
}

#[tokio::test]
async fn list_with_token_but_no_has_more_is_refused() {
    let mock = start_mock(USAGE_UNREPORTED, true);
    let provider = SpritesProvider::new(&mock.base_url, "test-token", "my-dev-sprite");
    assert!(provider.sandbox_inventory().await.is_err());
}
