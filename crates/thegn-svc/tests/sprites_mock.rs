//! Replay mock for the Sprites provider — a tiny **no-dependency** HTTP server
//! seeded with the EXACT responses the live `api.sprites.dev` v1 API returned
//! during development. It exercises `SpritesProvider` for both **request
//! encoding** (method / path / query / body) and **response parsing** against
//! real bytes, deterministically (no token, no network, no cost). The fixtures
//! were captured live; this locks in the corrections found there (fs `PUT` +
//! query-param path, checkpoint `POST /checkpoint` singular + NDJSON stream).
//!
//! Run: `cargo test -p thegn-svc --test sprites_mock`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use thegn_svc::provider::{
    Provider, ProviderCheckpoints, ProviderEgress, ProviderFiles, RemoteProvider, SpritesProvider,
};
use thegn_svc::sprites::ledger::OwnerScope;

/// A fixed ownership scope for the mock (never the host's real controller id).
fn scope() -> OwnerScope {
    OwnerScope {
        owner: "ctl-sprites-mock".into(),
        account: "acct1-sprites-mock".into(),
    }
}

/// A provider that may create and destroy: scoped, with a private ledger
/// directory (the returned guard keeps it alive) so no test touches the real
/// state directory.
fn scoped(base: &str, name: &str) -> (SpritesProvider, tempfile::TempDir) {
    let ledger = tempfile::tempdir().unwrap();
    let provider = SpritesProvider::new(base, "tok", name)
        .with_scope(scope())
        .with_ledger_dir(ledger.path().to_path_buf());
    (provider, ledger)
}

/// A tiny stateful fs the mock serves so `/fs/write` → `/fs/read` round-trips
/// (needed for SHA-256 verification in `ensure_executable`). Paths are decoded
/// from the query string so exec commands and fs requests address the same file.
type FsState = Arc<Mutex<HashMap<String, Vec<u8>>>>;

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
    start_mock_with_restore_error(false)
}

fn start_mock_with_restore_error(restore_error: bool) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let fs: FsState = Arc::new(Mutex::new(HashMap::new()));
    let rec = recorded.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let rec = rec.clone();
            let fs = fs.clone();
            thread::spawn(move || handle(stream, rec, fs, restore_error));
        }
    });
    Mock {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        recorded,
    }
}

/// The decoded value of the `path=` query param, if present.
fn query_path(target: &str) -> Option<String> {
    let q = target.split_once('?')?.1;
    url::form_urlencoded::parse(q.as_bytes())
        .find_map(|(key, value)| (key == "path").then(|| value.into_owned()))
}

fn handle(stream: TcpStream, rec: Arc<Mutex<Vec<Recorded>>>, fs: FsState, restore_error: bool) {
    // Request line.
    let mut line = String::new();
    let mut stream = stream;
    if read_line(&mut stream, &mut line).is_none() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    // Headers.
    let mut content_length = 0usize;
    let mut auth = String::new();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if read_line(&mut stream, &mut h).is_none() {
            break;
        }
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        let lower = t.to_ascii_lowercase();
        if let Some((name, value)) = t.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
        if let Some(v) = lower.strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
        if let Some(v) = lower.strip_prefix("authorization:") {
            auth = t[t.len() - v.trim().len()..].to_string();
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && stream.read_exact(&mut body).is_err() {
        return;
    }
    rec.lock().unwrap().push(Recorded {
        method: method.clone(),
        path: path.clone(),
        body: body.clone(),
        auth,
    });
    if method == "GET" && path.starts_with("/v1/sprites/tgtest1/exec?") {
        handle_exec_websocket(stream, &headers, &path, &fs);
        return;
    }
    // Stateful fs: writes store by the `path=` query, reads serve them back (404
    // when absent). Everything else falls to the recorded-response router.
    let (status, ctype, resp) = if method == "PUT" && path.contains("/fs/write") {
        if let Some(k) = query_path(&path) {
            fs.lock().unwrap().insert(k, body);
        }
        (
            "200 OK",
            "application/json",
            br#"{"path":"x","size":0,"mode":"644"}"#.to_vec(),
        )
    } else if method == "GET" && path.contains("/fs/read") {
        match query_path(&path).and_then(|k| fs.lock().unwrap().get(&k).cloned()) {
            Some(b) => ("200 OK", "application/octet-stream", b),
            None => ("404 Not Found", "text/plain", b"missing".to_vec()),
        }
    } else if method == "POST" && path.split('?').next() == Some("/v1/sprites") {
        // Echo the requested name, as the real API does: create refuses any other.
        let name = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("name")
                    .and_then(|name| name.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "tgtest1".to_owned());
        (
            "200 OK",
            "application/json",
            format!(
                r#"{{"id":"sprite-abc","name":"{name}","status":"cold","url":"https://x.sprites.app"}}"#
            )
            .into_bytes(),
        )
    } else {
        route(&method, &path, restore_error)
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.len()
    );
    let _ = stream.write_all(head.as_bytes()); // best-effort: client may have disconnected
    let _ = stream.write_all(&resp); // best-effort: client may have disconnected
    let _ = stream.flush(); // best-effort: client may have disconnected
}

/// Read a single CRLF-terminated line from the reader (byte at a time — the
/// bodies are binary, so we can't wrap the whole stream in line-buffered text).
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

/// Complete the Sprites exec WebSocket handshake and emulate the install shell
/// steps and `mv -f --` used to publish a staged executable. The stdout frame,
/// the single-byte exit frame `[0x03, code]`, then a close.
fn handle_exec_websocket(
    mut stream: TcpStream,
    headers: &HashMap<String, String>,
    target: &str,
    fs: &FsState,
) {
    let Some(key) = headers.get("sec-websocket-key") else {
        return;
    };
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }

    use tokio_tungstenite::tungstenite::protocol::{Role, WebSocket};
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    let args: Vec<String> = target
        .split_once('?')
        .map(|(_, query)| {
            url::form_urlencoded::parse(query.as_bytes())
                .filter_map(|(key, value)| (key == "cmd").then(|| value.into_owned()))
                .collect()
        })
        .unwrap_or_default();
    let (code, output) = if args.len() >= 4 && args[0] == "/bin/sh" && args[1] == "-c" {
        match args[3].as_str() {
            "thegn-install-lease" | "thegn-install-release" => (0, String::new()),
            "thegn-install-preserve" if args.len() >= 6 => {
                let previous = args[5].clone();
                let bytes = {
                    let files = fs.lock().unwrap();
                    files.get(&args[4]).cloned()
                };
                let mut files = fs.lock().unwrap();
                files.remove(&previous);
                if let Some(bytes) = bytes {
                    files.insert(previous, bytes);
                }
                (0, String::new())
            }
            "thegn-install-publish" if args.len() >= 6 => {
                let stage = &args[4];
                let destination = &args[5];
                let mut files = fs.lock().unwrap();
                match files.remove(stage) {
                    Some(bytes) => {
                        files.insert(destination.clone(), bytes);
                        (0, String::new())
                    }
                    None => (1, "missing staging file".to_string()),
                }
            }
            "thegn-install-restore" if args.len() >= 6 => {
                let mut files = fs.lock().unwrap();
                match files.remove(&args[5]) {
                    Some(bytes) => {
                        files.insert(args[4].clone(), bytes);
                    }
                    None => {
                        files.remove(&args[4]);
                    }
                }
                (0, String::new())
            }
            "thegn-install-cleanup" if args.len() >= 5 => {
                fs.lock().unwrap().remove(&args[4]);
                (0, String::new())
            }
            _ => (1, format!("unexpected install command: {args:?}")),
        }
    } else if args.len() == 5 && args[0] == "mv" && args[1] == "-f" && args[2] == "--" {
        let stage = &args[3];
        let destination = &args[4];
        let mut files = fs.lock().unwrap();
        match files.remove(stage) {
            Some(bytes) => {
                files.insert(destination.clone(), bytes);
                (0, String::new())
            }
            None => (1, "missing staging file".to_string()),
        }
    } else {
        (1, format!("unexpected exec command: {args:?}"))
    };
    if !output.is_empty() {
        let mut frame = vec![1];
        frame.extend_from_slice(output.as_bytes());
        if socket
            .send(tokio_tungstenite::tungstenite::Message::Binary(
                frame.into(),
            ))
            .is_err()
        {
            return;
        }
    }
    let exit = vec![0x03, code as u8];
    if socket
        .send(tokio_tungstenite::tungstenite::Message::Binary(exit.into()))
        .is_err()
    {
        return;
    }
    if socket.close(None).is_err() {
        return;
    }
    socket.flush().unwrap_or_default(); // best-effort: test mock: the client may already have hung up
}

