//! Replay mock for the DigitalOcean VPS adapter — a tiny no-dependency HTTP
//! server seeded with the documented `api.digitalocean.com/v2` response shapes.
//! Exercises `VpsProvider` (kind = DigitalOcean) request encoding (paths /
//! bodies / auth) and response parsing deterministically, plus the leak-safety
//! ledger flow. Mirrors `vps_mock.rs` (Hetzner), proving the shared driver +
//! `VpsShaper` seam works unchanged for a second vendor with flat-tag scoping.
//!
//! One #[test] only: the registry lives under `THEGN_DIR`, set process-wide.
//!
//! Run: `cargo test -p thegn-svc --test vps_do_mock`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use thegn_svc::provider::RemoteProvider;
use thegn_svc::vps::{VpsKind, VpsProvider, VpsSpec, registry};

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
    (format!("http://127.0.0.1:{port}/v2"), recorded)
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

    // Seeded responses: documented DigitalOcean shapes.
    let resp = match (method.as_str(), path.as_str()) {
        ("GET", "/v2/account/keys") => {
            r#"{"ssh_keys":[{"id":9,"public_key":"ssh-ed25519 OTHERKEY someone"}]}"#.to_string()
        }
        ("POST", "/v2/account/keys") => r#"{"ssh_key":{"id":42}}"#.to_string(),
        // Created: status "new", no public network yet (skip_ready_wait bypasses
        // the poll, so create() finalizes from this).
        ("POST", "/v2/droplets") => {
            r#"{"droplet":{"id":101,"name":"sz-do-1","status":"new","created_at":"2026-07-01T12:00:00Z"}}"#.to_string()
        }
        (m, p) if m == "GET" && p.starts_with("/v2/droplets?") => {
            r#"{"droplets":[{"id":101,"name":"sz-do-1","status":"active","created_at":"2026-07-01T12:00:00Z","networks":{"v4":[{"ip_address":"10.1.0.5","type":"private"},{"ip_address":"203.0.113.9","type":"public"}]},"tags":["sz-managed","tg-host:h1"]}]}"#.to_string()
        }
        ("DELETE", "/v2/droplets/101") => String::new(),
        _ => {
            let _ = writer.write_all(
                b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 2\r\n\r\n{}",
            );
            return;
        }
    };
    // `connection: close`: this mock serves one request per stream, but reqwest
    // pools HTTP/1.1 connections — a reused just-closed socket flakes as an RST.
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
        kind: VpsKind::DigitalOcean,
        api_base: api_base.to_string(),
        token: "mock-token".into(),
        name: "sz-do-1".into(),
        region: String::new(),
        size: String::new(),
        image: String::new(),
        max_instances: 0,
        max_lifetime_secs: 0,
        key_path: tmp.join("key"),
        pubkey: "ssh-ed25519 MOCKKEY thegn".into(),
        skip_ready_wait: true,
    }
}

#[test]
fn create_list_destroy_round_trip_with_ledger() {
    let tmp = tempfile::tempdir().unwrap();
    // Isolate the registry (one test per binary — no env-var race).
    unsafe { std::env::set_var("THEGN_DIR", tmp.path()) };
    let (base, recorded) = start_mock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let p = VpsProvider::new(spec(&base, tmp.path()));

    // --- create: registers our key, posts the droplet, finalizes the ledger.
    let handle = rt.block_on(p.create()).expect("create");
    assert_eq!(handle.id, "sz-do-1");
    let reqs = recorded.lock().unwrap().clone();
    let key_list = &reqs[0];
    assert_eq!(
        (key_list.method.as_str(), key_list.path.as_str()),
        ("GET", "/v2/account/keys")
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
        ("POST", "/v2/droplets")
    );
    let body: serde_json::Value = serde_json::from_str(&create.body).unwrap();
    assert_eq!(body["name"], "sz-do-1");
    assert_eq!(body["size"], "s-1vcpu-2gb");
    assert_eq!(body["image"], "ubuntu-24-04-x64");
    assert_eq!(body["region"], "nyc3");
    assert_eq!(body["ssh_keys"], serde_json::json!([42]));
    // Flat tags carry the managed marker + host scoping (the reaper's filter);
    // the host tag's hash is machine-derived, so assert its shape, not a value.
    let tags = body["tags"].as_array().expect("tags array");
    assert!(
        tags.iter().any(|t| t == "sz-managed"),
        "managed tag: {tags:?}"
    );
    assert!(
        tags.iter()
            .any(|t| t.as_str().is_some_and(|s| s.starts_with("tg-host:"))),
        "host-scoping tag: {tags:?}"
    );
    let ud = body["user_data"].as_str().unwrap();
    assert!(ud.starts_with("#cloud-config"), "cloud-init user data");
    assert!(ud.contains("get.docker.com"), "stock image installs docker");
    // Ledger finalized (skip_ready_wait ⇒ ip empty, state ready, id from create).
    let rec = registry::read("sz-do-1").expect("ledger record");
    assert_eq!(rec.state, "ready");
    assert_eq!(rec.provider, "digitalocean");
    assert_eq!(rec.instance_id, "101");

    // --- list: single-tag server-side filter.
    let names = rt.block_on(p.list()).expect("list");
    assert_eq!(names, vec!["sz-do-1"]);
    let last = recorded.lock().unwrap().last().unwrap().clone();
    assert!(
        last.path.contains("tag_name=sz-managed"),
        "list is tag-filtered: {}",
        last.path
    );

    // --- resolve_ip falls back to the API (ledger ip empty), reads the PUBLIC
    // v4 address, and persists it.
    let ip = rt.block_on(p.resolve_ip("sz-do-1")).expect("ip");
    assert_eq!(ip, "203.0.113.9");
    assert_eq!(registry::read("sz-do-1").unwrap().ip, "203.0.113.9");

    // --- destroy: DELETE by the vendor id from the ledger; ledger cleared.
    rt.block_on(p.destroy("sz-do-1")).expect("destroy");
    let last = recorded.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        (last.method.as_str(), last.path.as_str()),
        ("DELETE", "/v2/droplets/101")
    );
    assert!(
        registry::read("sz-do-1").is_none(),
        "ledger cleared on destroy"
    );

    // --- destroy of an unknown name is idempotent.
    rt.block_on(p.destroy("never-existed"))
        .expect("idempotent destroy");
}
