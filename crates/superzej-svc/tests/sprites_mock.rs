//! Replay mock for the Sprites provider — a tiny **no-dependency** HTTP server
//! seeded with the EXACT responses the live `api.sprites.dev` v1 API returned
//! during development. It exercises `SpritesProvider` for both **request
//! encoding** (method / path / query / body) and **response parsing** against
//! real bytes, deterministically (no token, no network, no cost). The fixtures
//! were captured live; this locks in the corrections found there (fs `PUT` +
//! query-param path, checkpoint `POST /checkpoint` singular + NDJSON stream).
//!
//! Run: `cargo test -p superzej-svc --test sprites_mock`.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use superzej_svc::provider::{
    Provider, ProviderCheckpoints, ProviderEgress, ProviderFiles, RemoteProvider, SpritesProvider,
};

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    /// Request target including the query string.
    path: String,
    body: Vec<u8>,
    auth: String,
}

struct Mock {
    base_url: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

fn start_mock() -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let rec = recorded.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let rec = rec.clone();
            thread::spawn(move || handle(stream, rec));
        }
    });
    Mock {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        recorded,
    }
}

fn handle(stream: TcpStream, rec: Arc<Mutex<Vec<Recorded>>>) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    // Request line.
    let mut line = String::new();
    if read_line(&mut reader, &mut line).is_none() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    // Headers.
    let mut content_length = 0usize;
    let mut auth = String::new();
    loop {
        let mut h = String::new();
        if read_line(&mut reader, &mut h).is_none() {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        let lower = t.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        if let Some(v) = lower.strip_prefix("authorization:") {
            auth = t[t.len() - v.trim().len()..].to_string();
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = reader.read_exact(&mut body);
    }
    rec.lock().unwrap().push(Recorded {
        method: method.clone(),
        path: path.clone(),
        body,
        auth,
    });
    let (status, ctype, resp) = route(&method, &path);
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.len()
    );
    let _ = writer.write_all(head.as_bytes());
    let _ = writer.write_all(&resp);
    let _ = writer.flush();
}

