//! Replay mock for the Hetzner VPS provider — a tiny no-dependency HTTP server
//! seeded with the documented `api.hetzner.cloud/v1` response shapes. Exercises
//! `VpsProvider` request encoding (paths / bodies / auth) and response parsing
//! deterministically, plus the leak-safety ledger flow (intent → ready →
//! removed-on-destroy). Mirrors `sprites_mock.rs`.
//!
//! One #[test] only: the registry lives under `SUPERZEJ_DIR`, set process-wide
//! here — parallel tests in this binary would race the env var.
//!
//! Run: `cargo test -p superzej-svc --test vps_mock`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use superzej_svc::provider::RemoteProvider;
use superzej_svc::vps::{VpsKind, VpsProvider, VpsSpec, registry};

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    body: String,
    auth: String,
}

fn start_mock() -> (String, Arc<Mutex<Vec<Recorded>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
    let rec = recorded.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let rec = rec.clone();
            thread::spawn(move || handle(stream, rec));
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), recorded)
}

fn handle(stream: TcpStream, rec: Arc<Mutex<Vec<Recorded>>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
    let mut auth = String::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        let lower = h.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        if lower.starts_with("authorization:") {
            auth = h
                .split_once(':')
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default();
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    let body = String::from_utf8_lossy(&body).into_owned();
    rec.lock().unwrap().push(Recorded {
        method: method.clone(),
        path: path.clone(),
        body,
        auth,
    });

    // Seeded responses: documented Hetzner shapes.
    let resp = match (method.as_str(), path.as_str()) {
        ("GET", "/v1/ssh_keys") => {
            r#"{"ssh_keys":[{"id":9,"public_key":"ssh-ed25519 OTHERKEY someone"}]}"#.to_string()
        }
        ("POST", "/v1/ssh_keys") => r#"{"ssh_key":{"id":42}}"#.to_string(),
        ("POST", "/v1/servers") => {
            // Created: booting, no ip yet — but the test sets skip_ready_wait
            // so this is what create() returns from.
            r#"{"server":{"id":101,"name":"sz-mock-1","status":"initializing","created":"2026-07-01T12:00:00+00:00"}}"#.to_string()
        }
        (m, p) if m == "GET" && p.starts_with("/v1/servers?") => {
            r#"{"servers":[{"id":101,"name":"sz-mock-1","status":"running","created":"2026-07-01T12:00:00+00:00","public_net":{"ipv4":{"ip":"203.0.113.9"}},"labels":{"managed-by":"superzej","sz-host":"h1"}}]}"#.to_string()
        }
        ("DELETE", "/v1/servers/101") => r#"{}"#.to_string(),
        _ => {
            let _ = writer.write_all(
                b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 2\r\n\r\n{}",
            );
            return;
        }
    };
    // `connection: close` matters: this mock serves ONE request per stream,
    // but reqwest pools HTTP/1.1 connections by default — a reused
    // just-closed socket surfaces as a flaky "error sending request" RST.
    let _ = writer.write_all(
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{resp}",
            resp.len()
        )
        .as_bytes(),
    );
}

fn spec(api_base: &str, tmp: &std::path::Path) -> VpsSpec {
    VpsSpec {
        kind: VpsKind::Hetzner,
        api_base: api_base.to_string(),
        token: "mock-token".into(),
        name: "sz-mock-1".into(),
        region: String::new(),
        size: String::new(),
        image: String::new(),
        max_instances: 0,
        max_lifetime_secs: 0,
        key_path: tmp.join("key"),
        pubkey: "ssh-ed25519 MOCKKEY superzej".into(),
        skip_ready_wait: true,
    }
}

#[test]
fn create_list_destroy_round_trip_with_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    // Isolate the registry (one test per binary — no env-var race).
    unsafe { std::env::set_var("SUPERZEJ_DIR", tmp.path()) };
    let (base, recorded) = start_mock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let p = VpsProvider::new(spec(&base, tmp.path()));

    // --- create: registers our key (the listed one differs), posts the server,
    // and finalizes the ledger record.
    let handle = rt.block_on(p.create()).expect("create");
    assert_eq!(handle.id, "sz-mock-1");
    let reqs = recorded.lock().unwrap().clone();
    let key_list = &reqs[0];
    assert_eq!(
        (key_list.method.as_str(), key_list.path.as_str()),
        ("GET", "/v1/ssh_keys")
    );
    assert_eq!(key_list.auth, "Bearer mock-token");
    let key_create = &reqs[1];
    assert_eq!(key_create.method, "POST");
    assert!(
        key_create.body.contains("MOCKKEY"),
        "registers OUR key: {}",
        key_create.body
    );
    let create = &reqs[2];
    assert_eq!(
        (create.method.as_str(), create.path.as_str()),
        ("POST", "/v1/servers")
    );
    let body: serde_json::Value = serde_json::from_str(&create.body).unwrap();
    assert_eq!(body["name"], "sz-mock-1");
    assert_eq!(body["server_type"], "cx22");
    assert_eq!(body["image"], "ubuntu-24.04");
    assert_eq!(body["ssh_keys"], serde_json::json!([42]));
    assert_eq!(body["labels"]["managed-by"], "superzej");
    let ud = body["user_data"].as_str().unwrap();
    assert!(ud.starts_with("#cloud-config"), "cloud-init user data");
    assert!(ud.contains("get.docker.com"), "stock image installs docker");
    // Ledger finalized (skip_ready_wait ⇒ ip empty, but state is ready).
    let rec = registry::read("sz-mock-1").expect("ledger record");
    assert_eq!(rec.state, "ready");
    assert_eq!(rec.instance_id, "101");

    // --- list: label-filtered server-side.
    let names = rt.block_on(p.list()).expect("list");
    assert_eq!(names, vec!["sz-mock-1"]);
    let last = recorded.lock().unwrap().last().unwrap().clone();
    assert!(
        last.path.contains("label_selector=managed-by%3Dsuperzej"),
        "list is label-filtered: {}",
        last.path
    );

    // --- resolve_ip falls back to the API when the ledger has no ip yet, and
    // persists what it finds.
    let ip = rt.block_on(p.resolve_ip("sz-mock-1")).expect("ip");
    assert_eq!(ip, "203.0.113.9");
    assert_eq!(registry::read("sz-mock-1").unwrap().ip, "203.0.113.9");

    // --- destroy: DELETE by the vendor id from the ledger; ledger cleared.
    rt.block_on(p.destroy("sz-mock-1")).expect("destroy");
    let last = recorded.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        (last.method.as_str(), last.path.as_str()),
        ("DELETE", "/v1/servers/101")
    );
    assert!(
        registry::read("sz-mock-1").is_none(),
        "ledger cleared on destroy"
    );

    // --- destroy of an unknown name is idempotent (no instance, no error).
    rt.block_on(p.destroy("never-existed"))
        .expect("idempotent destroy");
}