/// The recorded real responses, keyed by (method, path-without-query).
fn route(method: &str, target: &str, restore_error: bool) -> (&'static str, &'static str, Vec<u8>) {
    let path = target.split('?').next().unwrap_or(target);
    let json = "application/json";
    match (method, path) {
        ("POST", "/v1/sprites") => (
            "200 OK",
            json,
            br#"{"id":"sprite-abc","name":"tgtest1","status":"cold","url":"https://x.sprites.app"}"#
                .to_vec(),
        ),
        ("GET", "/v1/sprites") => (
            "200 OK",
            json,
            br#"{"name":"blake-270","sprites":[{"id":"sprite-abc","name":"tgtest1","status":"warm"}],"has_more":false}"#
                .to_vec(),
        ),
        ("DELETE", "/v1/sprites/tgtest1") => ("204 No Content", "text/plain", vec![]),
        ("GET", "/v1/sprites/tgtest1/policy/network") => (
            "200 OK",
            json,
            br#"{"rules":[{"domain":"github.com","action":"allow"},{"domain":"*","action":"deny"}]}"#
                .to_vec(),
        ),
        ("POST", "/v1/sprites/tgtest1/policy/network") => ("204 No Content", "text/plain", vec![]),
        ("PUT", "/v1/sprites/tgtest1/fs/write") => (
            "200 OK",
            json,
            br#"{"path":"/workspace/a.txt","size":5,"mode":"644"}"#.to_vec(),
        ),
        ("GET", "/v1/sprites/tgtest1/fs/read") => {
            ("200 OK", "application/octet-stream", b"hello".to_vec())
        }
        ("GET", "/v1/sprites/tgtest1/fs/list") => (
            "200 OK",
            json,
            br#"{"path":"/workspace","entries":[{"name":"a.txt","path":"/workspace/a.txt","type":"file","size":5,"mode":"644","isDir":false}],"count":1}"#
                .to_vec(),
        ),
        ("DELETE", "/v1/sprites/tgtest1/fs/delete") => (
            // UNVERIFIED which form the server honours; the handler records and
            // accepts both the legacy query and documented JSON body.
            "200 OK",
            json,
            br#"{"deleted":[],"count":0}"#.to_vec(),
        ),
        ("POST", "/v1/sprites/tgtest1/checkpoint") => (
            "200 OK",
            "application/x-ndjson",
            b"{\"type\":\"info\",\"data\":\"Creating checkpoint...\"}\n{\"type\":\"info\",\"data\":\"  ID: v1\"}\n{\"type\":\"complete\",\"data\":\"Checkpoint v1 created successfully\"}\n".to_vec(),
        ),
        ("GET", "/v1/sprites/tgtest1/checkpoints") => (
            "200 OK",
            json,
            br#"[{"id":"Current","create_time":"2026-06-27T05:08:24Z","is_auto":false},{"id":"v1","create_time":"2026-06-27T05:19:29Z","is_auto":false}]"#
                .to_vec(),
        ),
        ("GET", "/v1/sprites/tgtest1/checkpoints/bad") => (
            "200 OK",
            json,
            br#"{"id":"bad","create_time":"t","health":"mount_failed"}"#.to_vec(),
        ),
        ("GET", "/v1/sprites/tgtest1/checkpoints/v1") => (
            "200 OK",
            json,
            br#"{"id":"v1","create_time":"t"}"#.to_vec(),
        ),
        ("POST", "/v1/sprites/tgtest1/checkpoints/v1/restore") if restore_error => (
            "200 OK",
            "application/x-ndjson",
            br#"{"type":"error","error":"boom","time":"t"}
"#.to_vec(),
        ),
        ("POST", "/v1/sprites/tgtest1/checkpoints/v1/restore") => (
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

/// A minimal mock that answers `DELETE` with `500` for the first `fail_first`
/// calls, then `204` — purpose-built for the transient-teardown retry (the
/// observed "sprites destroy failed (500)" that leaked a paid sandbox). Returns
/// the base URL and a shared counter of DELETEs seen, so tests can assert the
/// retry budget is exactly what the code intends (bounded, not infinite).
fn start_delete_mock(fail_first: usize) -> (String, Arc<Mutex<usize>>) {
    start_delete_mock_get(fail_first, "404 Not Found")
}

/// Like `start_delete_mock`, but answers the read-only existence probe with
/// `get_status` until an accepted DELETE, then answers 404 like the real API.
fn start_delete_mock_get(
    fail_first: usize,
    get_status: &'static str,
) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let deletes = Arc::new(Mutex::new(0usize));
    let accepted = Arc::new(Mutex::new(false));
    let d = deletes.clone();
    let a = accepted.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let d = d.clone();
            let a = a.clone();
            thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = stream;
                let mut line = String::new();
                if read_line(&mut reader, &mut line).is_none() {
                    return;
                }
                let method = line.split_whitespace().next().unwrap_or("").to_string();
                // Drain the headers so the client's request completes cleanly.
                loop {
                    let mut h = String::new();
                    if read_line(&mut reader, &mut h).is_none() {
                        break;
                    }
                    if h.trim_end().is_empty() {
                        break;
                    }
                }
                let (status, body): (&str, &[u8]) = if method == "DELETE" {
                    let mut n = d.lock().unwrap();
                    *n += 1;
                    if *n <= fail_first {
                        ("500 Internal Server Error", b"boom")
                    } else {
                        *a.lock().unwrap() = true;
                        ("204 No Content", b"")
                    }
                } else if *a.lock().unwrap() {
                    ("404 Not Found", b"no route")
                } else if get_status.starts_with("200") {
                    (get_status, b"{}")
                } else {
                    (get_status, b"no route")
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = writer.write_all(head.as_bytes()); // best-effort: client may have disconnected
                let _ = writer.write_all(body); // best-effort: client may have disconnected
                let _ = writer.flush(); // best-effort: client may have disconnected
            });
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), deletes)
}

