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
        VercelClient::new(
            &self.base,
            VercelToken::new(TOKEN.into()),
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
        for r in self.requests() {
            assert!(!r.target.contains(TOKEN), "token in target: {}", r.target);
            assert!(
                !String::from_utf8_lossy(&r.body).contains(TOKEN),
                "token in body of {} {}",
                r.method,
                r.path()
            );
            for (name, value) in &r.headers {
                if name != "authorization" {
                    assert!(!value.contains(TOKEN), "token in header {name}");
                }
            }
            assert_eq!(r.header("authorization"), format!("Bearer {TOKEN}"));
            assert!(
                r.header("user-agent").starts_with("thegn/"),
                "user-agent: {}",
                r.header("user-agent")
            );
            assert_eq!(r.query().get("teamId").map(String::as_str), Some(TEAM));
        }
        for e in errors {
            for shown in [format!("{e}"), format!("{e:#}"), format!("{e:?}")] {
                assert!(!shown.contains(TOKEN), "token in error: {shown}");
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
    let reply = (**router)(&req);
    let mut head = format!("HTTP/1.1 {}\r\n", reply.status);
    for (name, value) in &reply.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !reply.no_length {
        head.push_str(&format!("Content-Length: {}\r\n", reply.body.len()));
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
            "resources": { "vcpus": 2 },
            "createdAt": 1_700_000_000_000_i64
        }
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
        ("POST", "/v2/sandboxes") => Reply::json("200 OK", sandbox_json("tg-a")),
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
    assert_eq!(reqs[0].path(), "/v2/sandboxes");
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
    assert_eq!(q.get("until").map(String::as_str), Some("1699"));
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
    assert_eq!(reqs[2].method, "DELETE");
    assert_eq!(reqs[2].path(), "/v2/sandboxes/snapshots/snap_1");
    mock.assert_custody(&[]);
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
fn extend_reports_extended_or_the_plan_ceiling() {
    let mock = Mock::start(|r| {
        if r.target.contains("sess_full") {
            Reply::json(
                "400 Bad Request",
                serde_json::json!({"error": "max duration"}),
            )
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
    let reqs = mock.requests();
    assert_eq!(
        reqs[0].path(),
        "/v2/sandboxes/sessions/sess_1/extend-timeout"
    );
    assert_eq!(reqs[0].json(), serde_json::json!({"duration": 900_000}));
    mock.assert_custody(&[]);
}

#[test]
fn invalid_names_and_ids_never_reach_the_wire() {
    let mock = Mock::start(|_| Reply::json("200 OK", serde_json::json!({})));
    let client = mock.client();
    let rt = rt();
    let errs = vec![
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
    assert_eq!(
        reqs[0].query().get("projectId").map(String::as_str),
        Some(PROJECT)
    );
    assert!(
        reqs[0]
            .header("content-type")
            .starts_with("application/json")
    );
    // `wait: true` is forced whatever the caller sent.
    assert_eq!(reqs[0].json()["wait"], serde_json::json!(true));
    assert_eq!(reqs[0].json()["command"], serde_json::json!("git"));
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
fn run_wait_without_a_final_exit_is_unknown() {
    let mock = Mock::start(|_| {
        let mut reply = Reply::ndjson(&[started(), out("partial")]);
        reply.no_length = true;
        reply
    });
    let (code, output) = rt()
        .block_on(mock.client().run_wait("sess_1", &serde_json::json!({}), 64))
        .expect("run_wait");
    assert_eq!(code, EXEC_EXIT_STATUS_UNKNOWN);
    assert_eq!(output, b"partial");
    mock.assert_custody(&[]);
}

#[test]
fn run_wait_surfaces_an_error_line_sanitized() {
    let mock = Mock::start(|_| {
        Reply::ndjson(&[
            out("before"),
            serde_json::json!({"stream": "error", "data": {
                "code": "sandbox_stopped",
                "message": format!("session ended; token {TOKEN}\n{}", "q".repeat(400))
            }}),
            exit(0),
        ])
    });
    let err = rt()
        .block_on(mock.client().run_wait("sess_1", &serde_json::json!({}), 64))
        .unwrap_err();
    let shown = format!("{err:#}");
    assert!(shown.contains("sandbox_stopped"), "{shown}");
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
        .block_on(
            mock.client()
                .run_detached("sess_1", &serde_json::json!({"command": "sh"})),
        )
        .expect("detached");
    assert_eq!(id, "cmd_42");
    assert_eq!(mock.requests()[0].json()["wait"], serde_json::json!(false));
    mock.assert_custody(&[]);
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
        while let Some(line) = stream.recv().await {
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
        let first = stream.recv().await.expect("first line");
        assert_eq!(first, LogLine::Stdout(b"first".to_vec()));
        // The body is still open: the reader is parked on it, not finished.
        assert!(!stream.reader_finished());
        drop(stream);
    });
    mock.assert_custody(&[]);
}

#[test]
fn stdin_and_kill_encode_their_requests() {
    let mock = Mock::start(|_| Reply::json("200 OK", serde_json::json!({})));
    let client = mock.client();
    let rt = rt();
    rt.block_on(client.stdin("sess_1", "cmd_1", b"hello\n", true))
        .expect("stdin");
    rt.block_on(client.kill("sess_1", "cmd_1", 15))
        .expect("kill");
    let reqs = mock.requests();
    assert_eq!(
        reqs[0].path(),
        "/v2/sandboxes/sessions/sess_1/cmd/cmd_1/stdin"
    );
    assert_eq!(reqs[0].body, b"hello\n");
    assert_eq!(
        reqs[0].query().get("close").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        reqs[1].path(),
        "/v2/sandboxes/sessions/sess_1/cmd/cmd_1/kill"
    );
    assert_eq!(reqs[1].json(), serde_json::json!({"signal": 15}));
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

// --- interactive -----------------------------------------------------------------

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
    let mock = Mock::start(|_| {
        Reply::json(
            "200 OK",
            serde_json::json!({"snapshots": [{"id": "snap_1"}, {"id": "bad/id"}, {"id": "snap_2"}]}),
        )
    });
    let ids = rt()
        .block_on(mock.client().snapshots_for("tg-a"))
        .expect("snapshots");
    assert_eq!(ids, vec!["snap_1".to_string(), "snap_2".to_string()]);
    let reqs = mock.requests();
    assert_eq!(reqs[0].path(), "/v2/sandboxes/snapshots");
    assert_eq!(
        reqs[0].query().get("name").map(String::as_str),
        Some("tg-a")
    );
    mock.assert_custody(&[]);
}

#[test]
fn client_debug_never_shows_the_token() {
    let mock = Mock::start(|_| Reply::json("200 OK", serde_json::json!({})));
    let shown = format!("{:?}", mock.client());
    assert!(!shown.contains(TOKEN), "{shown}");
}
