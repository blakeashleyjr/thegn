//! Request-encoding and stream-parsing mock for the Vercel Sandbox client — a
//! tiny no-dependency HTTP server (the `sprites_mock.rs` pattern) that records
//! every request and answers from a per-test router.
//!
//! The custody property is asserted on every test: the sentinel API token
//! appears only as `Authorization: Bearer <token>`, never in a request path,
//! query or body, and never in any error rendering.
//!
//! Touches no registry, HOME or XDG state (pure HTTP), so no `IsolatedState`.
//!
//! Run: `cargo test -p thegn-svc --test vercel_mock`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use thegn_core::vercel::api::{LogLine, Scope};
use thegn_svc::provider::EXEC_EXIT_STATUS_UNKNOWN;
use thegn_svc::vercel::VercelToken;
use thegn_svc::vercel::client::{ExtendOutcome, VercelClient};

const TOKEN: &str = "vcp_SENTINEL_token_9f8e7d6c";
const TEAM: &str = "team_sentinel1";
const PROJECT: &str = "prj_sentinel1";

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    /// Request target including the query string.
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Recorded {
    fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(p, _)| p)
    }

    fn query(&self) -> HashMap<String, String> {
        self.target
            .split_once('?')
            .map(|(_, q)| {
                url::form_urlencoded::parse(q.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn header(&self, name: &str) -> &str {
        self.headers.get(name).map_or("", String::as_str)
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

/// One canned answer.
struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    /// Omit `Content-Length` (close-delimited body).
    no_length: bool,
    /// Keep the connection open this long after writing (a live follow).
    hold: Duration,
}

impl Reply {
    fn new(status: &'static str, ctype: &str, body: impl Into<Vec<u8>>) -> Self {
        Reply {
            status,
            headers: vec![("Content-Type", ctype.to_string())],
            body: body.into(),
            no_length: false,
            hold: Duration::ZERO,
        }
    }

    fn json(status: &'static str, v: serde_json::Value) -> Self {
        Reply::new(status, "application/json", v.to_string())
    }

    fn ndjson(lines: &[serde_json::Value]) -> Self {
        let mut body = String::new();
        for line in lines {
            body.push_str(&line.to_string());
            body.push('\n');
        }
        Reply::new("200 OK", "application/x-ndjson", body)
    }
}

type Router = Arc<dyn Fn(&Recorded) -> Reply + Send + Sync>;

struct Mock {
    base: String,
    recorded: Arc<Mutex<Vec<Recorded>>>,
}

impl Mock {
    fn start(router: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        let router: Router = Arc::new(router);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let rec = rec.clone();
                let router = router.clone();
                thread::spawn(move || handle(stream, &rec, &router));
            }
        });
        Mock {
            base: format!("http://127.0.0.1:{port}"),
            recorded,
        }
    }

    fn client(&self) -> VercelClient {
        self.client_with_token(TOKEN)
    }

    fn client_with_token(&self, token: &str) -> VercelClient {
        VercelClient::new(
            &self.base,
            VercelToken::new(token.into()),
            Scope {
                team_id: TEAM.into(),
                project_id: PROJECT.into(),
            },
        )
        .expect("loopback http base is admitted")
    }

    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().unwrap().clone()
    }

    /// The custody property, for every recorded request and every error.
    fn assert_custody(&self, errors: &[&anyhow::Error]) {
        self.assert_custody_of(TOKEN, errors);
    }

    /// [`Mock::assert_custody`] for a client built with `token`.
    fn assert_custody_of(&self, token: &str, errors: &[&anyhow::Error]) {
        for r in self.requests() {
            assert!(!r.target.contains(token), "token in target: {}", r.target);
            assert!(
                !String::from_utf8_lossy(&r.body).contains(token),
                "token in body of {} {}",
                r.method,
                r.path()
            );
            for (name, value) in &r.headers {
                if name != "authorization" {
                    assert!(!value.contains(token), "token in header {name}");
                }
            }
            assert_eq!(r.header("authorization"), format!("Bearer {token}"));
            assert!(
                r.header("user-agent").starts_with("thegn/"),
                "user-agent: {}",
                r.header("user-agent")
            );
            assert_eq!(r.query().get("teamId").map(String::as_str), Some(TEAM));
        }
        for e in errors {
            for shown in [format!("{e}"), format!("{e:#}"), format!("{e:?}")] {
                assert!(!shown.contains(token), "token in error: {shown}");
                assert!(!shown.contains(&self.base), "url in error: {shown}");
                assert!(!shown.contains("teamId="), "query in error: {shown}");
            }
        }
    }
}

fn handle(mut stream: TcpStream, rec: &Mutex<Vec<Recorded>>, router: &Router) {
    let mut line = String::new();
    if read_line(&mut stream, &mut line).is_none() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
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
    }
    let content_length = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0usize);
    let mut body = vec![0u8; content_length];
    if content_length > 0 && stream.read_exact(&mut body).is_err() {
        return;
    }
    let req = Recorded {
        method,
        target,
        headers,
        body,
    };
    rec.lock().unwrap().push(req.clone());
    let reply = if req.method == "PATCH"
        && req.path().starts_with("/v2/sandboxes/")
        && req
            .json()
            .get("networkPolicy")
            .and_then(|p| p.get("type"))
            .is_some()
    {
        Reply::json(
            "400 Bad Request",
            serde_json::json!({"error": "legacy network policy"}),
        )
    } else if req.path().ends_with("/stdin") {
        let has_attached_command = rec.lock().unwrap().iter().any(|prior| {
            prior.method == "POST"
                && prior.path().ends_with("/cmd")
                && prior
                    .json()
                    .get("attachStdin")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        });
        if has_attached_command {
            (**router)(&req)
        } else {
            Reply::json(
                "400 Bad Request",
                serde_json::json!({"error": "stdin not attached"}),
            )
        }
    } else if req.path().ends_with("/cmd")
        && req.json().get("wait") == Some(&serde_json::Value::Bool(true))
        && req.json().get("logs") != Some(&serde_json::Value::Bool(true))
    {
        Reply::new("200 OK", "application/x-ndjson", Vec::new())
    } else {
        (**router)(&req)
    };
    let mut head = format!("HTTP/1.1 {}\r\n", reply.status);
    for (name, value) in &reply.headers {
        write!(head, "{name}: {value}\r\n").expect("writing to String cannot fail");
    }
    if !reply.no_length {
        write!(head, "Content-Length: {}\r\n", reply.body.len())
            .expect("writing to String cannot fail");
    }
    head.push_str("Connection: close\r\n\r\n");
    if stream.write_all(head.as_bytes()).is_err()
        || stream.write_all(&reply.body).is_err()
        || stream.flush().is_err()
    {
        return;
    }
    if !reply.hold.is_zero() {
        thread::sleep(reply.hold);
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

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().unwrap()
}

fn sandbox_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "sandbox": {
            "name": name,
            "status": "running",
            "currentSessionId": "sess_1",
            "vcpus": 2,
            "memory": 4096,
            "createdAt": 1_700_000_000_000_i64
        },
        "session": {
            "id": "sess_1",
            "status": "running"
        },
        "routes": [],
        "resumed": false
    })
}

fn not_found() -> Reply {
    Reply::json(
        "404 Not Found",
        serde_json::json!({"error": {"code": "not_found"}}),
    )
}

// --- control plane -----------------------------------------------------------