/// A mock for destroy-without-proof: `GET /v1/sprites/<name>` answers
/// `get_status`, every `DELETE` answers `204`. GETs and DELETEs are counted
/// separately, so a test can assert the probe ran and the delete never did.
fn start_probe_delete_mock(
    get_status: &'static str,
) -> (String, Arc<Mutex<usize>>, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let gets = Arc::new(Mutex::new(0usize));
    let deletes = Arc::new(Mutex::new(0usize));
    let (g, d) = (gets.clone(), deletes.clone());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (g, d) = (g.clone(), d.clone());
            thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = stream;
                let mut line = String::new();
                if read_line(&mut reader, &mut line).is_none() {
                    return;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                loop {
                    let mut h = String::new();
                    if read_line(&mut reader, &mut h).is_none() {
                        break;
                    }
                    if h.trim_end().is_empty() {
                        break;
                    }
                }
                let (status, body): (&str, &[u8]) = match method.as_str() {
                    "GET" if path.starts_with("/v1/sprites/") => {
                        *g.lock().unwrap() += 1;
                        (get_status, b"{}")
                    }
                    "DELETE" => {
                        *d.lock().unwrap() += 1;
                        ("204 No Content", b"")
                    }
                    _ => ("404 Not Found", b"no route"),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = writer.write_all(head.as_bytes()); // best-effort: client may have disconnected
                let _ = writer.write_all(body); // best-effort: client may have disconnected
                let _ = writer.flush(); // best-effort: client may have disconnected
            });
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), gets, deletes)
}

/// A mock that answers `POST /sprites` (create) with `500` for the first
/// `fail_first` calls, then a valid create body — the create analogue of
/// [`start_delete_mock`], for the transient-create retry (a cold-booting control
/// plane can 5xx before it settles; an unretried create strands provisioning).
/// Returns the base URL and a shared counter of create POSTs seen.
fn start_create_mock(fail_first: usize) -> (String, Arc<Mutex<usize>>) {
    start_create_mock_with_echo(fail_first, "newone")
}

fn start_create_mock_with_echo(
    fail_first: usize,
    echoed_name: &'static str,
) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let posts = Arc::new(Mutex::new(0usize));
    let p = posts.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let p = p.clone();
            thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = stream;
                let mut line = String::new();
                if read_line(&mut reader, &mut line).is_none() {
                    return;
                }
                let method = line.split_whitespace().next().unwrap_or("").to_string();
                let mut content_length = 0usize;
                loop {
                    let mut h = String::new();
                    if read_line(&mut reader, &mut h).is_none() {
                        break;
                    }
                    let t = h.trim_end();
                    if t.is_empty() {
                        break;
                    }
                    if let Some(v) = t.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    let _ = reader.read_exact(&mut body); // best-effort: client may have disconnected
                }
                let (status, resp): (&str, &[u8]) = if method == "POST" {
                    let mut n = p.lock().unwrap();
                    *n += 1;
                    if *n <= fail_first {
                        ("500 Internal Server Error", b"boom")
                    } else {
                        let response: &'static [u8] = match echoed_name {
                            "newone" => br#"{"id":"x","name":"newone","status":"cold"}"#,
                            "other" => br#"{"id":"x","name":"other","status":"cold"}"#,
                            _ => br#"{"id":"x","name":"unexpected","status":"cold"}"#,
                        };
                        ("200 OK", response)
                    }
                } else {
                    ("404 Not Found", b"no route")
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.len()
                );
                let _ = writer.write_all(head.as_bytes()); // best-effort: client may have disconnected
                let _ = writer.write_all(resp); // best-effort: client may have disconnected
                let _ = writer.flush(); // best-effort: client may have disconnected
            });
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), posts)
}