/// Read a single CRLF-terminated line from the reader (byte at a time — the
/// bodies are binary, so we can't wrap the whole stream in line-buffered text).
fn read_line(reader: &mut BufReader<TcpStream>, out: &mut String) -> Option<()> {
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

/// The recorded real responses, keyed by (method, path-without-query).
fn route(method: &str, target: &str) -> (&'static str, &'static str, Vec<u8>) {
    let path = target.split('?').next().unwrap_or(target);
    let json = "application/json";
    match (method, path) {
        ("POST", "/v1/sprites") => (
            "200 OK",
            json,
            br#"{"id":"sprite-abc","name":"sztest1","status":"cold","url":"https://x.sprites.app"}"#
                .to_vec(),
        ),
        ("GET", "/v1/sprites") => (
            "200 OK",
            json,
            br#"{"name":"blake-270","sprites":[{"id":"sprite-abc","name":"sztest1","status":"warm"}],"has_more":false}"#
                .to_vec(),
        ),
        ("DELETE", "/v1/sprites/sztest1") => ("204 No Content", "text/plain", vec![]),
        ("GET", "/v1/sprites/sztest1/policy/network") => (
            "200 OK",
            json,
            br#"{"rules":[{"domain":"github.com","action":"allow"},{"domain":"*","action":"deny"}]}"#
                .to_vec(),
        ),
        ("POST", "/v1/sprites/sztest1/policy/network") => ("204 No Content", "text/plain", vec![]),
        ("PUT", "/v1/sprites/sztest1/fs/write") => (
            "200 OK",
            json,
            br#"{"path":"/workspace/a.txt","size":5,"mode":"644"}"#.to_vec(),
        ),
        ("GET", "/v1/sprites/sztest1/fs/read") => {
            ("200 OK", "application/octet-stream", b"hello".to_vec())
        }
        ("GET", "/v1/sprites/sztest1/fs/list") => (
            "200 OK",
            json,
            br#"{"path":"/workspace","entries":[{"name":"a.txt","path":"/workspace/a.txt","type":"file","size":5,"mode":"644","isDir":false}],"count":1}"#
                .to_vec(),
        ),
        ("POST", "/v1/sprites/sztest1/checkpoint") => (
            "200 OK",
            "application/x-ndjson",
            b"{\"type\":\"info\",\"data\":\"Creating checkpoint...\"}\n{\"type\":\"info\",\"data\":\"  ID: v1\"}\n{\"type\":\"complete\",\"data\":\"Checkpoint v1 created successfully\"}\n".to_vec(),
        ),
        ("GET", "/v1/sprites/sztest1/checkpoints") => (
            "200 OK",
            json,
            br#"[{"id":"Current","create_time":"2026-06-27T05:08:24Z","is_auto":false},{"id":"v1","create_time":"2026-06-27T05:19:29Z","is_auto":false}]"#
                .to_vec(),
        ),
        ("POST", "/v1/sprites/sztest1/checkpoints/v1/restore") => (
            "200 OK",
            "application/x-ndjson",
            b"{\"type\":\"complete\",\"data\":\"Restored\"}\n".to_vec(),
        ),
        _ => (
            "404 Not Found",
            "text/plain",
            format!("no route {method} {path}").into_bytes(),
        ),
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

#[test]
fn full_provider_flow_against_recorded_api() {
    let mock = start_mock();
    let p = SpritesProvider::new(&mock.base_url, "tok", "sztest1");
    let rt = rt();

    // ---- response parsing (real fixtures) ----
    assert_eq!(rt.block_on(p.create()).unwrap().id, "sztest1");
    assert_eq!(rt.block_on(p.list()).unwrap(), vec!["sztest1"]);

    rt.block_on(p.set_network_policy("sztest1", &["github.com".into()], &["evil.com".into()]))
        .unwrap();
    let rules = rt.block_on(p.get_network_policy("sztest1")).unwrap();
    assert!(
        rules
            .iter()
            .any(|r| r.domain == "github.com" && r.action == "allow")
    );

    rt.block_on(p.write("sztest1", "/workspace/a.txt", b"hello"))
        .unwrap();
    assert_eq!(
        rt.block_on(p.read("sztest1", "/workspace/a.txt")).unwrap(),
        b"hello"
    );
    let entries = rt.block_on(p.list_dir("sztest1", "/workspace")).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.name == "a.txt" && !e.is_dir && e.size == 5)
    );

    assert_eq!(
        rt.block_on(p.checkpoint("sztest1", Some("t"))).unwrap(),
        "v1"
    );
    let cps = rt.block_on(p.list_checkpoints("sztest1")).unwrap();
    assert!(cps.iter().any(|c| c.id == "v1"));
    rt.block_on(p.restore("sztest1", "v1")).unwrap();
    rt.block_on(p.destroy("sztest1")).unwrap();

    // ---- request encoding (from recordings) ----
    let reqs = mock.recorded.lock().unwrap().clone();
    let find = |m: &str, sub: &str| {
        reqs.iter()
            .find(|r| r.method == m && r.path.contains(sub))
            .unwrap_or_else(|| panic!("no {m} request matching {sub} in {reqs:?}"))
    };

    // Every request carries the bearer token.
    assert!(
        reqs.iter().all(|r| r.auth == "Bearer tok"),
        "auth: {reqs:?}"
    );

    // Egress: deny-first, then allow, then default-deny — in the POST body.
    let pol = find("POST", "/policy/network");
    let body = String::from_utf8_lossy(&pol.body);
    let evil = body.find("evil.com").expect("block rule");
    let gh = body.find("github.com").expect("allow rule");
    let star = body.find(r#""domain":"*""#).expect("default-deny rule");
    assert!(evil < gh && gh < star, "rule order wrong: {body}");

    // fs write is PUT with the path + mkdirParents as query params, body = bytes.
    let w = find("PUT", "/fs/write");
    assert!(
        w.path.contains("mkdirParents=true"),
        "write query: {}",
        w.path
    );
    assert!(w.path.contains("workspace"), "write path query: {}", w.path);
    assert_eq!(w.body, b"hello");

    // fs read/list are GET with a query path (not path-in-URL).
    assert!(find("GET", "/fs/read").path.contains("path="));
    assert!(find("GET", "/fs/list").path.contains("path="));

    // Checkpoint create hits the SINGULAR endpoint (not /checkpoints).
    let cp = find("POST", "/v1/sprites/sztest1/checkpoint");
    assert!(
        !cp.path
            .trim_end_matches(|c| c != '/')
            .ends_with("checkpoints/"),
        "create must be singular /checkpoint, got {}",
        cp.path
    );
    assert_eq!(
        cp.path.split('?').next().unwrap(),
        "/v1/sprites/sztest1/checkpoint"
    );
}

#[test]
fn provider_enum_dispatch_against_mock() {
    let mock = start_mock();
    let prov = Provider::Sprites(SpritesProvider::new(&mock.base_url, "tok", "sztest1"));
    let rt = rt();
    // The generic enum dispatch reaches the same Sprites impls.
    assert_eq!(rt.block_on(prov.create()).unwrap().id, "sztest1");
    assert_eq!(rt.block_on(prov.list()).unwrap(), vec!["sztest1"]);
    rt.block_on(prov.set_network_policy("sztest1", &[], &["x.com".into()]))
        .unwrap();
    assert_eq!(rt.block_on(prov.checkpoint("sztest1", None)).unwrap(), "v1");
    rt.block_on(prov.upload_dir(
        "sztest1",
        std::path::Path::new("/nonexistent-dir-xyz"),
        "/workspace",
    ))
    .unwrap_err(); // local dir missing → error, but the dispatch path is exercised
}