#[test]
fn create_get_list_encode_method_path_scope_and_auth() {
    let mock = Mock::start(|r| match (r.method.as_str(), r.path()) {
        ("POST", "/v3/sandboxes") => Reply::json("200 OK", sandbox_json("tg-a")),
        ("GET", "/v2/sandboxes/tg-a") => Reply::json("200 OK", sandbox_json("tg-a")),
        ("GET", "/v2/sandboxes/tg-gone") => not_found(),
        ("GET", "/v2/sandboxes") => Reply::json(
            "200 OK",
            serde_json::json!({
                "sandboxes": [sandbox_json("tg-a")["sandbox"].clone()],
                "pagination": { "next": "1700000000000" }
            }),
        ),
        _ => Reply::new("500 Internal Server Error", "text/plain", "unrouted"),
    });
    let client = mock.client();
    let rt = rt();

    let body = serde_json::json!({"projectId": PROJECT, "name": "tg-a", "persistent": true});
    let created = rt.block_on(client.create(&body)).expect("create");
    assert_eq!(created.name, "tg-a");
    let got = rt.block_on(client.get("tg-a", false)).expect("get");
    assert_eq!(got.map(|v| v.name).as_deref(), Some("tg-a"));
    rt.block_on(client.get("tg-a", true)).expect("get resume");
    assert!(
        rt.block_on(client.get("tg-gone", false))
            .expect("404")
            .is_none()
    );
    let (items, next) = rt
        .block_on(client.list_page(Some("1699"), "tg-"))
        .expect("list");
    assert_eq!(items.len(), 1);
    assert_eq!(next.as_deref(), Some("1700000000000"));

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 5);
    // create: POST, projectId travels in the body, JSON content type.
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path(), "/v3/sandboxes");
    assert!(
        reqs[0]
            .header("content-type")
            .starts_with("application/json")
    );
    assert_eq!(reqs[0].json(), body);
    // get: name in the path, team + project in the query, resume only on ask.
    assert_eq!(reqs[1].method, "GET");
    assert_eq!(reqs[1].path(), "/v2/sandboxes/tg-a");
    assert_eq!(
        reqs[1].query().get("projectId").map(String::as_str),
        Some(PROJECT)
    );
    assert!(!reqs[1].query().contains_key("resume"));
    assert_eq!(
        reqs[2].query().get("resume").map(String::as_str),
        Some("true")
    );
    // list: `project`, the prefix and the cursor.
    let q = reqs[4].query();
    assert_eq!(reqs[4].path(), "/v2/sandboxes");
    assert_eq!(q.get("project").map(String::as_str), Some(PROJECT));
    assert_eq!(q.get("namePrefix").map(String::as_str), Some("tg-"));
    assert_eq!(q.get("sortBy").map(String::as_str), Some("name"));
    assert_eq!(q.get("cursor").map(String::as_str), Some("1699"));
    mock.assert_custody(&[]);
}

#[test]
fn get_with_an_expired_snapshot_is_snapshot_expired() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let mock = Mock::start(move |_| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json(
                "410 Gone",
                serde_json::json!({"error": {"code": "snapshot_not_found"}}),
            )
        } else {
            Reply::json(
                "410 Gone",
                serde_json::json!({"error": {"code": "other_code"}}),
            )
        }
    });
    let client = mock.client();
    let rt = rt();
    let expired = rt.block_on(client.get("tg-a", true)).unwrap_err();
    let typed = expired
        .downcast_ref::<thegn_svc::vercel::client::SnapshotExpired>()
        .expect("expired snapshot error");
    assert_eq!(typed.name, "tg-a");
    assert!(format!("{expired:#}").contains("tg-a"));
    let plain = rt.block_on(client.get("tg-a", true)).unwrap_err();
    assert!(format!("{plain:#}").contains("HTTP 410"), "{plain:#}");
    mock.assert_custody(&[&expired, &plain]);
}

#[test]
fn list_without_a_prefix_does_not_send_sort_by() {
    let mock = Mock::start(|_| {
        Reply::json(
            "200 OK",
            serde_json::json!({"sandboxes": [], "pagination": {"next": null}}),
        )
    });
    rt().block_on(mock.client().list_page(None, ""))
        .expect("list");
    let query = mock.requests()[0].query();
    assert!(!query.contains_key("namePrefix"));
    assert!(!query.contains_key("sortBy"));
    mock.assert_custody(&[]);
}

#[test]
fn a_429_is_retried_then_succeeds() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let mock = Mock::start(move |_| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json(
                "429 Too Many Requests",
                serde_json::json!({"error": "slow down"}),
            )
        } else {
            Reply::json("200 OK", sandbox_json("tg-a"))
        }
    });
    let got = rt()
        .block_on(mock.client().get("tg-a", false))
        .expect("retried get");
    assert!(got.is_some());
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    mock.assert_custody(&[]);
}

#[test]
fn retries_stop_after_three_attempts_and_terminal_4xx_is_not_retried() {
    let mock = Mock::start(|r| {
        if r.method == "GET" {
            Reply::json("503 Service Unavailable", serde_json::json!({}))
        } else {
            Reply::json("400 Bad Request", serde_json::json!({"error": "bad name"}))
        }
    });
    let client = mock.client();
    let rt = rt();
    let get_err = rt.block_on(client.get("tg-a", false)).unwrap_err();
    assert!(format!("{get_err:#}").contains("HTTP 503"), "{get_err:#}");
    assert_eq!(mock.requests().len(), 3);
    let create_err = rt
        .block_on(client.create(&serde_json::json!({"name": "tg-a"})))
        .unwrap_err();
    assert!(
        format!("{create_err:#}").contains("HTTP 400"),
        "{create_err:#}"
    );
    assert!(
        format!("{create_err:#}").contains("bad name"),
        "{create_err:#}"
    );
    assert_eq!(mock.requests().len(), 4);
    mock.assert_custody(&[&get_err, &create_err]);
}

#[test]
fn delete_and_stop_treat_404_as_success_and_use_their_paths() {
    let mock = Mock::start(|_| not_found());
    let client = mock.client();
    let rt = rt();
    rt.block_on(client.delete("tg-a"))
        .expect("404 delete is ok");
    rt.block_on(client.stop("sess_1")).expect("404 stop is ok");
    rt.block_on(client.delete_snapshot("snap_1"))
        .expect("404 snapshot delete is ok");
    let reqs = mock.requests();
    assert_eq!(reqs[0].method, "DELETE");
    assert_eq!(reqs[0].path(), "/v2/sandboxes/tg-a");
    assert_eq!(
        reqs[0].query().get("projectId").map(String::as_str),
        Some(PROJECT)
    );
    assert_eq!(reqs[1].method, "POST");
    assert_eq!(reqs[1].path(), "/v2/sandboxes/sessions/sess_1/stop");
    assert!(!reqs[1].query().contains_key("projectId"));
    assert_eq!(reqs[2].method, "DELETE");
    assert_eq!(reqs[2].path(), "/v2/sandboxes/snapshots/snap_1");
    mock.assert_custody(&[]);
}

#[test]
fn stop_sends_a_json_body_or_the_mock_answers_415() {
    let mock = Mock::start(|r| {
        if r.header("content-type").starts_with("application/json") {
            Reply::json("200 OK", serde_json::json!({}))
        } else {
            Reply::json(
                "415 Unsupported Media Type",
                serde_json::json!({"error": {"message": "Unsupported Media Type"}}),
            )
        }
    });
    rt().block_on(mock.client().stop("sess_1"))
        .expect("stop sends a JSON content type");
    let reqs = mock.requests();
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path(), "/v2/sandboxes/sessions/sess_1/stop");
    assert_eq!(reqs[0].json(), serde_json::json!({}));
    mock.assert_custody(&[]);
}

#[test]
fn stop_and_kill_treat_410_as_already_stopped() {
    let mock = Mock::start(|_| {
        Reply::json(
            "410 Gone",
            serde_json::json!({"error": {"code": "sandbox_stopped"}}),
        )
    });
    let client = mock.client();
    let rt = rt();
    rt.block_on(client.stop("sess_1")).expect("stopped session");
    rt.block_on(client.kill("sess_1", "cmd_1", 9))
        .expect("stopped command");
    mock.assert_custody(&[]);
}

#[test]
fn delete_still_refuses_a_410() {
    let mock = Mock::start(|_| {
        Reply::json(
            "410 Gone",
            serde_json::json!({"error": {"code": "sandbox_stopped"}}),
        )
    });
    let err = rt().block_on(mock.client().delete("tg-a")).unwrap_err();
    assert!(format!("{err:#}").contains("HTTP 410"), "{err:#}");
    mock.assert_custody(&[&err]);
}

#[test]
fn an_error_code_that_echoes_the_token_is_never_shown() {
    let mock = Mock::start(|_| {
        Reply::json(
            "422 Unprocessable Entity",
            serde_json::json!({"error": {"code": TOKEN}}),
        )
    });
    let err = rt()
        .block_on(mock.client().run_detached("sess_1", &serde_json::json!({})))
        .unwrap_err();
    assert!(
        err.downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
            .is_none()
    );
    mock.assert_custody(&[&err]);
}

#[test]
fn an_error_body_echoing_the_token_is_scrubbed_and_capped() {
    let mock = Mock::start(|r| {
        let echo = format!(
            "denied for {}\r\n{}",
            r.header("authorization"),
            "z".repeat(1000)
        );
        Reply::new("500 Internal Server Error", "text/plain", echo)
    });
    let err = rt().block_on(mock.client().stop("sess_1")).unwrap_err();
    let shown = format!("{err:#}");
    assert!(shown.contains("HTTP 500"), "{shown}");
    assert!(shown.contains("<redacted>"), "{shown}");
    // "vercel: stop session returned HTTP 500: " + at most 200 chars.
    let snippet = shown.split_once("HTTP 500: ").map_or("", |(_, s)| s);
    assert!(snippet.chars().count() <= 200, "{}", snippet.len());
    assert!(
        !snippet.contains('\n') && !snippet.contains('\r'),
        "{snippet}"
    );
    mock.assert_custody(&[&err]);
}