/// A mock that answers `GET /fs/list` with `404` for the first `fail_first`
/// calls, then a valid listing — for the post-create readiness probe
/// ([`Provider::wait_ready`]): a cold-booting sprite's guest fs API is absent for
/// a beat, and `wait_ready` must poll past that rather than fail on the first miss.
fn start_flaky_list_mock(fail_first: usize) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let lists = Arc::new(Mutex::new(0usize));
    let l = lists.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let l = l.clone();
            thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = stream;
                let mut line = String::new();
                if read_line(&mut reader, &mut line).is_none() {
                    return;
                }
                let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
                loop {
                    let mut h = String::new();
                    if read_line(&mut reader, &mut h).is_none() {
                        break;
                    }
                    if h.trim_end().is_empty() {
                        break;
                    }
                }
                let (status, ctype, resp): (&str, &str, Vec<u8>) = if target.contains("/fs/list") {
                    let mut n = l.lock().unwrap();
                    *n += 1;
                    if *n <= fail_first {
                        ("404 Not Found", "text/plain", b"booting".to_vec())
                    } else {
                        (
                            "200 OK",
                            "application/json",
                            br#"{"path":"/","entries":[],"count":0}"#.to_vec(),
                        )
                    }
                } else {
                    ("404 Not Found", "text/plain", b"no route".to_vec())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.len()
                );
                let _ = writer.write_all(head.as_bytes()); // best-effort: client may have disconnected
                let _ = writer.write_all(&resp); // best-effort: client may have disconnected
                let _ = writer.flush(); // best-effort: client may have disconnected
            });
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), lists)
}

#[test]
fn create_retries_transient_500_then_succeeds() {
    // 500 twice, then 200: create must ride out the transient errors (a cold
    // control plane) and succeed — hitting POST /sprites exactly 3×.
    let (base, posts) = start_create_mock(2);
    let (p, _ledger) = scoped(&base, "newone");
    let h = rt().block_on(p.create()).unwrap();
    assert_eq!(h.id, "newone");
    assert_eq!(
        *posts.lock().unwrap(),
        3,
        "two failures then a success = three create attempts"
    );
}

#[test]
fn create_gives_up_bounded_on_persistent_500() {
    // Always 500: create errors after a BOUNDED attempt budget (never hangs the
    // provisioning thread) and surfaces the status.
    let (base, posts) = start_create_mock(usize::MAX);
    let (p, _ledger) = scoped(&base, "newone");
    let err = rt().block_on(p.create()).unwrap_err();
    assert!(err.to_string().contains("500"), "surfaced status: {err}");
    assert_eq!(
        *posts.lock().unwrap(),
        3,
        "exactly the bounded number of attempts, then give up"
    );
}

#[test]
fn create_without_a_scope_is_refused_before_any_request() {
    // No controller id ⇒ no ledger record ⇒ no create: refused with zero HTTP
    // requests, so an unrecorded (unreapable, undeletable) sprite never exists.
    let (base, posts) = start_create_mock(0);
    let unscoped = SpritesProvider::new(&base, "tok", "newone");
    let err = rt().block_on(unscoped.create()).unwrap_err();
    assert!(
        err.to_string()
            .contains("no controller id; refusing to create an unrecorded sprite"),
        "{err}"
    );
    assert_eq!(*posts.lock().unwrap(), 0, "refused before any request");
}

#[test]
fn create_with_a_scope_sends_exactly_one_post() {
    let (base, posts) = start_create_mock(0);
    let (p, _ledger) = scoped(&base, "newone");
    let handle = rt().block_on(p.create()).unwrap();
    assert_eq!(handle.id, "newone");
    assert_eq!(*posts.lock().unwrap(), 1, "one POST /sprites");
}

#[test]
fn destroy_without_a_scope_is_refused_before_any_request() {
    // No scope ⇒ no ownership proof ⇒ no DELETE.
    let (base, deletes) = start_delete_mock(0);
    let unscoped = SpritesProvider::new(&base, "tok", "tgtest1");
    let err = rt().block_on(unscoped.destroy("tgtest1")).unwrap_err();
    assert!(
        err.to_string().contains("refusing to delete tgtest1"),
        "{err}"
    );
    assert_eq!(*deletes.lock().unwrap(), 0, "refused before any request");
}

#[test]
fn wait_ready_returns_ok_when_fs_answers() {
    // The recorded mock answers GET /fs/list with a listing ⇒ the guest fs API is
    // up ⇒ readiness resolves on the first probe.
    let mock = start_mock();
    let prov = Provider::Sprites(SpritesProvider::new(&mock.base_url, "tok", "tgtest1"));
    rt().block_on(prov.wait_ready("tgtest1", Duration::from_secs(5)))
        .expect("fs answering ⇒ ready");
}

#[test]
fn wait_ready_errors_on_deadline() {
    // An unknown sprite name never resolves a listing (404) ⇒ with a zero budget,
    // readiness does exactly one probe then fails with a clear deadline message —
    // bounded, never an infinite wait.
    let mock = start_mock();
    let prov = Provider::Sprites(SpritesProvider::new(&mock.base_url, "tok", "ghost"));
    let err = rt()
        .block_on(prov.wait_ready("ghost", Duration::ZERO))
        .unwrap_err();
    assert!(
        err.to_string().contains("did not become ready"),
        "clear deadline error: {err}"
    );
}

#[test]
fn wait_ready_polls_past_a_transient_failure() {
    // 404 once (still booting) then a listing: readiness must poll past the miss
    // and succeed, hitting GET /fs/list at least twice.
    let (base, lists) = start_flaky_list_mock(1);
    let prov = Provider::Sprites(SpritesProvider::new(&base, "tok", "tgtest1"));
    rt().block_on(prov.wait_ready("tgtest1", Duration::from_secs(10)))
        .expect("polls past the transient miss");
    assert!(
        *lists.lock().unwrap() >= 2,
        "probed again after the first failure"
    );
}

