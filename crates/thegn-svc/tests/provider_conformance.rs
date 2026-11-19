//! The provider conformance suite (`thegn_svc::provider_conformance::suite`,
//! first slice of WS08-P1-08) run against a minimal sprites fake server — a
//! trimmed copy of the one in `sprites_mock.rs`, made stateful (created
//! sprites, per-name DELETE counts, ledger-at-create) so the suite's cases have
//! something to observe. Hermetic: no token, no network, no cost.
//!
//! WS08-P1-08 slice 2 (fly, vps): minimal fakes copied from `fly_mock.rs` and
//! `vps_mock.rs` run the same suite below. Slice 3: record kept until proven
//! gone.
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
    let exit = vec![0x03, 7];
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
        cap: None,
        supports_stop: false,
        // Sprites' 30 s confirmation budget is not injectable from an integration test; the same contract is unit-tested in provider.rs `sprites_destroy_keeps_record_and_custody_when_confirmation_times_out`.
        hold_after_delete: None,
        recorded: None,
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert_eq!(
        report.skipped,
        vec![
            "reads_never_resume",
            "create_respects_cap",
            "stop_resume_round_trip",
            "destroy_keeps_record_until_gone"
        ],
        "{report:?}"
    );
}

const TEST_PUBKEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIgVgF3FLyN2aHUalBpkk3cMVfTgD+7TrbdfTAcSvLvB thegn";

/// One recorded request line of a Fly/VPS fake.
type Requests = Arc<Mutex<Vec<(String, String, String)>>>;

/// Read one HTTP request; returns (method, path, body).
fn read_request(stream: &TcpStream) -> Option<(String, String, String)> {
    use std::io::{BufRead, BufReader};
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some((method, path, String::from_utf8_lossy(&body).into_owned()))
}

fn reply(mut stream: TcpStream, resp: Option<String>) {
    let out = match resp {
        Some(r) => format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{r}",
            r.len()
        ),
        None => "HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 2\r\n\r\n{}".into(),
    };
    stream.write_all(out.as_bytes()).unwrap_or_default(); // best-effort: client may have gone
}

fn delete_counter(
    requests: Requests,
    needle: &'static str,
) -> thegn_svc::provider_conformance::Probe<thegn_svc::provider_conformance::DeleteCounter> {
    Box::new(move || {
        let requests = requests.clone();
        Box::new(move |_name: &str| {
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|(m, p, _)| m == "DELETE" && p.contains(needle))
                .count()
        })
    })
}