#[test]
fn a_redirect_is_refused_not_followed() {
    let elsewhere = Mock::start(|_| Reply::json("200 OK", sandbox_json("tg-a")));
    let target = format!("{}/v2/sandboxes/tg-a", elsewhere.base);
    let mock = Mock::start(move |_| {
        let mut reply = Reply::new("302 Found", "text/plain", "");
        reply.headers.push(("Location", target.clone()));
        reply
    });
    let err = rt().block_on(mock.client().get("tg-a", false)).unwrap_err();
    assert!(format!("{err:#}").contains("redirect refused"), "{err:#}");
    assert_eq!(mock.requests().len(), 1);
    assert!(elsewhere.requests().is_empty(), "the redirect was followed");
    mock.assert_custody(&[&err]);
}

#[test]
fn extend_reports_extended_or_the_plan_ceiling_and_missing_sessions() {
    let mock = Mock::start(|r| {
        if r.target.contains("sess_full") {
            Reply::json(
                "400 Bad Request",
                serde_json::json!({"error": "max duration"}),
            )
        } else if r.target.contains("sess_422") {
            Reply::json(
                "422 Unprocessable Entity",
                serde_json::json!({"error": "max duration"}),
            )
        } else if r.target.contains("sess_404") {
            Reply::json("404 Not Found", serde_json::json!({"error": "gone"}))
        } else if r.target.contains("sess_410") {
            Reply::json("410 Gone", serde_json::json!({"error": "gone"}))
        } else {
            Reply::json("200 OK", serde_json::json!({}))
        }
    });
    let client = mock.client();
    let rt = rt();
    assert_eq!(
        rt.block_on(client.extend("sess_1", 900_000)).unwrap(),
        ExtendOutcome::Extended
    );
    assert_eq!(
        rt.block_on(client.extend("sess_full", 900_000)).unwrap(),
        ExtendOutcome::AtCeiling
    );
    assert_eq!(
        rt.block_on(client.extend("sess_422", 900_000)).unwrap(),
        ExtendOutcome::AtCeiling
    );
    for session in ["sess_404", "sess_410"] {
        let err = rt.block_on(client.extend(session, 900_000)).unwrap_err();
        assert!(
            format!("{err:#}").contains("session no longer exists"),
            "{err:#}"
        );
        assert!(
            err.downcast_ref::<thegn_svc::vercel::client::SessionGone>()
                .is_some(),
            "{err:#}"
        );
    }
    let reqs = mock.requests();
    assert_eq!(
        reqs[0].path(),
        "/v2/sandboxes/sessions/sess_1/extend-timeout"
    );
    assert_eq!(reqs[0].json(), serde_json::json!({"duration": 900_000}));
    assert!(reqs[1].query().contains_key("teamId"));
    assert!(!reqs[1].query().contains_key("projectId"));
    mock.assert_custody(&[]);
}

#[test]
fn invalid_names_and_ids_never_reach_the_wire() {
    let mock = Mock::start(|_| Reply::json("200 OK", serde_json::json!({})));
    let client = mock.client();
    let rt = rt();
    let errs = [
        rt.block_on(client.get("../etc", false)).unwrap_err(),
        rt.block_on(client.delete("Bad Name")).unwrap_err(),
        rt.block_on(client.stop("sess/../x")).unwrap_err(),
        rt.block_on(client.kill("sess_1", "cmd?x=1", 15))
            .unwrap_err(),
    ];
    assert!(mock.requests().is_empty());
    let refs: Vec<&anyhow::Error> = errs.iter().collect();
    mock.assert_custody(&refs);
}

#[test]
fn a_request_body_carrying_the_token_is_refused() {
    let mock = Mock::start(|_| Reply::json("200 OK", sandbox_json("tg-a")));
    let err = rt()
        .block_on(
            mock.client()
                .create(&serde_json::json!({"env": {"LEAK": TOKEN}})),
        )
        .unwrap_err();
    assert!(mock.requests().is_empty());
    mock.assert_custody(&[&err]);
}

// --- commands ------------------------------------------------------------------

fn out(s: &str) -> serde_json::Value {
    serde_json::json!({"stream": "stdout", "data": s})
}

fn err_line(s: &str) -> serde_json::Value {
    serde_json::json!({"stream": "stderr", "data": s})
}

fn exit(code: i32) -> serde_json::Value {
    serde_json::json!({"command": {"id": "cmd_1", "exitCode": code}})
}

fn started() -> serde_json::Value {
    serde_json::json!({"command": {"id": "cmd_1", "exitCode": null}})
}

#[test]
fn run_wait_relays_stdout_drops_stderr_and_reports_the_exit() {
    let mock = Mock::start(|_| {
        Reply::ndjson(&[
            started(),
            out("one\n"),
            err_line("warn: noisy\n"),
            out("two\n"),
            err_line("more noise\n"),
            exit(3),
        ])
    });
    let body = serde_json::json!({"command": "git", "args": ["status"], "wait": false});
    let (code, output) = rt()
        .block_on(mock.client().run_wait("sess_1", &body, 1024))
        .expect("run_wait");
    assert_eq!(code, 3);
    assert_eq!(output, b"one\ntwo\n");
    let reqs = mock.requests();
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path(), "/v2/sandboxes/sessions/sess_1/cmd");
    assert!(
        reqs[0]
            .header("content-type")
            .starts_with("application/json")
    );
    // `wait: true` is forced whatever the caller sent.
    assert_eq!(reqs[0].json()["wait"], serde_json::json!(true));
    assert_eq!(reqs[0].json()["logs"], serde_json::json!(true));
    assert!(!reqs[0].query().contains_key("projectId"));
    assert_eq!(reqs[0].json()["command"], serde_json::json!("git"));
    mock.assert_custody(&[]);
}

#[test]
fn run_wait_with_stderr_keeps_the_sanitized_tail() {
    let mock = Mock::start(|_| Reply::ndjson(&[err_line("\u{1b}[31mred\n"), exit(3)]));
    let (code, output, stderr) = rt()
        .block_on(
            mock.client()
                .run_wait_with_stderr("sess_1", &serde_json::json!({}), 1024),
        )
        .expect("run_wait_with_stderr");
    assert_eq!(code, 3);
    assert!(output.is_empty());
    assert_eq!(stderr, "[31mred\n");
    mock.assert_custody(&[]);
}

#[test]
fn run_wait_with_stderr_caps_at_8_kib() {
    let first = "a".repeat(8 * 1024);
    let mock = Mock::start(move |_| Reply::ndjson(&[err_line(&first), err_line("bc"), exit(1)]));
    let (_, _, stderr) = rt()
        .block_on(
            mock.client()
                .run_wait_with_stderr("sess_1", &serde_json::json!({}), 1024),
        )
        .expect("run_wait_with_stderr");
    assert_eq!(stderr.len(), 8 * 1024);
    assert_eq!(stderr, format!("{}bc", "a".repeat(8 * 1024 - 2)));
    mock.assert_custody(&[]);
}

#[test]
fn run_wait_keeps_only_the_last_out_cap_bytes() {
    let mock = Mock::start(|_| Reply::ndjson(&[out("abcdef"), out("ghij"), exit(0)]));
    let (code, output) = rt()
        .block_on(mock.client().run_wait("sess_1", &serde_json::json!({}), 5))
        .expect("run_wait");
    assert_eq!(code, 0);
    assert_eq!(output, b"fghij");
    mock.assert_custody(&[]);
}

#[test]
fn run_wait_without_a_final_exit_is_an_error() {
    let mock = Mock::start(|_| {
        let mut reply = Reply::ndjson(&[started(), out("partial")]);
        reply.no_length = true;
        reply
    });
    let err = rt()
        .block_on(mock.client().run_wait("sess_1", &serde_json::json!({}), 64))
        .unwrap_err();
    let shown = format!("{err:#}");
    assert!(shown.contains("without an exit status"), "{shown}");
    mock.assert_custody(&[&err]);
}