#[test]
fn destroy_of_an_unrecorded_sprite_that_is_gone_is_ok_without_a_delete() {
    // No ownership proof, but the sprite is already gone (404): teardown stays
    // idempotent and returns Ok, after one GET and zero DELETEs.
    let (base, gets, deletes) = start_probe_delete_mock("404 Not Found");
    let (p, _ledger) = scoped(&base, "unrecorded");
    rt().block_on(p.destroy("unrecorded"))
        .expect("an already-gone unrecorded sprite is not an error");
    assert_eq!(*gets.lock().unwrap(), 1, "one non-mutating probe");
    assert_eq!(*deletes.lock().unwrap(), 0, "never DELETE without proof");
}

#[test]
fn destroy_of_an_unrecorded_sprite_that_exists_is_refused_without_a_delete() {
    // No ownership proof and the sprite exists (200): refused, zero DELETEs.
    let (base, gets, deletes) = start_probe_delete_mock("200 OK");
    let (p, _ledger) = scoped(&base, "unrecorded");
    let err = rt().block_on(p.destroy("unrecorded")).unwrap_err();
    assert!(
        format!("{err:#}").contains("refusing to delete unrecorded"),
        "{err:#}"
    );
    assert_eq!(*gets.lock().unwrap(), 1, "one non-mutating probe");
    assert_eq!(*deletes.lock().unwrap(), 0, "never DELETE without proof");
}

#[test]
fn destroy_retries_transient_500_then_succeeds() {
    // 500 twice, then 204: the teardown retry must ride out the transient errors
    // and succeed rather than leaking the sandbox — hitting DELETE exactly 3×.
    let (base, deletes) = start_delete_mock(2);
    let (p, _ledger) = recorded(&base, "tgtest1");
    rt().block_on(p.destroy("tgtest1")).unwrap();
    assert_eq!(
        *deletes.lock().unwrap(),
        3,
        "two failures then a success = three DELETE attempts"
    );
}

#[test]
fn destroy_gives_up_bounded_on_persistent_500() {
    // Always 500: teardown errors after a BOUNDED attempt budget (never loops
    // forever, which would wedge the off-loop delete thread) and surfaces the
    // status so the warn is actionable.
    let (base, deletes) = start_delete_mock(usize::MAX);
    let (p, _ledger) = recorded(&base, "tgtest1");
    let err = rt().block_on(p.destroy("tgtest1")).unwrap_err();
    assert!(
        err.to_string().contains("500"),
        "the surfaced error names the status: {err}"
    );
    assert_eq!(
        *deletes.lock().unwrap(),
        3,
        "exactly the bounded number of attempts, then give up"
    );
}

#[test]
fn full_provider_flow_against_recorded_api() {
    let mock = start_mock();
    let (p, _ledger) = scoped(&mock.base_url, "tgtest1");
    let rt = rt();

    // ---- response parsing (real fixtures) ----
    assert_eq!(rt.block_on(p.create()).unwrap().id, "tgtest1");
    assert_eq!(rt.block_on(p.list()).unwrap(), vec!["tgtest1"]);

    rt.block_on(p.set_network_policy("tgtest1", &["github.com".into()], &["evil.com".into()]))
        .unwrap();
    let rules = rt.block_on(p.get_network_policy("tgtest1")).unwrap();
    assert!(
        rules
            .iter()
            .any(|r| r.domain == "github.com" && r.action == "allow")
    );

    rt.block_on(p.write("tgtest1", "/workspace/a.txt", b"hello"))
        .unwrap();
    assert_eq!(
        rt.block_on(p.read("tgtest1", "/workspace/a.txt")).unwrap(),
        b"hello"
    );
    let entries = rt.block_on(p.list_dir("tgtest1", "/workspace")).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| e.name == "a.txt" && !e.is_dir && e.size == 5)
    );

    assert_eq!(
        rt.block_on(p.checkpoint("tgtest1", Some("t"))).unwrap(),
        "v1"
    );
    let cps = rt.block_on(p.list_checkpoints("tgtest1")).unwrap();
    assert!(cps.iter().any(|c| c.id == "v1"));
    rt.block_on(p.restore("tgtest1", "v1")).unwrap();
    rt.block_on(p.destroy("tgtest1")).unwrap();

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

    // fs write is PUT with the path + both mkdir query params, body = bytes.
    let w = find("PUT", "/fs/write");
    assert!(w.path.contains("mkdir=true"), "write query: {}", w.path);
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
    let cp = find("POST", "/v1/sprites/tgtest1/checkpoint");
    assert!(
        !cp.path
            .trim_end_matches(|c| c != '/')
            .ends_with("checkpoints/"),
        "create must be singular /checkpoint, got {}",
        cp.path
    );
    assert_eq!(
        cp.path.split('?').next().unwrap(),
        "/v1/sprites/tgtest1/checkpoint"
    );
}

#[test]
fn restore_reports_a_stream_error() {
    let mock = start_mock_with_restore_error(true);
    let (p, _ledger) = recorded(&mock.base_url, "tgtest1");
    let err = rt().block_on(p.restore("tgtest1", "v1")).unwrap_err();
    assert!(err.to_string().contains("boom"), "{err:#}");
}

#[test]
fn restore_refuses_an_unhealthy_checkpoint() {
    let mock = start_mock();
    let (p, _ledger) = recorded(&mock.base_url, "tgtest1");
    let err = rt().block_on(p.restore("tgtest1", "bad")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "sprites: checkpoint bad is unhealthy (mount_failed); not restoring it"
    );
    let requests = mock.recorded.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.method != "POST" || !request.path.ends_with("/restore")),
        "unhealthy checkpoint must not be restored: {requests:?}"
    );
}

#[test]
fn restore_refuses_a_missing_checkpoint() {
    let mock = start_mock();
    let (p, _ledger) = recorded(&mock.base_url, "tgtest1");
    let err = rt().block_on(p.restore("tgtest1", "missing")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "sprites: no checkpoint missing on tgtest1; not restoring"
    );
    let requests = mock.recorded.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.method != "POST" || !request.path.ends_with("/restore")),
        "missing checkpoint must not be restored: {requests:?}"
    );
}

