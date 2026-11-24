//! The provider conformance suite (`thegn_svc::provider_conformance::suite`,
//! first slice of WS08-P1-08) run against a minimal sprites fake server — a
//! trimmed copy of the one in `sprites_mock.rs`, made stateful (created
//! sprites, per-name DELETE counts, ledger-at-create) so the suite's cases have
//! something to observe. Hermetic: no token, no network, no cost.
//!
//! WS08-P1-08 slice 2 (fly, vps): minimal fakes copied from `fly_mock.rs` and
//! `vps_mock.rs` run the same suite below. Slice 3: record kept until proven
//! gone. Slice 4: unfinished listings, missing exit status, stopped retry,
//! egress.
//!
//! Run: `cargo test -p thegn-svc --test provider_conformance`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    held: BTreeSet<String>,
    deletes: HashMap<String, usize>,
    ledger_at_create: Option<bool>,
    drop_exit: bool,
    unfinished: bool,
    wakes: usize,
    policies: HashMap<String, String>,
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
        let drop_exit = {
            let mut st = state.lock().unwrap();
            st.wakes += 1;
            st.drop_exit
        };
        exec_websocket(stream, &headers, drop_exit);
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
                if st.unfinished {
                    format!(r#"{{"sprites":[{}],"has_more":true}}"#, rows.join(",")).into_bytes()
                } else {
                    format!(r#"{{"sprites":[{}],"has_more":false}}"#, rows.join(",")).into_bytes()
                },
            )
        }
        ("POST", p) if p.starts_with("/v1/sprites/") && p.ends_with("/policy/network") => {
            let name = p
                .trim_start_matches("/v1/sprites/")
                .trim_end_matches("/policy/network");
            let mut st = state.lock().unwrap();
            st.policies.insert(
                name.to_string(),
                String::from_utf8_lossy(&body).into_owned(),
            );
            ("200 OK", b"{}".to_vec())
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
            if st.held.contains(&name) || st.sprites.remove(&name) {
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
/// With `drop_exit` the socket closes after the stdout frame, with no exit status.
fn exec_websocket(mut stream: TcpStream, headers: &HashMap<String, String>, drop_exit: bool) {
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
    if drop_exit {
        if socket.close(None).is_err() {
            return;
        }
        if socket.flush().is_err() {
            return;
        }
        return;
    }
    let exit = vec![0x03, 7];
    if socket.send(Message::Binary(exit.into())).is_err() {
        return; // the client may already have hung up
    }
    // Do not drop the socket right after the exit frame: the client's unread
    // Ping would turn the close into an RST that can discard the exit frame
    // (seen in the pre-push run). Read until the client closes, bounded.
    if socket
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .is_err()
    {
        return;
    }
    while let Ok(message) = socket.read() {
        if matches!(message, Message::Close(_)) {
            break;
        }
    }
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
        .with_ledger_dir(ledger.path().to_path_buf())
        .with_destroy_confirm(Duration::from_millis(200), Duration::from_millis(20));
    let p = Provider::Sprites(provider);
    let (seen, counts, held) = (fake.state.clone(), fake.state.clone(), fake.state.clone());
    let wakes = fake.state.clone();
    let drop_exit = fake.state.clone();
    let unfinished_listing = fake.state.clone();
    let policies = fake.state.clone();
    let ledger_dir = ledger.path().to_path_buf();
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
        // Sprites wakes on exec; a read must not open one.
        resumes: Some(Box::new(move || wakes.lock().unwrap().wakes)),
        cap: None,
        supports_stop: false,
        hold_after_delete: Some(Box::new(move |name, hold| {
            let mut state = held.lock().unwrap();
            if hold {
                state.held.insert(name.to_string());
            } else {
                state.held.remove(name);
            }
        })),
        recorded: Some(Box::new(move |name| {
            registry::read_at(&ledger_dir, name).is_some()
        })),
        drop_exit: Some(Box::new(move |on| {
            drop_exit.lock().unwrap().drop_exit = on;
        })),
        refuse_runs_stopped: None,
        runs: None,
        unfinished_listing: Some(Box::new(move |on| {
            unfinished_listing.lock().unwrap().unfinished = on;
        })),
        egress_allows_all: Some(Box::new(move |name| {
            let st = policies.lock().unwrap();
            let body = st.policies.get(name)?;
            let value = serde_json::from_str::<serde_json::Value>(body).ok()?;
            let rules = value.get("rules")?.as_array()?;
            Some(!rules.iter().any(|rule| {
                rule.get("domain").and_then(serde_json::Value::as_str) == Some("*")
                    && rule.get("action").and_then(serde_json::Value::as_str) == Some("deny")
            }))
        })),
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert_eq!(
        report.skipped,
        vec![
            "stopped_run_is_retried_once",
            "create_respects_cap",
            "stop_resume_round_trip"
        ],
        "{report:?}"
    );
    assert!(
        report.passed.contains(&"exec_without_exit_fails")
            && report.passed.contains(&"reads_never_resume")
            && report
                .passed
                .contains(&"unfinished_listing_is_never_complete")
            && report.passed.contains(&"egress_never_widens")
            && report.passed.contains(&"destroy_keeps_record_until_gone"),
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

fn fly_fake() -> (String, Requests, Arc<Mutex<bool>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Requests = Arc::new(Mutex::new(Vec::new()));
    let deleted = Arc::new(Mutex::new(HashSet::new()));
    let hold_after_delete = Arc::new(Mutex::new(false));
    let shared = requests.clone();
    let deleted_server = deleted.clone();
    let hold_server = hold_after_delete.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = shared.clone();
            let deleted = deleted_server.clone();
            let hold_after_delete = hold_server.clone();
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
                    if deleted.lock().unwrap().contains(&path)
                        && !*hold_after_delete.lock().unwrap()
                    {
                        None
                    } else {
                        Some(r#"{"name":"tg-app","status":"deployed"}"#.to_string())
                    }
                } else if method == "POST" && path.ends_with("/machines") {
                    Some(format!(
                        r#"{{"id":"90810","name":"tg-fly-1","state":"created","config":{{"metadata":{managed}}}}}"#
                    ))
                } else if method == "DELETE" && path.starts_with("/v1/apps/") {
                    deleted.lock().unwrap().insert(path.clone());
                    Some(r#"{"ok":true}"#.to_string())
                } else {
                    None
                };
                reply(stream, resp);
            });
        }
    });
    (
        format!("http://127.0.0.1:{port}"),
        requests,
        hold_after_delete,
    )
}