#[test]
fn run_wait_surfaces_an_error_line_sanitized() {
    let mock = Mock::start(|_| {
        Reply::ndjson(&[
            out("before"),
            serde_json::json!({"stream": "error", "data": {
                "code": "sandbox_stream_closed",
                "message": format!("stream closed; token {TOKEN}\n{}", "q".repeat(400))
            }}),
            exit(0),
        ])
    });
    let err = rt()
        .block_on(mock.client().run_wait("sess_1", &serde_json::json!({}), 64))
        .unwrap_err();
    let shown = format!("{err:#}");
    assert!(shown.contains("sandbox_stream_closed"), "{shown}");
    assert!(!shown.contains('\n'), "{shown}");
    assert!(shown.len() < 400, "{}", shown.len());
    mock.assert_custody(&[&err]);
}

#[test]
fn run_wait_refuses_an_over_long_line() {
    let mock = Mock::start(|_| {
        let huge = "x".repeat(1024 * 1024 + 16);
        Reply::ndjson(&[out("ok"), out(&huge), exit(0)])
    });
    let err = rt()
        .block_on(mock.client().run_wait("sess_1", &serde_json::json!({}), 64))
        .unwrap_err();
    assert!(format!("{err:#}").contains("too long"), "{err:#}");
    mock.assert_custody(&[&err]);
}

#[test]
fn run_detached_returns_the_command_id() {
    let mock =
        Mock::start(|_| Reply::json("200 OK", serde_json::json!({"command": {"id": "cmd_42"}})));
    let id = rt()
        .block_on(mock.client().run_detached(
            "sess_1",
            &serde_json::json!({"command": "sh", "attachStdin": true}),
        ))
        .expect("detached");
    assert_eq!(id, "cmd_42");
    assert_eq!(mock.requests()[0].json()["wait"], serde_json::json!(false));
    assert_eq!(
        mock.requests()[0].json()["attachStdin"],
        serde_json::json!(true)
    );
    mock.assert_custody(&[]);
}

#[test]
fn run_wait_on_a_stopped_session_is_session_stopped() {
    let mock = Mock::start(|r| {
        let (status, code) = if r.target.contains("sess_gone") {
            ("410 Gone", "sandbox_stopped")
        } else if r.target.contains("sess_stopping") {
            ("422 Unprocessable Entity", "sandbox_stopping")
        } else if r.target.contains("sess_snapshotting") {
            ("422 Unprocessable Entity", "sandbox_snapshotting")
        } else {
            ("422 Unprocessable Entity", "bad_request")
        };
        Reply::json(status, serde_json::json!({"error": {"code": code}}))
    });
    let client = mock.client();
    let rt = rt();
    let mut errors = Vec::new();
    for (session, status, stopped) in [
        ("sess_gone", 410, true),
        ("sess_stopping", 422, true),
        ("sess_snapshotting", 422, true),
        ("sess_bad", 422, false),
    ] {
        let err = rt
            .block_on(client.run_wait(session, &serde_json::json!({"command": "true"}), 64))
            .unwrap_err();
        if stopped {
            let typed = err
                .downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                .expect("stopped session error");
            assert_eq!(typed.status, status);
        } else {
            assert!(
                err.downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                    .is_none()
            );
            assert!(format!("{err:#}").contains("HTTP 422"), "{err:#}");
        }
        errors.push(err);
    }
    let refs: Vec<&anyhow::Error> = errors.iter().collect();
    mock.assert_custody(&refs);
}

#[test]
fn run_detached_on_a_stopped_session_is_session_stopped() {
    let mock = Mock::start(|r| {
        let (status, code) = if r.target.contains("sess_gone") {
            ("410 Gone", "sandbox_stopped")
        } else if r.target.contains("sess_stopping") {
            ("422 Unprocessable Entity", "sandbox_stopping")
        } else if r.target.contains("sess_snapshotting") {
            ("422 Unprocessable Entity", "sandbox_snapshotting")
        } else {
            ("422 Unprocessable Entity", "bad_request")
        };
        Reply::json(status, serde_json::json!({"error": {"code": code}}))
    });
    let client = mock.client();
    let rt = rt();
    let mut errors = Vec::new();
    for (session, status, stopped) in [
        ("sess_gone", 410, true),
        ("sess_stopping", 422, true),
        ("sess_snapshotting", 422, true),
        ("sess_bad", 422, false),
    ] {
        let err = rt
            .block_on(client.run_detached(session, &serde_json::json!({"command": "true"})))
            .unwrap_err();
        if stopped {
            let typed = err
                .downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                .expect("stopped session error");
            assert_eq!(typed.status, status);
        } else {
            assert!(
                err.downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                    .is_none()
            );
            assert!(format!("{err:#}").contains("HTTP 422"), "{err:#}");
        }
        errors.push(err);
    }
    let refs: Vec<&anyhow::Error> = errors.iter().collect();
    mock.assert_custody(&refs);
}

#[test]
fn logs_streams_lines_then_ends_on_eof() {
    let mock = Mock::start(|_| {
        let mut reply = Reply::ndjson(&[out("a"), err_line("b"), exit(0)]);
        reply.no_length = true;
        reply
    });
    let rt = rt();
    let lines = rt.block_on(async {
        let mut stream = mock.client().logs("sess_1", "cmd_1").await.expect("logs");
        let mut lines = Vec::new();
        while let Some(line) = stream.receiver_mut().recv().await {
            lines.push(line);
        }
        lines
    });
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0], LogLine::Stdout(b"a".to_vec()));
    assert_eq!(lines[1], LogLine::Stderr(b"b".to_vec()));
    let reqs = mock.requests();
    assert_eq!(reqs[0].method, "GET");
    assert_eq!(
        reqs[0].path(),
        "/v2/sandboxes/sessions/sess_1/cmd/cmd_1/logs"
    );
    mock.assert_custody(&[]);
}

#[test]
fn dropping_a_live_log_stream_aborts_its_reader() {
    let mock = Mock::start(|_| {
        let mut reply = Reply::ndjson(&[out("first")]);
        reply.no_length = true;
        reply.hold = Duration::from_secs(5);
        reply
    });
    let rt = rt();
    rt.block_on(async {
        let mut stream = mock.client().logs("sess_1", "cmd_1").await.expect("logs");
        let first = stream.receiver_mut().recv().await.expect("first line");
        assert_eq!(first, LogLine::Stdout(b"first".to_vec()));
        // The body is still open: the reader is parked on it, not finished.
        assert!(!stream.reader_finished());
        drop(stream);
    });
    mock.assert_custody(&[]);
}

#[test]
fn stdin_and_kill_encode_their_requests() {
    let mock = Mock::start(|r| {
        if r.path().ends_with("/cmd") {
            Reply::json("200 OK", serde_json::json!({"command": {"id": "cmd_1"}}))
        } else {
            Reply::json("200 OK", serde_json::json!({}))
        }
    });
    let client = mock.client();
    let rt = rt();
    rt.block_on(client.run_detached(
        "sess_1",
        &serde_json::json!({"command": "sh", "attachStdin": true}),
    ))
    .expect("start stdin command");
    rt.block_on(client.stdin("sess_1", "cmd_1", b"hello\n", true))
        .expect("stdin");
    rt.block_on(client.kill("sess_1", "cmd_1", 15))
        .expect("kill");
    let reqs = mock.requests();
    assert_eq!(
        reqs[1].path(),
        "/v2/sandboxes/sessions/sess_1/cmd/cmd_1/stdin"
    );
    assert_eq!(reqs[1].json()["data"], serde_json::json!("aGVsbG8K"));
    assert_eq!(reqs[1].json()["close"], serde_json::json!(true));
    assert!(!reqs[1].query().contains_key("close"));
    assert_eq!(
        reqs[2].path(),
        "/v2/sandboxes/sessions/sess_1/cmd/cmd_1/kill"
    );
    assert_eq!(reqs[2].json(), serde_json::json!({"signal": 15}));
    mock.assert_custody(&[]);
}

// --- files ---------------------------------------------------------------------

#[test]
fn fs_write_targz_sends_gzip_and_the_validated_cwd() {
    let mock = Mock::start(|_| Reply::json("200 OK", serde_json::json!({})));
    let client = mock.client();
    let rt = rt();
    let payload = vec![0x1f, 0x8b, 8, 0, 1, 2, 3];
    rt.block_on(client.fs_write_targz("sess_1", "/vercel/sandbox", payload.clone()))
        .expect("write");
    let relative = rt
        .block_on(client.fs_write_targz("sess_1", "relative/dir", payload.clone()))
        .unwrap_err();
    let nul = rt
        .block_on(client.fs_write_targz("sess_1", "/a\0b", payload.clone()))
        .unwrap_err();
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1, "refused cwds never reach the wire");
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[0].path(), "/v2/sandboxes/sessions/sess_1/fs/write");
    assert_eq!(reqs[0].header("content-type"), "application/gzip");
    assert_eq!(reqs[0].header("x-cwd"), "/vercel/sandbox");
    assert_eq!(reqs[0].body, payload);
    mock.assert_custody(&[&relative, &nul]);
}