#[test]
fn checkpoint_and_restore_refuse_an_unrecorded_sprite() {
    let mock = start_mock();
    let (p, _ledger) = scoped(&mock.base_url, "tgtest1");
    let checkpoint = rt().block_on(p.checkpoint("tgtest1", None)).unwrap_err();
    assert!(
        checkpoint.to_string().contains("thegn env adopt"),
        "{checkpoint:#}"
    );
    let restore = rt().block_on(p.restore("tgtest1", "v1")).unwrap_err();
    assert!(
        restore.to_string().contains("thegn env adopt"),
        "{restore:#}"
    );
    let requests = mock.recorded.lock().unwrap();
    assert!(
        requests.iter().all(|request| request.method != "POST"),
        "ownership refusal must send no POST: {requests:?}"
    );
}

#[test]
fn fs_delete_sends_the_documented_json_body() {
    let mock = start_mock();
    let p = SpritesProvider::new(&mock.base_url, "tok", "tgtest1");
    rt().block_on(p.delete("tgtest1", "/workspace/a.txt"))
        .unwrap();
    let requests = mock.recorded.lock().unwrap();
    let delete = requests
        .iter()
        .find(|request| {
            request.method == "DELETE" && request.path.contains("/v1/sprites/tgtest1/fs/delete")
        })
        .unwrap_or_else(|| panic!("no documented fs delete request: {requests:?}"));
    let body: serde_json::Value = serde_json::from_slice(&delete.body).unwrap();
    assert_eq!(body["recursive"], true);
    assert_eq!(body["asRoot"], false);
    assert!(
        delete.path.contains("path="),
        "query path missing: {}",
        delete.path
    );
    assert!(
        delete.path.contains("workingDir="),
        "query workingDir missing: {}",
        delete.path
    );
    assert!(
        delete.path.contains("recursive=true"),
        "query recursive missing: {}",
        delete.path
    );
}

#[test]
fn provider_enum_dispatch_against_mock() {
    let mock = start_mock();
    let (sprites, _ledger) = recorded(&mock.base_url, "tgtest1");
    let prov = Provider::Sprites(sprites);
    let rt = rt();
    // The generic enum dispatch reaches the same Sprites impls.
    assert_eq!(rt.block_on(prov.create()).unwrap().id, "tgtest1");
    assert_eq!(rt.block_on(prov.list()).unwrap(), vec!["tgtest1"]);
    rt.block_on(prov.set_network_policy("tgtest1", &[], &["x.com".into()]))
        .unwrap();
    assert_eq!(rt.block_on(prov.checkpoint("tgtest1", None)).unwrap(), "v1");
    rt.block_on(prov.upload_dir(
        "tgtest1",
        std::path::Path::new("/nonexistent-dir-xyz"),
        "/workspace",
    ))
    .unwrap_err(); // local dir missing → error, but the dispatch path is exercised
}

#[test]
fn ensure_exists_skips_when_present_creates_when_absent() {
    let mock = start_mock();
    let rt = rt();
    // The list fixture returns ["tgtest1"], so seed the matching ledger record
    // before ensure_exists proves it and treats it as a no-op…
    let (present, ledger) = scoped(&mock.base_url, "tgtest1");
    let mut intent =
        thegn_svc::sprites::ledger::reserve_at(ledger.path(), &scope(), "tgtest1", 0).unwrap();
    thegn_svc::sprites::ledger::commit_at(ledger.path(), &scope(), "tgtest1").unwrap();
    intent.preserve();
    let present = Provider::Sprites(present);
    assert!(
        !rt.block_on(present.ensure_exists("tgtest1")).unwrap(),
        "present ⇒ no create"
    );
    // …and for an unknown name it creates (the create fixture).
    let (newone, _ledger) = scoped(&mock.base_url, "newone");
    let absent = Provider::Sprites(newone);
    assert!(
        rt.block_on(absent.ensure_exists("newone")).unwrap(),
        "absent ⇒ create"
    );
    // The create endpoint was hit exactly once (only for the absent case).
    let reqs = mock.recorded.lock().unwrap().clone();
    let creates = reqs
        .iter()
        .filter(|r| r.method == "POST" && r.path == "/v1/sprites")
        .count();
    assert_eq!(creates, 1, "create only for the absent sprite: {reqs:?}");
}

#[test]
fn ensure_exists_refuses_a_listed_unrecorded_sprite() {
    let mock = start_mock();
    let (provider, _ledger) = scoped(&mock.base_url, "tgtest1");
    let err = rt()
        .block_on(Provider::Sprites(provider).ensure_exists("tgtest1"))
        .unwrap_err();
    assert!(format!("{err:#}").contains("thegn env adopt"), "{err:#}");

    // The inventory GET and the creation-marker read are the only requests.
    let reqs = mock.recorded.lock().unwrap().clone();
    assert!(
        reqs.iter().all(|r| r.method == "GET"),
        "a refusal must not mutate: {reqs:?}"
    );
    assert_eq!(reqs[0].path, "/v1/sprites");
}

/// Stamp `name`'s creation marker on the mock as `owner` at time `at`.
fn stamp_marker(base: &str, name: &str, owner: &str, at: i64) {
    let other = OwnerScope {
        owner: owner.into(),
        account: scope().account,
    };
    let writer = SpritesProvider::new(base, "tok", name);
    let bytes = thegn_svc::sprites::marker::encode(&other, name, at);
    rt().block_on(ProviderFiles::write(
        &writer,
        name,
        thegn_svc::sprites::marker::MARKER_PATH,
        &bytes,
    ))
    .unwrap();
}

#[test]
fn ensure_exists_adopts_a_sprite_marked_by_another_instance_once_its_lease_lapsed() {
    let mock = start_mock();
    stamp_marker(&mock.base_url, "tgtest1", "ctl-other-instance", 0);
    let (provider, ledger) = scoped(&mock.base_url, "tgtest1");
    let created = rt()
        .block_on(Provider::Sprites(provider).ensure_exists("tgtest1"))
        .unwrap();
    assert!(!created, "reattach, never a create");
    assert!(
        thegn_svc::sprites::ledger::prove_owned_at(ledger.path(), &scope(), "tgtest1", &[]).is_ok(),
        "the sprite is now recorded as ours"
    );
    let reqs = mock.recorded.lock().unwrap().clone();
    assert!(
        reqs.iter()
            .all(|r| r.method != "POST" && r.method != "DELETE"),
        "{reqs:?}"
    );
}