fn fly_fake() -> (String, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let shared = requests.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = shared.clone();
            thread::spawn(move || {
                let Some((method, path, body)) = read_request(&stream) else {
                    return;
                };
                shared
                    .lock()
                    .unwrap()
                    .push((method.clone(), path.clone(), body.clone()));
                let managed = r#"{"managed-by":"thegn","tg-host":"h1"}"#;
                let resp = if path == "/graphql" {
                    Some(if body.contains("allocateIpAddress") {
                        r#"{"data":{"allocateIpAddress":{"ipAddress":{"id":"ip_x","address":"137.0.0.99","type":"v4"}}}}"#.to_string()
                    } else {
                        r#"{"data":{"app":{"ipAddresses":{"nodes":[]}}}}"#.to_string()
                    })
                } else if method == "GET"
                    && path.starts_with("/v1/apps/")
                    && !path.contains("/machines")
                {
                    Some(r#"{"name":"tg-app","status":"deployed"}"#.to_string())
                } else if method == "POST" && path.ends_with("/machines") {
                    Some(format!(
                        r#"{{"id":"90810","name":"tg-fly-1","state":"created","config":{{"metadata":{managed}}}}}"#
                    ))
                } else if method == "DELETE" && path.starts_with("/v1/apps/") {
                    Some(r#"{"ok":true}"#.to_string())
                } else {
                    None
                };
                reply(stream, resp);
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), requests)
}

#[tokio::test]
async fn fly_passes_the_conformance_suite() {
    use thegn_svc::fly::{FlyProvider, FlySpec};
    let _state = thegn_core::testenv::IsolatedState::new();
    let tmp = tempfile::tempdir().unwrap();
    let (base, _requests) = fly_fake();
    let p = Provider::Fly(FlyProvider::new(FlySpec {
        api_base: format!("{base}/v1"),
        graphql_url: format!("{base}/graphql"),
        token: "mock-token".into(),
        org_slug: String::new(),
        name: "tg-fly-1".into(),
        region: String::new(),
        size: String::new(),
        image: String::new(),
        max_instances: 0,
        max_lifetime_secs: 0,
        key_path: tmp.path().join("key"),
        pubkey: TEST_PUBKEY.into(),
        iroh: None,
        skip_ready_wait: true,
    }));
    // The suite's sandbox must exist (and be in the ledger) up front.
    p.create().await.expect("fly create");
    let fx = Fixture {
        created: "tg-fly-1".into(),
        missing: "tgconf-missing".into(),
        foreign: None,
        supports_run: false,
        run_expect: (0, ""),
        ledger_at_create: None,
        // Fly's destroy is 404-tolerant and re-sends the DELETE by design; idempotence is asserted by the Ok result only.
        deletes_for: None,
        resumes: None,
        cap: None,
        // The minimal fake serves no machine list/stop routes.
        supports_stop: false,
        // UNVERIFIED: whether Fly/VPS keep the record until absent is queued.
        hold_after_delete: None,
        recorded: None,
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert!(
        report.skipped.contains(&"destroy_keeps_record_until_gone"),
        "{report:?}"
    );
}

#[tokio::test]
async fn vps_passes_the_conformance_suite() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use thegn_svc::vps::{VpsKind, VpsProvider, VpsSpec};
    let _state = thegn_core::testenv::IsolatedState::new();
    let tmp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let present = Arc::new(AtomicBool::new(true));
    let (shared, present_srv) = (requests.clone(), present.clone());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (shared, present) = (shared.clone(), present_srv.clone());
            thread::spawn(move || {
                let Some((method, path, body)) = read_request(&stream) else {
                    return;
                };
                shared
                    .lock()
                    .unwrap()
                    .push((method.clone(), path.clone(), body));
                let page = r#""meta":{"pagination":{"page":1,"per_page":50,"previous_page":null,"next_page":null,"last_page":1,"total_entries":1}}"#;
                let resp = match (method.as_str(), path.as_str()) {
                    ("GET", "/v1/ssh_keys?page=1&per_page=50") => Some(format!(
                        r#"{{"ssh_keys":[{{"id":9,"public_key":"ssh-ed25519 OTHERKEY someone"}}],{page}}}"#
                    )),
                    ("POST", "/v1/ssh_keys") => Some(r#"{"ssh_key":{"id":42}}"#.to_string()),
                    ("POST", "/v1/servers") => Some(r#"{"server":{"id":101,"name":"tg-mock-1","status":"initializing","created":"2026-07-01T12:00:00+00:00"}}"#.to_string()),
                    ("GET", p) if p.starts_with("/v1/servers?") => Some(
                        if present.load(Ordering::Acquire) {
                            format!(r#"{{"servers":[{{"id":101,"name":"tg-mock-1","status":"running","created":"2026-07-01T12:00:00+00:00","public_net":{{"ipv4":{{"ip":"203.0.113.9"}}}},"labels":{{"managed-by":"thegn","tg-host":"h1"}}}}],{page}}}"#)
                        } else {
                            r#"{"servers":[],"meta":{"pagination":{"page":1,"per_page":50,"previous_page":null,"next_page":null,"last_page":1,"total_entries":0}}}"#.to_string()
                        },
                    ),
                    ("DELETE", "/v1/servers/101") => {
                        present.store(false, Ordering::Release);
                        Some("{}".to_string())
                    }
                    _ => None,
                };
                reply(stream, resp);
            });
        }
    });
    let p = Provider::Vps(VpsProvider::new(VpsSpec {
        kind: VpsKind::Hetzner,
        api_base: format!("http://127.0.0.1:{port}/v1"),
        token: "mock-token".into(),
        name: "tg-mock-1".into(),
        region: String::new(),
        size: String::new(),
        image: String::new(),
        max_instances: 0,
        max_lifetime_secs: 0,
        key_path: tmp.path().join("key"),
        pubkey: TEST_PUBKEY.into(),
        skip_ready_wait: true,
    }));
    p.create().await.expect("vps create");
    let fx = Fixture {
        created: "tg-mock-1".into(),
        missing: "tgconf-missing".into(),
        foreign: None,
        supports_run: false,
        run_expect: (0, ""),
        ledger_at_create: None,
        deletes_for: Some(delete_counter(requests, "/v1/servers/")),
        resumes: None,
        cap: None,
        // A powered-off VPS still bills: no stop by construction.
        supports_stop: false,
        // UNVERIFIED: whether Fly/VPS keep the record until absent is queued.
        hold_after_delete: None,
        recorded: None,
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert!(
        report.skipped.contains(&"destroy_keeps_record_until_gone"),
        "{report:?}"
    );
}