#[test]
fn fs_read_returns_bytes_none_on_404_and_fails_over_the_cap() {
    let mock = Mock::start(|r| {
        let path = r.json()["path"].as_str().unwrap_or("").to_string();
        match path.as_str() {
            "/small" => Reply::new("200 OK", "application/octet-stream", "tiny"),
            "/missing" => not_found(),
            // Declared length over the cap.
            "/declared" => Reply::new("200 OK", "application/octet-stream", vec![b'x'; 64]),
            // No declared length: the cap must hold while streaming.
            _ => {
                let mut reply = Reply::new("200 OK", "application/octet-stream", vec![b'y'; 64]);
                reply.no_length = true;
                reply
            }
        }
    });
    let client = mock.client();
    let rt = rt();
    assert_eq!(
        rt.block_on(client.fs_read("sess_1", "/small", 16)).unwrap(),
        Some(b"tiny".to_vec())
    );
    assert_eq!(
        rt.block_on(client.fs_read("sess_1", "/missing", 16))
            .unwrap(),
        None
    );
    let declared = rt
        .block_on(client.fs_read("sess_1", "/declared", 16))
        .unwrap_err();
    let streamed = rt
        .block_on(client.fs_read("sess_1", "/streamed", 16))
        .unwrap_err();
    for e in [&declared, &streamed] {
        assert!(format!("{e:#}").contains("cap"), "{e:#}");
    }
    let reqs = mock.requests();
    assert_eq!(reqs[0].path(), "/v2/sandboxes/sessions/sess_1/fs/read");
    assert!(
        reqs[0]
            .header("content-type")
            .starts_with("application/json")
    );
    mock.assert_custody(&[&declared, &streamed]);
}

#[test]
fn fs_calls_on_a_stopped_session_are_session_stopped() {
    let mock = Mock::start(|r| {
        if r.path().ends_with("/fs/read") && r.json()["path"] == "/missing" {
            return not_found();
        }
        let code = if r.target.contains("sess_gone") {
            "sandbox_stopped"
        } else if r.target.contains("sess_stopping") {
            "sandbox_stopping"
        } else if r.target.contains("sess_bad") {
            "bad_request"
        } else if r.path().ends_with("/fs/read") {
            return Reply::new("200 OK", "application/octet-stream", "ok");
        } else {
            return Reply::json("200 OK", serde_json::json!({}));
        };
        let status = if r.target.contains("sess_gone") {
            "410 Gone"
        } else {
            "422 Unprocessable Entity"
        };
        Reply::json(status, serde_json::json!({"error": {"code": code}}))
    });
    let client = mock.client();
    let rt = rt();
    let mut errors = Vec::new();
    for session in ["sess_gone", "sess_stopping", "sess_bad"] {
        let write = rt
            .block_on(client.fs_write_targz(session, "/tmp", vec![1, 2, 3]))
            .unwrap_err();
        let read = rt
            .block_on(client.fs_read(session, "/file", 16))
            .unwrap_err();
        let mkdir = rt.block_on(client.fs_mkdir(session, "/tmp")).unwrap_err();
        errors.extend([write, read, mkdir]);
    }
    for (index, error) in errors.iter().enumerate() {
        let stopped = index < 6;
        if stopped {
            let typed = error
                .downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                .expect("stopped session error");
            assert_eq!(typed.status, if index < 3 { 410 } else { 422 });
        } else {
            assert!(
                error
                    .downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                    .is_none()
            );
        }
    }
    let missing = rt
        .block_on(client.fs_read("sess_missing", "/missing", 16))
        .expect("404 is a missing file");
    assert_eq!(missing, None);
    let refs: Vec<&anyhow::Error> = errors.iter().collect();
    mock.assert_custody(&refs);
}

// --- interactive -----------------------------------------------------------------

#[test]
fn interactive_on_a_stopped_session_is_session_stopped() {
    let mock = Mock::start(|r| {
        let (status, code) = if r.target.contains("sess_gone") {
            ("410 Gone", "sandbox_stopped")
        } else if r.target.contains("sess_stopping") {
            ("422 Unprocessable Entity", "sandbox_stopping")
        } else if r.target.contains("sess_bad") {
            ("422 Unprocessable Entity", "bad_request")
        } else {
            return Reply::json(
                "200 OK",
                serde_json::json!({"url": "wss://x.vercel.run/pty", "token": "ticket"}),
            );
        };
        Reply::json(status, serde_json::json!({"error": {"code": code}}))
    });
    let client = mock.client();
    let rt = rt();
    let mut errors = Vec::new();
    for session in ["sess_gone", "sess_stopping", "sess_bad"] {
        errors.push(rt.block_on(client.interactive(session)).unwrap_err());
    }
    for (index, error) in errors.iter().enumerate() {
        if index < 2 {
            let typed = error
                .downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                .expect("stopped session error");
            assert_eq!(typed.status, if index == 0 { 410 } else { 422 });
        } else {
            assert!(
                error
                    .downcast_ref::<thegn_svc::vercel::client::SessionStopped>()
                    .is_none()
            );
        }
    }
    let refs: Vec<&anyhow::Error> = errors.iter().collect();
    mock.assert_custody(&refs);
}

#[test]
fn interactive_ticket_is_wss_and_its_token_is_redacted() {
    let mock = Mock::start(|r| {
        let url = if r.target.contains("sess_https") {
            "https://sbx-1.vercel.run/interactive"
        } else {
            "wss://sbx-1.vercel.run/interactive"
        };
        Reply::json(
            "200 OK",
            serde_json::json!({"url": url, "token": "ticket_secret_123"}),
        )
    });
    let client = mock.client();
    let rt = rt();
    let ticket = rt.block_on(client.interactive("sess_1")).expect("ticket");
    assert_eq!(ticket.url.scheme(), "wss");
    assert_eq!(ticket.url.host_str(), Some("sbx-1.vercel.run"));
    assert_eq!(ticket.token.as_str(), "ticket_secret_123");
    assert!(!format!("{ticket:?}").contains("ticket_secret_123"));
    let rewritten = rt
        .block_on(client.interactive("sess_https"))
        .expect("https");
    assert_eq!(rewritten.url.scheme(), "wss");
    assert_eq!(mock.requests()[0].method, "POST");
    assert_eq!(
        mock.requests()[0].path(),
        "/v2/sandboxes/sessions/sess_1/interactive"
    );
    assert_eq!(mock.requests()[0].json(), serde_json::json!({}));
    assert!(
        mock.requests()[0]
            .header("content-type")
            .starts_with("application/json")
    );
    mock.assert_custody(&[]);
}

#[test]
fn interactive_refuses_loopback_ip_localhost_userinfo_and_plain_ws() {
    let urls = Arc::new(Mutex::new(vec![
        "ws://127.0.0.1:8080/pty",
        "wss://127.0.0.1/pty",
        "wss://localhost/pty",
        "wss://user:pw@sbx-1.vercel.run/pty",
        "ws://sbx-1.vercel.run/pty",
    ]));
    let queue = urls.clone();
    let mock = Mock::start(move |_| {
        let url = queue.lock().unwrap().remove(0);
        Reply::json("200 OK", serde_json::json!({"url": url, "token": "t_1"}))
    });
    let client = mock.client();
    let rt = rt();
    let mut errs = Vec::new();
    for _ in 0..5 {
        let err = rt.block_on(client.interactive("sess_1")).unwrap_err();
        let shown = format!("{err:#}");
        assert!(!shown.contains("127.0.0.1"), "url echoed: {shown}");
        assert!(!shown.contains("pw@"), "userinfo echoed: {shown}");
        errs.push(err);
    }
    assert!(urls.lock().unwrap().is_empty());
    let refs: Vec<&anyhow::Error> = errs.iter().collect();
    mock.assert_custody(&refs);
}

#[test]
fn interactive_refuses_a_ticket_carrying_the_api_token() {
    let mock = Mock::start(|_| {
        Reply::json(
            "200 OK",
            serde_json::json!({"url": format!("wss://sbx-1.vercel.run/pty?k={TOKEN}"), "token": "t"}),
        )
    });
    let err = rt()
        .block_on(mock.client().interactive("sess_1"))
        .unwrap_err();
    mock.assert_custody(&[&err]);
}