#[test]
fn ensure_exists_refuses_a_sprite_whose_marker_lease_is_live() {
    let mock = start_mock();
    stamp_marker(
        &mock.base_url,
        "tgtest1",
        "ctl-other-instance",
        thegn_core::util::now(),
    );
    let (provider, ledger) = scoped(&mock.base_url, "tgtest1");
    let err = rt()
        .block_on(Provider::Sprites(provider).ensure_exists("tgtest1"))
        .unwrap_err();
    assert!(format!("{err:#}").contains("thegn env adopt"), "{err:#}");
    assert!(
        thegn_svc::sprites::ledger::prove_owned_at(ledger.path(), &scope(), "tgtest1", &[])
            .is_err(),
        "nothing was recorded"
    );
}

#[test]
fn ensure_exists_refuses_without_scope() {
    let mock = start_mock();
    let provider = SpritesProvider::new(&mock.base_url, "tok", "tgtest1");
    let err = rt()
        .block_on(Provider::Sprites(provider).ensure_exists("tgtest1"))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("no controller id; refusing to use sprite tgtest1"),
        "{err:#}"
    );

    let reqs = mock.recorded.lock().unwrap().clone();
    assert_eq!(reqs.len(), 1, "only the inventory GET is allowed: {reqs:?}");
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(reqs[0].path, "/v1/sprites");
}

#[test]
fn write_exec_uses_0755_mode() {
    let mock = start_mock();
    let p = SpritesProvider::new(&mock.base_url, "tok", "tgtest1");
    rt().block_on(p.write_exec("tgtest1", "/workspace/.tg/thegn", b"ELF"))
        .unwrap();
    let reqs = mock.recorded.lock().unwrap().clone();
    let w = reqs
        .iter()
        .find(|r| r.method == "PUT" && r.path.contains("/fs/write"))
        .expect("a write request");
    assert!(
        w.path.contains("mode=0755"),
        "exec write must be 0755: {}",
        w.path
    );
    assert_eq!(w.body, b"ELF");
}

#[test]
fn ensure_executable_pushes_once_then_idempotent() {
    let mock = start_mock();
    let prov = Provider::Sprites(SpritesProvider::new(&mock.base_url, "tok", "tgtest1"));
    let rt = rt();
    let bin = b"fake-thegn-musl-binary";
    let path = "/workspace/.tg/thegn";

    // First install: missing remote binary → stages, verifies, and publishes it.
    assert!(
        rt.block_on(prov.ensure_executable("tgtest1", path, bin))
            .unwrap(),
        "first ensure should push"
    );
    // Second with identical bytes: the verified remote SHA-256 matches, so no upload.
    assert!(
        !rt.block_on(prov.ensure_executable("tgtest1", path, bin))
            .unwrap(),
        "second ensure (same bytes) should be a no-op"
    );
    // Changed bytes → SHA-256 mismatch → re-stages and publishes.
    assert!(
        rt.block_on(prov.ensure_executable("tgtest1", path, b"different-bytes"))
            .unwrap(),
        "changed bytes should re-push"
    );

    // The 0755 staged binary write happened exactly twice (first + after
    // change), never on the idempotent no-op.
    let reqs = mock.recorded.lock().unwrap().clone();
    let exec_writes = reqs
        .iter()
        .filter(|r| {
            r.method == "PUT" && r.path.contains("/fs/write") && r.path.contains("mode=0755")
        })
        .count();
    assert_eq!(
        exec_writes, 2,
        "binary should push twice, not on the no-op: {reqs:?}"
    );
}

// ---- b160 sx-ledger-reap: the ownership ledger around create/destroy ----

/// [`scoped`], with a `ready` ledger record for `name` already written (a
/// sprite this host created), so destroy has its proof.
fn recorded(base: &str, name: &str) -> (SpritesProvider, tempfile::TempDir) {
    let (provider, ledger) = scoped(base, name);
    thegn_svc::sprites::ledger::reserve_at(ledger.path(), &scope(), name, 0)
        .unwrap()
        .preserve();
    thegn_svc::sprites::ledger::commit_at(ledger.path(), &scope(), name).unwrap();
    (provider, ledger)
}

/// How the scripted create mock answers `POST /sprites`.
#[derive(Clone, Copy)]
enum CreateReply {
    Created,
    BadRequest,
    /// Close the connection without answering (an ambiguous outcome).
    Drop,
}

/// What the scripted create mock saw: POSTs, and the ledger state of the
/// sprite's record at the moment each POST arrived (`None` = no record file).
#[derive(Default)]
struct CreateSeen {
    posts: usize,
    states_at_post: Vec<Option<String>>,
}

/// A create mock that, on each `POST /sprites`, reads `ledger/<name>.json`
/// (so the test proves the intent was written **before** the request) and
/// then answers per `reply`.
fn start_ledger_create_mock(
    ledger: std::path::PathBuf,
    name: &'static str,
    reply: CreateReply,
) -> (String, Arc<Mutex<CreateSeen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(CreateSeen::default()));
    let shared = seen.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = shared.clone();
            let ledger = ledger.clone();
            thread::spawn(move || {
                let mut writer = stream.try_clone().unwrap();
                let mut reader = stream;
                let mut line = String::new();
                if read_line(&mut reader, &mut line).is_none() {
                    return;
                }
                let method = line.split_whitespace().next().unwrap_or("").to_string();
                let mut content_length = 0usize;
                loop {
                    let mut h = String::new();
                    if read_line(&mut reader, &mut h).is_none() {
                        break;
                    }
                    let t = h.trim_end();
                    if t.is_empty() {
                        break;
                    }
                    if let Some(v) = t.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    let _ = reader.read_exact(&mut body); // best-effort: client may have disconnected
                }
                let (status, resp): (&str, Vec<u8>) = if method == "POST" {
                    let state = std::fs::read(ledger.join(format!("{name}.json")))
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                        .and_then(|v| v["state"].as_str().map(str::to_string));
                    {
                        let mut seen = shared.lock().unwrap();
                        seen.posts += 1;
                        seen.states_at_post.push(state);
                    }
                    match reply {
                        CreateReply::Created => (
                            "200 OK",
                            format!(r#"{{"id":"x","name":"{name}","status":"cold"}}"#).into_bytes(),
                        ),
                        CreateReply::BadRequest => {
                            ("400 Bad Request", br#"{"error":"bad name"}"#.to_vec())
                        }
                        CreateReply::Drop => return, // close without a response
                    }
                } else {
                    ("404 Not Found", b"no route".to_vec())
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    resp.len()
                );
                let _ = writer.write_all(head.as_bytes()); // best-effort: client may have disconnected
                let _ = writer.write_all(&resp); // best-effort: client may have disconnected
                let _ = writer.flush(); // best-effort: client may have disconnected
            });
        }
    });
    (format!("http://127.0.0.1:{port}/v1"), seen)
}