#[tokio::test]
async fn fly_passes_the_conformance_suite() {
    use thegn_svc::fly::{FlyProvider, FlySpec};
    let _state = thegn_core::testenv::IsolatedState::new();
    let tmp = tempfile::tempdir().unwrap();
    let (base, _requests, hold_after_delete) = fly_fake();
    let fly = FlyProvider::new(FlySpec {
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
    })
    .with_destroy_confirm(Duration::from_millis(200), Duration::from_millis(20));
    let p = Provider::Fly(fly);
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
        hold_after_delete: Some(Box::new(move |_name, held| {
            *hold_after_delete.lock().unwrap() = held;
        })),
        recorded: Some(Box::new(|name| {
            thegn_svc::vps::registry::read(name).is_some()
        })),
        drop_exit: None,
        refuse_runs_stopped: None,
        runs: None,
        unfinished_listing: None,
        egress_allows_all: None,
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert_eq!(
        report.skipped,
        vec![
            "create_records_before_request",
            "destroy_unrecorded_refused",
            "run_captures_exit_and_output",
            "exec_without_exit_fails",
            "stopped_run_is_retried_once",
            "reads_never_resume",
            "unfinished_listing_is_never_complete",
            "create_respects_cap",
            "egress_never_widens",
            "stop_resume_round_trip",
        ],
        "{report:?}"
    );
    assert!(
        report.passed.contains(&"destroy_keeps_record_until_gone"),
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
    let hold = Arc::new(AtomicBool::new(false));
    let unfinished = Arc::new(AtomicBool::new(false));
    let (shared, present_srv, hold_srv, unfinished_srv) = (
        requests.clone(),
        present.clone(),
        hold.clone(),
        unfinished.clone(),
    );
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (shared, present, hold, unfinished) = (
                shared.clone(),
                present_srv.clone(),
                hold_srv.clone(),
                unfinished_srv.clone(),
            );
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
                    ("GET", p) if p.starts_with("/v1/servers?") => {
                        let present = u8::from(present.load(Ordering::Acquire));
                        let unfinished = u8::from(unfinished.load(Ordering::Acquire));
                        let total_entries = u64::from(present) + u64::from(unfinished);
                        let server_page = format!(
                            r#""meta":{{"pagination":{{"page":1,"per_page":50,"previous_page":null,"next_page":null,"last_page":1,"total_entries":{total_entries}}}}}"#
                        );
                        Some(if present != 0 {
                            format!(r#"{{"servers":[{{"id":101,"name":"tg-mock-1","status":"running","created":"2026-07-01T12:00:00+00:00","public_net":{{"ipv4":{{"ip":"203.0.113.9"}}}},"labels":{{"managed-by":"thegn","tg-host":"h1"}}}}],{server_page}}}"#)
                        } else {
                            format!(r#"{{"servers":[],{server_page}}}"#)
                        })
                    },
                    ("DELETE", "/v1/servers/101") => {
                        if !hold.load(Ordering::Acquire) {
                            present.store(false, Ordering::Release);
                        }
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
        // VPS keeps the ledger record until the inventory confirms the instance is gone (confirm_absent).
        hold_after_delete: Some(Box::new(move |_name, held| {
            hold.store(held, Ordering::Release);
        })),
        recorded: Some(Box::new(|name| {
            thegn_svc::vps::registry::read(name).is_some()
        })),
        unfinished_listing: Some(Box::new(move |on| {
            unfinished.store(on, Ordering::Release);
        })),
        drop_exit: None,
        refuse_runs_stopped: None,
        runs: None,
        egress_allows_all: None,
    };
    let report = suite(&p, &fx).await;
    report.assert_green();
    assert_eq!(
        report.skipped,
        vec![
            "create_records_before_request",
            "destroy_unrecorded_refused",
            "run_captures_exit_and_output",
            "exec_without_exit_fails",
            "stopped_run_is_retried_once",
            "reads_never_resume",
            "create_respects_cap",
            "egress_never_widens",
            "stop_resume_round_trip",
        ],
        "{report:?}"
    );
    assert!(
        report
            .passed
            .contains(&"unfinished_listing_is_never_complete")
            && report.passed.contains(&"destroy_keeps_record_until_gone"),
        "{report:?}"
    );
}