#[test]
fn snapshots_for_lists_ids_scoped_by_name() {
    let mock = Mock::start(|r| {
        let query = r.query();
        let page = query.get("cursor").map(String::as_str);
        match page {
            None => Reply::json(
                "200 OK",
                serde_json::json!({
                    "snapshots": [{"id": "snap_1"}, {"id": "bad/id"}],
                    "pagination": {"next": "page-2"}
                }),
            ),
            Some("page-2") => Reply::json(
                "200 OK",
                serde_json::json!({
                    "snapshots": [{"id": "snap_2"}],
                    "pagination": {"next": "page-3"}
                }),
            ),
            Some("page-3") => Reply::json(
                "200 OK",
                serde_json::json!({
                    "snapshots": [{"id": "snap_3"}],
                    "pagination": {"next": null}
                }),
            ),
            _ => Reply::json("500 Internal Server Error", serde_json::json!({})),
        }
    });
    let (ids, complete) = rt()
        .block_on(mock.client().snapshots_for("tg-a"))
        .expect("snapshots");
    assert_eq!(
        ids,
        vec![
            "snap_1".to_string(),
            "snap_2".to_string(),
            "snap_3".to_string()
        ]
    );
    assert!(!complete);
    let reqs = mock.requests();
    assert_eq!(reqs[0].path(), "/v2/sandboxes/snapshots");
    assert_eq!(
        reqs[0].query().get("name").map(String::as_str),
        Some("tg-a")
    );
    assert_eq!(
        reqs[0].query().get("project").map(String::as_str),
        Some(PROJECT)
    );
    assert_eq!(reqs[0].query().get("limit").map(String::as_str), Some("50"));
    assert_eq!(reqs.len(), 3);
    assert_eq!(
        reqs[1].query().get("cursor").map(String::as_str),
        Some("page-2")
    );
    assert_eq!(
        reqs[2].query().get("cursor").map(String::as_str),
        Some("page-3")
    );
    mock.assert_custody(&[]);
}

#[test]
fn snapshots_for_is_complete_when_every_id_is_valid_and_the_last_page_ends() {
    let mock = Mock::start(|r| {
        if r.query().contains_key("cursor") {
            Reply::json(
                "200 OK",
                serde_json::json!({
                    "snapshots": [{"id": "snap_2"}],
                    "pagination": {"next": null}
                }),
            )
        } else {
            Reply::json(
                "200 OK",
                serde_json::json!({
                    "snapshots": [{"id": "snap_1"}],
                    "pagination": {"next": "page-2"}
                }),
            )
        }
    });
    let (ids, complete) = rt()
        .block_on(mock.client().snapshots_for("tg-a"))
        .expect("snapshots");
    assert_eq!(ids, vec!["snap_1", "snap_2"]);
    assert!(complete);
    assert_eq!(mock.requests().len(), 2);
    mock.assert_custody(&[]);
}

#[test]
fn snapshot_listing_with_a_repeated_cursor_is_incomplete() {
    let mock = Mock::start(|_| {
        Reply::json(
            "200 OK",
            serde_json::json!({
                "snapshots": [{"id": "snap_1"}],
                "pagination": {"next": "same"}
            }),
        )
    });
    let (ids, complete) = rt()
        .block_on(mock.client().snapshots_for("tg-a"))
        .expect("snapshots");
    assert_eq!(ids, vec!["snap_1", "snap_1"]);
    assert!(!complete);
    assert_eq!(mock.requests().len(), 2);
    mock.assert_custody(&[]);
}

#[test]
fn set_network_policy_patches_the_sandbox() {
    let mock = Mock::start(|r| match (r.method.as_str(), r.path()) {
        ("PATCH", "/v2/sandboxes/tg-a") => Reply::json("200 OK", serde_json::json!({})),
        ("GET", "/v2/sandboxes/tg-a") => Reply::json("200 OK", sandbox_json("tg-a")),
        ("POST", "/v2/sandboxes/sessions/sess_1/network-policy") => {
            Reply::json("200 OK", serde_json::json!({}))
        }
        _ => Reply::json("500 Internal Server Error", serde_json::json!({})),
    });
    let policy = serde_json::json!({"mode": "custom", "allowedDomains": ["example.com"]});
    rt().block_on(mock.client().set_network_policy("tg-a", &policy))
        .expect("set network policy");
    let reqs = mock.requests();
    assert_eq!(reqs[0].method, "PATCH");
    assert_eq!(reqs[0].path(), "/v2/sandboxes/tg-a");
    assert_eq!(
        reqs[0].query().get("projectId").map(String::as_str),
        Some(PROJECT)
    );
    assert_eq!(reqs[0].json()["networkPolicy"], policy);
    assert_eq!(reqs[1].method, "GET");
    assert_eq!(reqs[2].method, "POST");
    assert_eq!(reqs[2].json(), policy);
    assert!(!reqs[2].query().contains_key("projectId"));
    mock.assert_custody(&[]);
}

#[test]
fn set_network_policy_patches_without_a_session_post_when_stopped() {
    let mock = Mock::start(|r| match (r.method.as_str(), r.path()) {
        ("PATCH", "/v2/sandboxes/tg-a") => Reply::json("200 OK", serde_json::json!({})),
        ("GET", "/v2/sandboxes/tg-a") => Reply::json(
            "200 OK",
            serde_json::json!({"sandbox": {"name": "tg-a", "status": "stopped"}}),
        ),
        _ => Reply::json("500 Internal Server Error", serde_json::json!({})),
    });
    let policy = serde_json::json!({"mode": "allow-all"});
    rt().block_on(mock.client().set_network_policy("tg-a", &policy))
        .expect("set network policy");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].method, "PATCH");
    assert_eq!(reqs[1].method, "GET");
    mock.assert_custody(&[]);
}

#[test]
fn set_network_policy_patches_and_posts_for_a_pending_session() {
    let mock = Mock::start(|r| match (r.method.as_str(), r.path()) {
        ("PATCH", "/v2/sandboxes/tg-a") => Reply::json("200 OK", serde_json::json!({})),
        ("GET", "/v2/sandboxes/tg-a") => Reply::json(
            "200 OK",
            serde_json::json!({
                "sandbox": {
                    "name": "tg-a",
                    "status": "pending",
                    "currentSessionId": "sess_1"
                },
                "session": {"id": "sess_1", "status": "pending"}
            }),
        ),
        ("POST", "/v2/sandboxes/sessions/sess_1/network-policy") => {
            Reply::json("200 OK", serde_json::json!({}))
        }
        _ => Reply::json("500 Internal Server Error", serde_json::json!({})),
    });
    let policy = serde_json::json!({"mode": "allow-all"});
    rt().block_on(mock.client().set_network_policy("tg-a", &policy))
        .expect("set network policy");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(reqs[0].method, "PATCH");
    assert_eq!(reqs[1].method, "GET");
    assert_eq!(reqs[2].method, "POST");
    assert_eq!(reqs[2].json(), policy);
    mock.assert_custody(&[]);
}

#[test]
fn set_network_policy_returns_a_session_failure() {
    let mock = Mock::start(|r| match (r.method.as_str(), r.path()) {
        ("PATCH", "/v2/sandboxes/tg-a") => Reply::json("200 OK", serde_json::json!({})),
        ("GET", "/v2/sandboxes/tg-a") => Reply::json("200 OK", sandbox_json("tg-a")),
        ("POST", "/v2/sandboxes/sessions/sess_1/network-policy") => Reply::json(
            "500 Internal Server Error",
            serde_json::json!({"error": "failed"}),
        ),
        _ => Reply::json("500 Internal Server Error", serde_json::json!({})),
    });
    let policy = serde_json::json!({"mode": "allow-all"});
    let err = rt()
        .block_on(mock.client().set_network_policy("tg-a", &policy))
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("set session network policy"),
        "{err:#}"
    );
    mock.assert_custody(&[&err]);
}

#[test]
fn client_debug_never_shows_the_token() {
    let mock = Mock::start(|_| Reply::json("200 OK", serde_json::json!({})));
    let shown = format!("{:?}", mock.client());
    assert!(!shown.contains(TOKEN), "{shown}");
}

// --- wait_command ------------------------------------------------------------

fn command_json(exit: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"command": {"id": "cmd_1", "exitCode": exit}})
}