fn ledger_state(ledger: &tempfile::TempDir, name: &str) -> Option<String> {
    thegn_svc::vps::registry::read_checked_at(ledger.path(), name)
        .unwrap()
        .map(|record| record.state)
}

#[test]
fn destroy_of_a_recorded_sprite_deletes_and_forgets_the_record() {
    let (base, deletes) = start_delete_mock(0);
    let (p, ledger) = recorded(&base, "tgtest1");
    rt().block_on(p.destroy("tgtest1")).unwrap();
    assert_eq!(*deletes.lock().unwrap(), 1);
    assert_eq!(ledger_state(&ledger, "tgtest1"), None, "record forgotten");
}

#[test]
fn create_writes_the_creating_record_before_the_post() {
    let ledger = tempfile::tempdir().unwrap();
    let (base, seen) =
        start_ledger_create_mock(ledger.path().to_path_buf(), "sx-new", CreateReply::Created);
    let p = SpritesProvider::new(&base, "tok", "sx-new")
        .with_scope(scope())
        .with_ledger_dir(ledger.path().to_path_buf());
    assert_eq!(rt().block_on(p.create()).unwrap().id, "sx-new");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.posts, 1);
    assert_eq!(
        seen.states_at_post,
        [Some("creating".to_string())],
        "the intent existed when the POST arrived"
    );
    assert_eq!(ledger_state(&ledger, "sx-new").as_deref(), Some("ready"));
}

#[test]
fn create_refused_with_400_leaves_no_record() {
    let ledger = tempfile::tempdir().unwrap();
    let (base, seen) = start_ledger_create_mock(
        ledger.path().to_path_buf(),
        "sx-new",
        CreateReply::BadRequest,
    );
    let p = SpritesProvider::new(&base, "tok", "sx-new")
        .with_scope(scope())
        .with_ledger_dir(ledger.path().to_path_buf());
    let err = rt().block_on(p.create()).unwrap_err();
    assert!(err.to_string().contains("400"), "{err}");
    assert_eq!(
        seen.lock().unwrap().posts,
        1,
        "a definite 4xx is not retried"
    );
    assert_eq!(ledger_state(&ledger, "sx-new"), None, "no record left");
}

#[test]
fn create_with_a_dropped_connection_keeps_the_creating_record() {
    let ledger = tempfile::tempdir().unwrap();
    let (base, seen) =
        start_ledger_create_mock(ledger.path().to_path_buf(), "sx-new", CreateReply::Drop);
    let p = SpritesProvider::new(&base, "tok", "sx-new")
        .with_scope(scope())
        .with_ledger_dir(ledger.path().to_path_buf());
    rt().block_on(p.create()).unwrap_err();
    assert!(seen.lock().unwrap().posts >= 1);
    assert_eq!(
        ledger_state(&ledger, "sx-new").as_deref(),
        Some("creating"),
        "an ambiguous create keeps its intent for the reaper"
    );
}

#[test]
fn create_refuses_an_echoed_name_that_differs() {
    let ledger = tempfile::tempdir().unwrap();
    let (base, _posts) = start_create_mock_with_echo(0, "other");
    let p = SpritesProvider::new(&base, "tok", "newone")
        .with_scope(scope())
        .with_ledger_dir(ledger.path().to_path_buf());
    let err = rt().block_on(p.create()).unwrap_err();
    assert!(err.to_string().contains("not the requested"), "{err:#}");
    assert_eq!(ledger_state(&ledger, "newone"), None);
}

#[test]
fn destroy_of_an_adopted_sprite_sends_one_delete() {
    // The sprite exists (probe GET answers 200) but is not ours until adopted.
    let (base, deletes) = start_delete_mock_get(0, "200 OK");
    let (p, ledger) = scoped(&base, "sx-adopted");
    // Not ours until adopted: no DELETE.
    rt().block_on(p.destroy("sx-adopted")).unwrap_err();
    assert_eq!(*deletes.lock().unwrap(), 0);
    assert_eq!(
        thegn_svc::sprites::ledger::adopt_at(ledger.path(), &scope(), "sx-adopted", 0).unwrap(),
        thegn_svc::sprites::ledger::AdoptOutcome::Adopted
    );
    assert_eq!(
        ledger_state(&ledger, "sx-adopted").as_deref(),
        Some("ready")
    );
    rt().block_on(p.destroy("sx-adopted")).unwrap();
    assert_eq!(*deletes.lock().unwrap(), 1, "exactly one DELETE");
    assert_eq!(
        ledger_state(&ledger, "sx-adopted"),
        None,
        "record forgotten"
    );
}

#[test]
fn write_private_uses_0600_mode() {
    // b162 credential custody: an agent login file lands owner-only.
    let mock = start_mock();
    let p = Provider::Sprites(SpritesProvider::new(&mock.base_url, "tok", "tgtest1"));
    rt().block_on(p.write_private("tgtest1", "/root/.codex/auth.json", b"{}"))
        .unwrap();
    let reqs = mock.recorded.lock().unwrap().clone();
    let w = reqs
        .iter()
        .find(|r| r.method == "PUT" && r.path.contains("/fs/write"))
        .expect("a write request");
    assert!(
        w.path.contains("mode=0600"),
        "a private write must be 0600: {}",
        w.path
    );
    assert!(!w.path.contains("mode=0644"), "{}", w.path);
    assert_eq!(w.body, b"{}");
}
