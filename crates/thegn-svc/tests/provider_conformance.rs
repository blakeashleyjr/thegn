//! The provider conformance suite (`thegn_svc::provider_conformance::suite`,
//! first slice of WS08-P1-08) run against a minimal sprites fake server — a
//! trimmed copy of the one in `sprites_mock.rs`, made stateful (created
//! sprites, per-name DELETE counts, ledger-at-create) so the suite's cases have
//! something to observe. Hermetic: no token, no network, no cost.
//!
//! Run: `cargo test -p thegn-svc --test provider_conformance`.

use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use thegn_svc::provider::{Provider, SpritesProvider};
use thegn_svc::provider_conformance::{Fixture, suite};
use thegn_svc::sprites::ledger::OwnerScope;
use thegn_svc::vps::registry;

const NAME: &str = "tgconf1";
const FOREIGN: &str = "tgconf-foreign";
const MISSING: &str = "tgconf-missing";

fn scope() -> OwnerScope {
    OwnerScope {
        owner: "ctl-sprites-conformance".into(),
        account: "acct1-sprites-conformance".into(),
    }
}

#[derive(Default)]
struct State {
    sprites: BTreeSet<String>,
    deletes: HashMap<String, usize>,
    ledger_at_create: Option<bool>,
}

struct Fake {
    base_url: String,
    state: Arc<Mutex<State>>,
}

fn start(ledger_file: PathBuf) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Mutex::new(State::default()));
    state.lock().unwrap().sprites.insert(FOREIGN.into());
    let shared = state.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (state, ledger_file) = (shared.clone(), ledger_file.clone());
            thread::spawn(move || handle(stream, &state, &ledger_file));
        }
    });
    Fake {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        state,
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

fn handle(mut stream: TcpStream, state: &Mutex<State>, ledger_file: &std::path::Path) {
    let mut line = String::new();
    if read_line(&mut stream, &mut line).is_none() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut content_length = 0usize;
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
        if let Some((name, value)) = t.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
        if let Some(v) = t.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && stream.read_exact(&mut body).is_err() {
        return;
    }
    let path = target.split('?').next().unwrap_or(&target).to_string();
    if method == "GET" && path.ends_with("/exec") {
        exec_websocket(stream, &headers);
        return;
    }
    let (status, resp): (&str, Vec<u8>) = match (method.as_str(), path.as_str()) {
        ("POST", "/v1/sprites") => {
            let mut st = state.lock().unwrap();
            st.ledger_at_create = Some(ledger_file.exists());
            st.sprites.insert(NAME.into());
            (
                "200 OK",
                format!(r#"{{"id":"sprite-1","name":"{NAME}","status":"cold"}}"#).into_bytes(),
            )
        }
        ("GET", "/v1/sprites") => {
            let st = state.lock().unwrap();
            let rows: Vec<String> = st
                .sprites
                .iter()
                .map(|n| format!(r#"{{"id":"id-{n}","name":"{n}","status":"warm"}}"#))
                .collect();
            (
                "200 OK",
                format!(r#"{{"sprites":[{}],"has_more":false}}"#, rows.join(",")).into_bytes(),
            )
        }
        ("GET", p) if p.starts_with("/v1/sprites/") => {
            let name = p.trim_start_matches("/v1/sprites/");
            let st = state.lock().unwrap();
            if st.sprites.contains(name) {
                (
                    "200 OK",
                    format!(r#"{{"id":"id-{name}","name":"{name}","status":"warm"}}"#).into_bytes(),
                )
            } else {
                ("404 Not Found", b"missing".to_vec())
            }
        }
        ("DELETE", p) if p.starts_with("/v1/sprites/") => {
            let name = p.trim_start_matches("/v1/sprites/").to_string();
            let mut st = state.lock().unwrap();
            *st.deletes.entry(name.clone()).or_default() += 1;
            if st.sprites.remove(&name) {
                ("204 No Content", Vec::new())
            } else {
                ("404 Not Found", b"missing".to_vec())
            }
        }
        _ => ("404 Not Found", b"no route".to_vec()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        resp.len()
    );
    stream.write_all(head.as_bytes()).unwrap_or_default(); // best-effort: client may have disconnected
    stream.write_all(&resp).unwrap_or_default(); // best-effort: client may have disconnected
    stream.flush().unwrap_or_default(); // best-effort: client may have disconnected
}

/// Complete the exec WebSocket handshake and answer every command with one
/// stdout frame and exit status 7 (the non-tty framing the provider reads).
fn exec_websocket(mut stream: TcpStream, headers: &HashMap<String, String>) {
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::{Role, WebSocket};
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
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    let mut out = vec![1];
    out.extend_from_slice(b"conformance-out\n");
    if socket.send(Message::Binary(out.into())).is_err() {
        return;
    }
    let mut exit = vec![3];
    exit.extend_from_slice(b"7");
    // best-effort: the client may already have hung up.
    socket
        .send(Message::Binary(exit.into()))
        .unwrap_or_default(); // best-effort: client may have gone
}

#[tokio::test]
async fn sprites_passes_the_conformance_suite() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let ledger = tempfile::tempdir().unwrap();
    let fake = start(ledger.path().join(format!("{NAME}.json")));
    let scope = scope();
    // A stale ready record for a sprite the API no longer has: destroying it
    // is cleanup, not an error.
    registry::write_at(
        ledger.path(),
        &registry::VpsRecord {
            name: MISSING.into(),
            provider: "sprites".into(),
            state: "ready".into(),
            instance_id: String::new(),
            ip: String::new(),
            created_at: 1_700_000_000,
            owner: scope.owner.clone(),
            account: scope.account.clone(),
        },
    )
    .unwrap();
    let provider = SpritesProvider::new(&fake.base_url, "tok", NAME)
        .with_scope(scope)
        .with_ledger_dir(ledger.path().to_path_buf());
    let p = Provider::Sprites(provider);
    let (seen, counts) = (fake.state.clone(), fake.state.clone());
    let fx = Fixture {
        created: NAME.into(),
        missing: MISSING.into(),
        foreign: Some(FOREIGN.into()),
        supports_run: true,
        run_expect: (7, "conformance-out"),
        ledger_at_create: Some(Box::new(move || seen.lock().unwrap().ledger_at_create)),
        deletes_for: Some(Box::new(move || {
            let state = counts.clone();
            Box::new(move |name: &str| {
                state
                    .lock()
                    .unwrap()
                    .deletes
                    .get(name)
                    .copied()
                    .unwrap_or(0)
            })
        })),
        // Sprites reads never resume anything: the case is n/a.
        resumes: None,
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert_eq!(report.skipped, vec!["reads_never_resume"], "{report:?}");
}