#[test]
fn wait_command_polls_until_the_exit_code_arrives() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let mock = Mock::start(move |_| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json("200 OK", command_json(serde_json::Value::Null))
        } else {
            Reply::json("200 OK", command_json(serde_json::json!(3)))
        }
    });
    let code = rt()
        .block_on(
            mock.client()
                .wait_command("sess_1", "cmd_1", Duration::from_secs(10)),
        )
        .expect("wait");
    assert_eq!(code, Some(3));
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    for r in &reqs {
        assert_eq!(r.method, "GET");
        assert_eq!(r.path(), "/v2/sandboxes/sessions/sess_1/cmd/cmd_1");
        assert_eq!(r.query().get("wait").map(String::as_str), Some("true"));
        assert!(!r.query().contains_key("projectId"));
    }
    mock.assert_custody(&[]);
}

#[test]
fn wait_command_gives_up_at_the_hard_deadline() {
    let mock = Mock::start(|_| Reply::json("200 OK", command_json(serde_json::Value::Null)));
    let started = std::time::Instant::now();
    let code = rt()
        .block_on(
            mock.client()
                .wait_command("sess_1", "cmd_1", Duration::from_millis(1200)),
        )
        .expect("wait");
    assert_eq!(code, None, "no exit within the deadline is unknown");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline bounds the whole wait: {:?}",
        started.elapsed()
    );
    mock.assert_custody(&[]);
}

#[test]
fn wait_command_maps_an_out_of_range_code_and_refuses_another_command() {
    let mock = Mock::start(|_| Reply::json("200 OK", command_json(serde_json::json!(1_i64 << 40))));
    let code = rt()
        .block_on(
            mock.client()
                .wait_command("sess_1", "cmd_1", Duration::from_secs(5)),
        )
        .expect("wait");
    assert_eq!(code, Some(EXEC_EXIT_STATUS_UNKNOWN));
    mock.assert_custody(&[]);

    let other = Mock::start(|_| {
        Reply::json(
            "200 OK",
            serde_json::json!({"command": {"id": "cmd_2", "exitCode": 0}}),
        )
    });
    let err = rt()
        .block_on(
            other
                .client()
                .wait_command("sess_1", "cmd_1", Duration::from_secs(5)),
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("different command"), "{err:#}");
    other.assert_custody(&[&err]);
}

#[test]
fn wait_command_errors_are_scrubbed_and_ids_are_checked_before_the_wire() {
    let mock = Mock::start(|_| {
        Reply::json(
            "400 Bad Request",
            serde_json::json!({"error": {"message": format!("bad token {TOKEN}")}}),
        )
    });
    let err = rt()
        .block_on(
            mock.client()
                .wait_command("sess_1", "cmd_1", Duration::from_secs(5)),
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("400"), "{err:#}");
    mock.assert_custody(&[&err]);
    assert_eq!(mock.requests().len(), 1, "a terminal 4xx is not retried");

    let bad = rt()
        .block_on(
            mock.client()
                .wait_command("sess_1", "../cmd", Duration::from_secs(5)),
        )
        .unwrap_err();
    assert_eq!(
        mock.requests().len(),
        1,
        "an invalid id never reaches the wire"
    );
    mock.assert_custody(&[&bad]);
}

// --- b156: host pin, OIDC, 429, delete, usage (THE-2233) -------------------------

/// A mock answering `/interactive` with a ticket whose URL is chosen by the
/// session id in the path.
fn ticket_mock() -> Mock {
    Mock::start(|r| {
        let url = match r.path() {
            "/v2/sandboxes/sessions/sess_run/interactive" => "wss://x.vercel.run/pty?ticket=1",
            "/v2/sandboxes/sessions/sess_evil/interactive" => "wss://evil.example/pty?ticket=1",
            "/v2/sandboxes/sessions/sess_app/interactive" => "wss://vercel.app/pty?ticket=1",
            "/v2/sandboxes/sessions/sess_appsub/interactive" => "wss://x.vercel.app/pty?ticket=1",
            "/v2/sandboxes/sessions/sess_ip/interactive" => "wss://203.0.113.7/pty?ticket=1",
            "/v2/sandboxes/sessions/sess_vx/interactive" => "wss://a.vx.example/pty?ticket=1",
            _ => "wss://unrouted.example/pty",
        };
        Reply::json(
            "200 OK",
            serde_json::json!({"url": url, "token": "tkt_secret_456"}),
        )
    })
}

fn assert_host_refusal(err: &anyhow::Error) {
    for shown in [format!("{err}"), format!("{err:#}"), format!("{err:?}")] {
        assert!(
            !shown.contains("tkt_secret_456"),
            "ticket in error: {shown}"
        );
        assert!(!shown.contains("ticket=1"), "query in error: {shown}");
        assert!(!shown.contains("wss://"), "url in error: {shown}");
        assert!(!shown.contains("/pty"), "url path in error: {shown}");
    }
}

#[test]
fn interactive_ticket_host_is_pinned_to_vercel_suffixes() {
    let mock = ticket_mock();
    let rt = rt();
    let client = mock.client();
    let ticket = rt
        .block_on(client.interactive("sess_run"))
        .expect("vercel.run is a default suffix");
    assert_eq!(ticket.url.host_str(), Some("x.vercel.run"));

    let mut errs = Vec::new();
    for session in ["sess_evil", "sess_app", "sess_appsub", "sess_ip", "sess_vx"] {
        let err = rt.block_on(client.interactive(session)).unwrap_err();
        assert_host_refusal(&err);
        errs.push(err);
    }
    // The refusal names the refused host (only the host) for the live run.
    let shown = format!("{:#}", errs[0]);
    assert!(shown.contains("(host evil.example)"), "{shown}");
    assert!(!shown.contains('?'), "query in error: {shown}");
    assert!(!shown.contains("token"), "token in error: {shown}");
    // The non-IP refusals carry the pin text and the key to set.
    for err in [&errs[0], &errs[1], &errs[2], &errs[4]] {
        let shown = format!("{err:#}");
        assert!(
            shown.contains(
                "vercel: interactive url host is not an allowed Vercel host \
                 (set env.<name>.provider.interactive_host_suffixes)"
            ),
            "{shown}"
        );
    }

    // A configured suffix replaces the defaults.
    let pinned = mock
        .client()
        .with_interactive_hosts(vec!["vx.example".to_string()]);
    let ticket = rt
        .block_on(pinned.interactive("sess_vx"))
        .expect("configured suffix");
    assert_eq!(ticket.url.host_str(), Some("a.vx.example"));
    let err = rt.block_on(pinned.interactive("sess_run")).unwrap_err();
    assert_host_refusal(&err);
    errs.push(err);

    let refs: Vec<&anyhow::Error> = errs.iter().collect();
    mock.assert_custody(&refs);
}

/// An unsigned JWT-shaped token whose payload carries `exp` (what an OIDC
/// token looks like to the local classifier). The whole string is the
/// sentinel.
fn oidc_token(exp_s: i64) -> String {
    use base64::Engine as _;
    let enc = |v: serde_json::Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string().as_bytes())
    };
    format!(
        "{}.{}.c2lnX3NlbnRpbmVs",
        enc(serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": "sentinel"})),
        enc(serde_json::json!({
            "iss": "https://oidc.vercel.com/team-sentinel",
            "aud": "https://vercel.com/team-sentinel",
            "sub": "owner:team-sentinel:project:prj:environment:development",
            "exp": exp_s,
        })),
    )
}

fn now_s() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

#[test]
fn an_oidc_token_near_expiry_makes_no_request_and_is_never_echoed() {
    let mock = Mock::start(|_| Reply::json("200 OK", sandbox_json("tg-a")));
    let rt = rt();
    let near = oidc_token(now_s() + 60);
    let client = mock.client_with_token(&near);
    let errs = [
        rt.block_on(client.get("tg-a", false)).unwrap_err(),
        rt.block_on(client.delete("tg-a")).unwrap_err(),
        rt.block_on(client.interactive("sess_1")).unwrap_err(),
    ];
    assert!(
        mock.requests().is_empty(),
        "an expiring OIDC token sent a request"
    );
    for err in &errs {
        let shown = format!("{err:#}");
        assert!(shown.contains("OIDC"), "{shown}");
        assert!(shown.contains("access token"), "{shown}");
    }
    let refs: Vec<&anyhow::Error> = errs.iter().collect();
    mock.assert_custody_of(&near, &refs);
    // The JWT's parts never surface either.
    for err in &errs {
        let shown = format!("{err:?}");
        for part in near.split('.') {
            assert!(!shown.contains(part), "token part in error: {shown}");
        }
    }

    let far = oidc_token(now_s() + 2 * 60 * 60);
    let got = rt
        .block_on(mock.client_with_token(&far).get("tg-a", false))
        .expect("an OIDC token with 2h left goes through");
    assert!(got.is_some());
    assert_eq!(mock.requests().len(), 1);
    mock.assert_custody_of(&far, &[]);
}

#[test]
fn an_access_token_is_unaffected_by_the_oidc_rule() {
    let mock = Mock::start(|_| Reply::json("200 OK", sandbox_json("tg-a")));
    let got = rt()
        .block_on(mock.client().get("tg-a", false))
        .expect("access token");
    assert!(got.is_some());
    assert_eq!(mock.requests().len(), 1);
    mock.assert_custody(&[]);
}

fn rate_limited(retry_after: &str) -> Reply {
    let mut reply = Reply::json(
        "429 Too Many Requests",
        serde_json::json!({"error": {"code": "rate_limited"}}),
    );
    reply.headers.push(("Retry-After", retry_after.to_string()));
    reply
}

#[test]
fn a_429_with_a_short_retry_after_is_waited_out() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let mock = Mock::start(move |_| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            rate_limited("1")
        } else {
            Reply::json("200 OK", sandbox_json("tg-a"))
        }
    });
    let started = std::time::Instant::now();
    let got = rt()
        .block_on(mock.client().get("tg-a", false))
        .expect("retried after Retry-After");
    assert!(got.is_some());
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "Retry-After: 1 was not honoured ({:?})",
        started.elapsed()
    );
    mock.assert_custody(&[]);
}

#[test]
fn a_429_with_a_long_or_unparsable_retry_after_fails_with_the_wait() {
    let mock = Mock::start(|r| {
        if r.path().ends_with("tg-long") {
            rate_limited("120")
        } else {
            rate_limited("Wed, 21 Oct 2015 07:28:00 GMT")
        }
    });
    let rt = rt();
    let long = rt
        .block_on(mock.client().get("tg-long", false))
        .unwrap_err();
    assert!(
        format!("{long:#}").contains("rate limited by Vercel; retry after 120s"),
        "{long:#}"
    );
    assert_eq!(mock.requests().len(), 1, "a 120 s wait must not be retried");
    let date = rt
        .block_on(mock.client().get("tg-date", false))
        .unwrap_err();
    assert!(
        format!("{date:#}").contains("rate limited by Vercel"),
        "{date:#}"
    );
    assert!(!format!("{date:#}").contains("2015"), "{date:#}");
    assert_eq!(
        mock.requests().len(),
        2,
        "an unparsable wait must not be retried"
    );
    mock.assert_custody(&[&long, &date]);
}

#[test]
fn a_429_on_create_or_run_is_not_retried() {
    let mock = Mock::start(|_| rate_limited("1"));
    let rt = rt();
    let create = rt
        .block_on(mock.client().create(&serde_json::json!({"name": "tg-a"})))
        .unwrap_err();
    assert!(
        format!("{create:#}").contains("rate limited by Vercel; retry after 1s"),
        "{create:#}"
    );
    assert_eq!(mock.requests().len(), 1);
    let run = rt
        .block_on(
            mock.client()
                .run_wait("sess_1", &serde_json::json!({"command": "true"}), 64),
        )
        .unwrap_err();
    assert!(
        format!("{run:#}").contains("rate limited by Vercel"),
        "{run:#}"
    );
    assert_eq!(mock.requests().len(), 2);
    mock.assert_custody(&[&create, &run]);
}

#[test]
fn stdin_retries_a_429_like_the_sdk() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let mock = Mock::start(move |r| {
        if r.path().ends_with("/cmd") {
            Reply::json("200 OK", serde_json::json!({"command": {"id": "cmd_1"}}))
        } else if seen.fetch_add(1, Ordering::SeqCst) < 2 {
            rate_limited("0")
        } else {
            Reply::json("200 OK", serde_json::json!({}))
        }
    });
    let client = mock.client();
    rt().block_on(client.run_detached(
        "sess_1",
        &serde_json::json!({"command": "sh", "attachStdin": true}),
    ))
    .expect("start stdin command");
    rt().block_on(client.stdin("sess_1", "cmd_1", b"hi", false))
        .expect("stdin retried through two 429s");
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    mock.assert_custody(&[]);
}

#[test]
fn delete_asks_for_orphan_snapshot_cleanup() {
    let mock = Mock::start(|_| Reply::json("200 OK", serde_json::json!({})));
    rt().block_on(mock.client().delete("tg-a")).expect("delete");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].method, "DELETE");
    assert_eq!(reqs[0].path(), "/v2/sandboxes/tg-a");
    assert_eq!(
        reqs[0]
            .query()
            .get("deleteOrphanSnapshots")
            .map(String::as_str),
        Some("true")
    );
    mock.assert_custody(&[]);
}

#[test]
fn usage_reads_the_current_session_without_resuming() {
    let mock = Mock::start(|r| match r.path() {
        "/v2/sandboxes/tg-a" => Reply::json(
            "200 OK",
            serde_json::json!({
                "sandbox": {
                    "name": "tg-a",
                    "status": "stopped",
                    "vcpus": 2,
                    "memory": 4096
                },
                "session": {
                    "id": "sess_1",
                    "activeCpuDurationMs": 90_000,
                    "startedAt": 1_700_000_000_000_i64,
                    "stoppedAt": 1_700_000_360_000_i64
                }
            }),
        ),
        _ => not_found(),
    });
    let rt = rt();
    let usage = rt
        .block_on(mock.client().usage("tg-a"))
        .expect("usage")
        .expect("exists");
    assert_eq!(usage.active_cpu_ms, Some(90_000));
    assert_eq!(usage.memory_mb, Some(4096));
    assert_eq!(usage.vcpus, Some(2));
    assert_eq!(usage.started_at_ms, Some(1_700_000_000_000));
    assert_eq!(usage.stopped_at_ms, Some(1_700_000_360_000));
    assert!(
        rt.block_on(mock.client().usage("tg-gone"))
            .expect("404")
            .is_none()
    );
    let reqs = mock.requests();
    assert_eq!(reqs[0].method, "GET");
    assert!(
        !reqs[0].query().contains_key("resume"),
        "usage must not resume"
    );
    assert_eq!(
        reqs[0].query().get("projectId").map(String::as_str),
        Some(PROJECT)
    );
    mock.assert_custody(&[]);
}

#[test]
fn snapshot_usage_pages_and_stops_at_the_cap() {
    // Every page names a fresh next cursor, so only the cap ends the walk.
    let mock = Mock::start(|r| {
        let page: u64 = r
            .query()
            .get("cursor")
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        Reply::json(
            "200 OK",
            serde_json::json!({
                "snapshots": [{
                    "id": format!("snap_{page}"),
                    "sizeBytes": 1_000_000,
                    "status": "created"
                }],
                "pagination": {"next": page + 1}
            }),
        )
    });
    let (snaps, complete) = rt()
        .block_on(mock.client().snapshot_usage("tg-a"))
        .expect("snapshots");
    assert!(!complete);
    assert_eq!(snaps.len(), 20);
    assert_eq!(snaps[0].id, "snap_0");
    assert_eq!(snaps[19].id, "snap_19");
    assert!(snaps.iter().all(|s| s.size_bytes == 1_000_000));
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 20, "the page cap bounds the walk");
    assert_eq!(reqs[0].path(), "/v2/sandboxes/snapshots");
    assert_eq!(
        reqs[0].query().get("name").map(String::as_str),
        Some("tg-a")
    );
    assert!(
        reqs.iter()
            .all(|req| { req.query().get("limit").map(String::as_str) == Some("50") })
    );
    assert!(!reqs[0].query().contains_key("cursor"));
    assert_eq!(reqs[1].query().get("cursor").map(String::as_str), Some("1"));
    mock.assert_custody(&[]);
}

#[test]
fn snapshot_usage_stops_on_the_last_page() {
    let mock = Mock::start(|_| {
        Reply::json(
            "200 OK",
            serde_json::json!({
                "snapshots": [{"id": "snap_1", "sizeBytes": 42, "status": "created"}],
                "pagination": {"next": null}
            }),
        )
    });
    let (snaps, complete) = rt()
        .block_on(mock.client().snapshot_usage("tg-a"))
        .expect("snapshots");
    assert!(complete);
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].size_bytes, 42);
    assert_eq!(mock.requests().len(), 1);
    mock.assert_custody(&[]);
}
