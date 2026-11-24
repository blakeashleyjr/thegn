//! Fixture-backed selected Job attempts on a project-selected Vercel Sandbox.
//!
//! Every test drives the real `VercelSelectedRunner` and the real Vercel client
//! against a loopback server that speaks the shapes the client uses: `POST
//! /v3/sandboxes`, `GET /v2/sandboxes/{name}`, the session `fs/write` (gzipped
//! ustar) and `fs/read` calls, a detached `cmd`, its NDJSON `logs`, `kill`,
//! `stop` and the sandbox `DELETE`. Nothing is stubbed at the Rust boundary: the
//! create body, the bytes that cross `fs/write`, the command body and the
//! durable ledger record are all observed as the client emits them.
//!
//! The sentinel API token must appear only as `Authorization: Bearer <token>`.
//!
//! Run: `cargo test -p thegn-svc --test vercel_job`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use thegn_core::vercel::api::Scope;
use thegn_job_spec::{Recipe, RunnerRefusal, SelectedCompletion};
use thegn_svc::vercel::job::{
    AttemptOptions, AttemptReceipt, AttemptRecord, AttemptState, AttemptStore, Cleanup,
    DEFAULT_CHECKOUT_ROOT, JobBinding, OwnerLease, ReserveRefusal, ReserveRequest, SnapshotContent,
    SnapshotSource, VercelSelectedRunner,
};
use thegn_svc::vercel::{VercelProvider, VercelSpec, VercelToken};

const TOKEN: &str = "vcp_SENTINEL_job_token_77";
const TEAM: &str = "team_sentinel1";
const OTHER_TEAM: &str = "team_other1";
const PROJECT: &str = "prj_sentinel1";
const OUTPUT: &str = "out/result.txt";
const DEADLINE: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------- the fixture

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Recorded {
    fn path(&self) -> &str {
        self.target
            .split_once('?')
            .map_or(self.target.as_str(), |(path, _)| path)
    }

    fn query(&self) -> HashMap<String, String> {
        self.target
            .split_once('?')
            .map(|(_, query)| {
                url::form_urlencoded::parse(query.as_bytes())
                    .into_owned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

struct Reply {
    status: &'static str,
    ctype: &'static str,
    body: Vec<u8>,
    /// Omit `Content-Length` (a close-delimited body, as the log follow uses).
    no_length: bool,
    /// Keep the connection open this long after writing (a live follow).
    hold: Duration,
}

impl Reply {
    fn json(status: &'static str, value: serde_json::Value) -> Self {
        Reply {
            status,
            ctype: "application/json",
            body: value.to_string().into_bytes(),
            no_length: false,
            hold: Duration::ZERO,
        }
    }

    fn ok() -> Self {
        Reply::json("200 OK", serde_json::json!({}))
    }

    fn not_found() -> Self {
        Reply::json(
            "404 Not Found",
            serde_json::json!({"error": {"code": "not_found"}}),
        )
    }

    fn server_error() -> Self {
        Reply::json(
            "500 Internal Server Error",
            serde_json::json!({"error": {"code": "internal"}}),
        )
    }
}

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
        let router = Arc::new(router);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let rec = rec.clone();
                let router = router.clone();
                thread::spawn(move || handle(stream, &rec, router.as_ref()));
            }
        });
        Mock {
            base: format!("http://127.0.0.1:{port}"),
            recorded,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.recorded.lock().unwrap().clone()
    }

    fn count(&self, method: &str, suffix: &str) -> usize {
        self.requests()
            .iter()
            .filter(|req| req.method == method && req.path().ends_with(suffix))
            .count()
    }

    /// The sentinel token appears only as the bearer header, and every request
    /// is scoped to the expected team.
    fn assert_custody(&self) {
        for req in self.requests() {
            assert_eq!(
                req.headers.get("authorization").map(String::as_str),
                Some(format!("Bearer {TOKEN}").as_str()),
                "{} {}",
                req.method,
                req.target
            );
            assert!(!req.target.contains(TOKEN), "token in {}", req.target);
            assert!(
                !String::from_utf8_lossy(&req.body).contains(TOKEN),
                "token in the body of {} {}",
                req.method,
                req.target
            );
            assert_eq!(
                req.query().get("teamId").map(String::as_str),
                Some(TEAM),
                "{}",
                req.target
            );
        }
    }
}

fn read_line(stream: &mut TcpStream, out: &mut String) -> Option<()> {
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
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

fn handle(
    mut stream: TcpStream,
    rec: &Mutex<Vec<Recorded>>,
    router: &(dyn Fn(&Recorded) -> Reply + Send + Sync),
) {
    let mut line = String::new();
    if read_line(&mut stream, &mut line).is_none() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("").to_string();
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        if read_line(&mut stream, &mut header).is_none() {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0usize);
    let mut body = vec![0u8; length];
    if length > 0 && stream.read_exact(&mut body).is_err() {
        return;
    }
    let req = Recorded {
        method,
        target,
        headers,
        body,
    };
    rec.lock().unwrap().push(req.clone());
    let reply = router(&req);
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\n",
        reply.status, reply.ctype
    );
    if !reply.no_length {
        writeln!(head, "Content-Length: {}\r", reply.body.len())
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

/// What the fake sandbox holds, as the requests have shaped it.
#[derive(Default)]
struct State {
    files: HashMap<String, Vec<u8>>,
    modes: HashMap<String, u64>,
    tags: serde_json::Value,
    created: bool,
    deleted: bool,
}

/// How the fake command behaves.
#[derive(Clone)]
struct Script {
    logs: Vec<serde_json::Value>,
    /// Keep the log follow open this long after the lines (no EOF).
    hold: Duration,
    /// Answer the detached `cmd` start with a 500 (an ambiguous start).
    start_fails: bool,
    /// Answer the detached `cmd` start with a 400 (a definite refusal).
    start_rejected: bool,
    /// Create the sandbox, then answer the create with an unreadable body (an
    /// ambiguous create: the sandbox exists but the reply says nothing).
    create_unreadable: bool,
    /// Run when the detached `cmd` start arrives.
    on_start: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn stdout_line(text: &str) -> serde_json::Value {
    serde_json::json!({"stream": "stdout", "data": text})
}

fn stderr_line(text: &str) -> serde_json::Value {
    serde_json::json!({"stream": "stderr", "data": text})
}

fn exit_line(code: i32) -> serde_json::Value {
    serde_json::json!({"command": {"id": "cmd_1", "exitCode": code}})
}

fn sandbox_json(name: &str, tags: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "sandbox": {
            "name": name,
            "status": "running",
            "currentSessionId": "sess_1",
            "vcpus": 2,
            "createdAt": 1_700_000_000_000_i64,
            "tags": tags
        },
        "session": {"id": "sess_1", "status": "running"},
        "routes": [],
        "resumed": false
    })
}

fn octal(field: &[u8]) -> u64 {
    let text: String = field
        .iter()
        .take_while(|byte| **byte != 0 && **byte != b' ')
        .map(|byte| *byte as char)
        .collect();
    u64::from_str_radix(text.trim(), 8).unwrap()
}

/// The one entry of an `fs/write` upload: name, mode, bytes.
fn untar_single(targz: &[u8]) -> (String, u64, Vec<u8>) {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(targz)
        .read_to_end(&mut tar)
        .unwrap();
    let name = String::from_utf8_lossy(&tar[..100])
        .trim_end_matches('\0')
        .to_string();
    let size = usize::try_from(octal(&tar[124..136])).unwrap();
    (name, octal(&tar[100..108]), tar[512..512 + size].to_vec())
}

fn router(
    state: Arc<Mutex<State>>,
    script: Script,
    output: &'static [u8],
) -> impl Fn(&Recorded) -> Reply + Send + Sync + 'static {
    move |req| {
        let path = req.path().to_owned();
        let method = req.method.as_str();
        let mut state = state.lock().unwrap();
        match (method, path.as_str()) {
            ("POST", "/v3/sandboxes") => {
                let body = req.json();
                let name = body["name"].as_str().unwrap_or("").to_owned();
                state.tags = body["tags"].clone();
                state.created = true;
                if script.create_unreadable {
                    return Reply {
                        status: "200 OK",
                        ctype: "application/json",
                        body: b"{".to_vec(),
                        no_length: false,
                        hold: Duration::ZERO,
                    };
                }
                Reply::json("200 OK", sandbox_json(&name, &state.tags))
            }
            ("GET", "/v2/sandboxes") => Reply::json(
                "200 OK",
                serde_json::json!({"sandboxes": [], "pagination": {"next": null}}),
            ),
            ("GET", "/v2/sandboxes/snapshots") => {
                Reply::json("200 OK", serde_json::json!({"snapshots": []}))
            }
            ("GET", p) if p.starts_with("/v2/sandboxes/") && !p.contains("/sessions/") => {
                if state.created && !state.deleted {
                    let name = p.trim_start_matches("/v2/sandboxes/");
                    Reply::json("200 OK", sandbox_json(name, &state.tags))
                } else {
                    Reply::not_found()
                }
            }
            ("DELETE", p) if p.starts_with("/v2/sandboxes/") => {
                state.deleted = true;
                Reply::ok()
            }
            ("POST", p) if p.ends_with("/fs/write") => {
                let cwd = req.headers.get("x-cwd").cloned().unwrap_or_default();
                let (name, mode, data) = untar_single(&req.body);
                let full = format!("{}/{name}", cwd.trim_end_matches('/'));
                state.modes.insert(full.clone(), mode);
                state.files.insert(full, data);
                Reply::ok()
            }
            ("POST", p) if p.ends_with("/fs/read") => {
                let wanted = req.json()["path"].as_str().unwrap_or("").to_owned();
                match state.files.get(&wanted) {
                    Some(bytes) => Reply {
                        status: "200 OK",
                        ctype: "application/octet-stream",
                        body: bytes.clone(),
                        no_length: false,
                        hold: Duration::ZERO,
                    },
                    None if wanted.ends_with(&format!("/{OUTPUT}")) => Reply {
                        status: "200 OK",
                        ctype: "application/octet-stream",
                        body: output.to_vec(),
                        no_length: false,
                        hold: Duration::ZERO,
                    },
                    None => Reply::not_found(),
                }
            }
            ("POST", p) if p.ends_with("/cmd") => {
                if let Some(hook) = &script.on_start {
                    hook();
                }
                if script.start_rejected {
                    Reply::json(
                        "400 Bad Request",
                        serde_json::json!({"error": {"code": "bad_request"}}),
                    )
                } else if script.start_fails {
                    Reply::server_error()
                } else {
                    Reply::json("200 OK", serde_json::json!({"command": {"id": "cmd_1"}}))
                }
            }
            ("GET", p) if p.ends_with("/cmd/cmd_1/logs") => {
                let mut body = String::new();
                for line in &script.logs {
                    body.push_str(&line.to_string());
                    body.push('\n');
                }
                Reply {
                    status: "200 OK",
                    ctype: "application/x-ndjson",
                    body: body.into_bytes(),
                    no_length: true,
                    hold: script.hold,
                }
            }
            ("GET", p) if p.ends_with("/cmd/cmd_1") => Reply::json(
                "200 OK",
                serde_json::json!({"command": {"id": "cmd_1", "exitCode": null}}),
            ),
            ("POST", p) if p.ends_with("/kill") || p.ends_with("/stop") => Reply::ok(),
            _ => Reply::not_found(),
        }
    }
}

fn start(script: Script) -> (Mock, Arc<Mutex<State>>) {
    let state = Arc::new(Mutex::new(State::default()));
    let mock = Mock::start(router(state.clone(), script, b"built"));
    (mock, state)
}

fn script(logs: Vec<serde_json::Value>) -> Script {
    Script {
        logs,
        hold: Duration::ZERO,
        start_fails: false,
        start_rejected: false,
        create_unreadable: false,
        on_start: None,
    }
}

// ----------------------------------------------------------------- the runner

struct Fixed(SnapshotContent);

impl SnapshotSource for Fixed {
    fn capture(&self) -> anyhow::Result<SnapshotContent> {
        Ok(self.0.clone())
    }
}

fn content() -> SnapshotContent {
    let mut content = SnapshotContent::new();
    content
        .insert("Cargo.toml", b"[package]\nname = \"x\"\n".to_vec())
        .unwrap();
    content
        .insert("src/main.rs", b"fn main() {}\n".to_vec())
        .unwrap();
    content
        .insert_file("build.sh", b"#!/bin/sh\n".to_vec(), true)
        .unwrap();
    content
}

fn provider(mock: &Mock) -> VercelProvider {
    provider_with(mock, 0)
}

fn provider_with(mock: &Mock, max_lifetime_secs: u64) -> VercelProvider {
    VercelProvider::new(VercelSpec {
        api_base: mock.base.clone(),
        token: VercelToken::new(TOKEN.into()),
        credential_ref: "VERCEL_TOKEN".into(),
        scope: Scope {
            team_id: TEAM.into(),
            project_id: PROJECT.into(),
        },
        name: String::new(),
        vcpus: 2,
        image: None,
        region: None,
        max_instances: 2,
        max_lifetime_secs,
        interactive_host_suffixes: Vec::new(),
        model_credentials: thegn_core::config::ModelCredentials::None,
    })
    .unwrap()
}

fn store() -> (tempfile::TempDir, AttemptStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = AttemptStore::at(dir.path().join("ledger"));
    (dir, store)
}

fn options() -> AttemptOptions {
    AttemptOptions {
        outputs: vec![OUTPUT.to_owned()],
        ..AttemptOptions::default()
    }
}

fn runner_for(
    mock: &Mock,
    team: &str,
    store: AttemptStore,
    options: AttemptOptions,
) -> VercelSelectedRunner {
    VercelSelectedRunner::with_options(
        provider(mock),
        JobBinding::new("proj", team, PROJECT).unwrap(),
        store,
        Box::new(Fixed(content())),
        tokio::runtime::Handle::current(),
        options,
    )
    .unwrap()
}

fn runner(mock: &Mock, store: AttemptStore) -> VercelSelectedRunner {
    runner_for(mock, TEAM, store, options())
}

fn runner_with(mock: &Mock, store: AttemptStore, options: AttemptOptions) -> VercelSelectedRunner {
    runner_for(mock, TEAM, store, options)
}

// ---------------------------------------------------------------------- tests

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pinned_attempt_runs_on_a_fresh_sandbox_and_reports_exit_and_digest() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, fs) = start(script(vec![stdout_line("compiling\n"), exit_line(0)]));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let content = content();
    let pinned = content.digest().unwrap();

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned, &content, DEADLINE)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Exited(0));
    assert_eq!(receipt.exit, Some(0));
    assert_eq!(receipt.materialized, pinned);
    assert_eq!(receipt.files_transferred, 3);
    assert!(
        receipt.log_tail.contains("compiling"),
        "{}",
        receipt.log_tail
    );
    assert_eq!(receipt.cleanup, Cleanup::Done);
    assert!(receipt.sandbox_deleted);
    assert_eq!(
        receipt.outputs.get(OUTPUT),
        Some(&format!("{:x}", Sha256::digest(b"built")))
    );
    assert!(!receipt.outputs_fenced);

    // The durable record names the sandbox and the command, and agrees.
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Finished);
    assert_eq!(record.exit, Some(0));
    assert_eq!(record.sandbox, receipt.sandbox);
    assert_eq!(record.session.as_deref(), Some("sess_1"));
    assert_eq!(record.command.as_deref(), Some("cmd_1"));
    assert_eq!(
        (record.team_id.as_str(), record.project_id.as_str()),
        (TEAM, PROJECT)
    );
    assert_eq!(record.cleanup, Cleanup::Done);
    assert!(record.sandbox_deleted);

    // The tree the sandbox holds is exactly the snapshot, with its modes.
    let held = fs.lock().unwrap();
    let tree: Vec<(&String, &Vec<u8>)> = held
        .files
        .iter()
        .filter(|(path, _)| path.starts_with(&record.checkout))
        .collect();
    assert_eq!(tree.len(), 3);
    let main = format!("{}/src/main.rs", record.checkout);
    assert_eq!(held.files.get(&main).unwrap(), b"fn main() {}\n");
    assert_eq!(
        held.modes[&format!("{}/build.sh", record.checkout)] & 0o777,
        0o755
    );
    assert_eq!(held.modes[&main] & 0o777, 0o644);
    drop(held);

    // The request stream: create with the resource ceiling and a bounded
    // lifetime, before any file write; then stop and delete.
    let requests = mock.requests();
    let create = requests
        .iter()
        .position(|req| req.method == "POST" && req.path() == "/v3/sandboxes")
        .expect("create");
    let first_write = requests
        .iter()
        .position(|req| req.path().ends_with("/fs/write"))
        .expect("write");
    assert!(create < first_write);
    let body = requests[create].json();
    assert_eq!(body["name"], serde_json::json!(receipt.sandbox));
    assert_eq!(body["resources"]["vcpus"], serde_json::json!(2));
    // transfer budget (15 min) + the 60 s deadline + 5 min slack.
    assert_eq!(body["timeout"], serde_json::json!(1_260_000u64));
    assert_eq!(mock.count("POST", "/stop"), 1);
    assert_eq!(mock.count("DELETE", &receipt.sandbox), 1);
    mock.assert_custody();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_carries_only_the_recipe_and_no_environment() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    let runner = runner(&mock, store);
    let content = content();

    runner
        .run(
            Recipe::CargoTest,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap();

    let requests = mock.requests();
    let commands: Vec<&Recorded> = requests
        .iter()
        .filter(|req| req.method == "POST" && req.path().ends_with("/cmd"))
        .collect();
    assert_eq!(commands.len(), 1, "one dispatch per attempt");
    let body = commands[0].json();
    assert_eq!(body["command"], serde_json::json!("/bin/sh"));
    assert_eq!(body["wait"], serde_json::json!(false));
    assert!(body.get("env").is_none(), "{body}");
    let args = body["args"].as_array().unwrap();
    assert!(args.ends_with(&[serde_json::json!("cargo"), serde_json::json!("test")]));
    let text = body.to_string();
    for needle in [TOKEN, "VERCEL_TOKEN", ".claude", ".codex"] {
        assert!(!text.contains(needle), "{needle} reached the sandbox");
    }
    mock.assert_custody();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nonzero_exit_is_reported_with_the_stderr_tail() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(script(vec![
        stdout_line("building\n"),
        stderr_line("boom\n"),
        exit_line(2),
    ]));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let content = content();

    let receipt = runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Exited(2));
    assert!(receipt.log_tail.contains("[stderr"), "{}", receipt.log_tail);
    assert!(receipt.log_tail.contains("boom"));
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Finished);
    assert_eq!(record.exit, Some(2));
    assert_eq!(record.cleanup, Cleanup::Done);
    mock.assert_custody();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deadline_kills_the_command_stops_the_sandbox_and_fences_outputs() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(Script {
        logs: vec![stdout_line("tick\n")],
        hold: Duration::from_secs(8),
        ..script(Vec::new())
    });
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let content = content();

    let receipt = runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::TimedOut);
    assert!(receipt.outputs_fenced);
    assert!(receipt.outputs.is_empty());
    assert_eq!(receipt.exit, None);
    assert_eq!(receipt.cleanup, Cleanup::Done);
    assert!(receipt.log_tail.contains("tick"));
    let kill = mock
        .requests()
        .into_iter()
        .find(|req| req.method == "POST" && req.path().ends_with("/cmd/cmd_1/kill"))
        .expect("the command was killed");
    assert_eq!(kill.json()["signal"], serde_json::json!(9));
    assert_eq!(mock.count("POST", "/stop"), 1, "the session was stopped");
    assert_eq!(mock.count("DELETE", &receipt.sandbox), 1);
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::TimedOut);
    assert!(record.outputs_fenced);
    assert_eq!(record.command.as_deref(), Some("cmd_1"));
    mock.assert_custody();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ambiguous_command_start_is_indeterminate_and_never_redispatched() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(Script {
        start_fails: true,
        ..script(Vec::new())
    });
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let content = content();

    let receipt = runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Indeterminate);
    assert_eq!(mock.count("POST", "/cmd"), 1, "never redispatched");
    assert!(receipt.outputs_fenced);
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert!(record.error.as_deref().unwrap_or("").contains("unknown"));
    assert_eq!(record.command, None);
    // The sandbox is still deleted, so nothing is left billing.
    assert_eq!(record.cleanup, Cleanup::Done);
    assert_eq!(mock.count("DELETE", &receipt.sandbox), 1);
    mock.assert_custody();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_team_or_project_mismatch_is_refused_before_any_create() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    // The provider is bound to TEAM; the binding demands another team.
    let runner = runner_for(&mock, OTHER_TEAM, store.clone(), options());
    let content = content();

    let refusal = runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap_err();

    let why = match refusal {
        RunnerRefusal::RunnerUnavailable(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("refusing to claim"), "{why}");
    assert!(mock.requests().is_empty(), "no request of any kind");
    assert!(store.records().unwrap().is_empty());
    assert!(store.binding().unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_checks_the_recorded_team_before_touching_anything() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(script(Vec::new()));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    // An attempt another team's binding left pending.
    let binding = JobBinding::new("proj", OTHER_TEAM, PROJECT).unwrap();
    let source = content().digest().unwrap();
    let mut record = store
        .reserve(
            &binding,
            ReserveRequest {
                attempt: None,
                recipe: Recipe::CargoBuild,
                source: &source,
                checkout_root: DEFAULT_CHECKOUT_ROOT,
                deadline_ms: 0,
                spend_cap_micros: 0,
                spend_envelope_micros: 0,
            },
        )
        .unwrap();
    record.sandbox = "tg-job-0123456789abcdef".to_owned();
    record.state = AttemptState::Running;
    record.cleanup = Cleanup::Pending;
    store.mark(&record).unwrap();

    let error = runner.reconcile(1).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("refusing to claim"),
        "{error:#}"
    );
    let error = runner.cancel(1).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("refusing to claim"),
        "{error:#}"
    );
    assert!(mock.requests().is_empty());
    let after: AttemptRecord = store.read(1).unwrap().unwrap();
    assert_eq!(after.cleanup, Cleanup::Pending, "the record is untouched");
}

#[test]
fn the_ledger_is_pinned_to_the_first_team_and_project_it_served() {
    let (_dir, store) = store();
    let source = content().digest().unwrap();
    let request = || ReserveRequest {
        attempt: None,
        recipe: Recipe::CargoBuild,
        source: &source,
        checkout_root: DEFAULT_CHECKOUT_ROOT,
        deadline_ms: 0,
        spend_cap_micros: 600,
        spend_envelope_micros: 1_000,
    };
    let ours = JobBinding::new("proj", TEAM, PROJECT).unwrap();
    let theirs = JobBinding::new("proj", OTHER_TEAM, PROJECT).unwrap();

    let first = store.reserve(&ours, request()).unwrap();
    assert_eq!(first.attempt, 1);
    assert_eq!(first.cleanup, Cleanup::NotNeeded);
    assert!(matches!(
        store.reserve(&theirs, request()),
        Err(ReserveRefusal::IdentityMismatch { .. })
    ));
    // The envelope counts the unreported cap of the first attempt.
    assert!(matches!(
        store.reserve(&ours, request()),
        Err(ReserveRefusal::EnvelopeExhausted { committed: 600, .. })
    ));
    assert_eq!(store.records().unwrap().len(), 1);
}

// ------------------------------------------------- ceiling, spend and recovery

/// Leave the finished attempt 1 looking like a crashed run: still running, its
/// sandbox alive, with the given ledger deadline.
fn revive(store: &AttemptStore, fs: &Arc<Mutex<State>>, deadline_ms: i64) {
    fs.lock().unwrap().deleted = false;
    let mut record = store.read(1).unwrap().unwrap();
    record.state = AttemptState::Running;
    record.exit = None;
    record.finished_ms = None;
    record.cleanup = Cleanup::Pending;
    record.sandbox_deleted = false;
    record.outputs.clear();
    record.outputs_fenced = false;
    record.deadline_ms = deadline_ms;
    store.mark(&record).unwrap();
}

async fn finished_attempt(runner: &VercelSelectedRunner) -> AttemptReceipt {
    let content = content();
    runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_running_attempt_is_cancelled_with_a_kill_stop_and_delete() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let receipt = finished_attempt(&runner).await;
    revive(&store, &fs, 0);
    let (kills, stops, deletes) = (
        mock.count("POST", "/kill"),
        mock.count("POST", "/stop"),
        mock.count("DELETE", &receipt.sandbox),
    );

    let record = runner.cancel(1).await.unwrap();

    assert_eq!(record.state, AttemptState::Cancelled);
    assert_eq!(record.cleanup, Cleanup::Done);
    assert!(record.sandbox_deleted);
    assert!(record.outputs_fenced);
    assert_eq!(mock.count("POST", "/kill"), kills + 1);
    assert_eq!(mock.count("POST", "/stop"), stops + 1);
    assert_eq!(mock.count("DELETE", &receipt.sandbox), deletes + 1);
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Cancelled
    );
    mock.assert_custody();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_settles_a_stale_running_attempt_as_indeterminate_and_never_reruns() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let receipt = finished_attempt(&runner).await;
    revive(&store, &fs, 0);
    let (dispatches, deletes) = (
        mock.count("POST", "/cmd"),
        mock.count("DELETE", &receipt.sandbox),
    );

    let record = runner.reconcile(1).await.unwrap();

    assert_eq!(record.state, AttemptState::Indeterminate);
    assert_eq!(record.cleanup, Cleanup::Done);
    assert_eq!(mock.count("POST", "/cmd"), dispatches, "never redispatched");
    assert_eq!(mock.count("DELETE", &receipt.sandbox), deletes + 1);
    mock.assert_custody();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_refuses_a_row_that_may_still_belong_to_a_live_run() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let receipt = finished_attempt(&runner).await;
    let in_an_hour = thegn_core::util::now_ms() + 3_600_000;
    revive(&store, &fs, in_an_hour);
    let (kills, deletes) = (
        mock.count("POST", "/kill"),
        mock.count("DELETE", &receipt.sandbox),
    );

    for error in [
        runner.reconcile(1).await.unwrap_err(),
        runner.cancel(1).await.unwrap_err(),
    ] {
        assert!(format!("{error:#}").contains("live run"), "{error:#}");
    }

    assert_eq!(mock.count("POST", "/kill"), kills, "nothing was killed");
    assert_eq!(mock.count("DELETE", &receipt.sandbox), deletes);
    assert_eq!(store.read(1).unwrap().unwrap().state, AttemptState::Running);
}

#[test]
fn a_ledger_write_is_refused_when_the_row_changed_since_it_was_read() {
    let (_dir, store) = store();
    let source = content().digest().unwrap();
    let binding = JobBinding::new("proj", TEAM, PROJECT).unwrap();
    let record = store
        .reserve(
            &binding,
            ReserveRequest {
                attempt: None,
                recipe: Recipe::CargoBuild,
                source: &source,
                checkout_root: DEFAULT_CHECKOUT_ROOT,
                deadline_ms: 0,
                spend_cap_micros: 0,
                spend_envelope_micros: 0,
            },
        )
        .unwrap();
    // The owner moves on after the recovery read `record`.
    let mut owner = record.clone();
    owner.state = AttemptState::Finished;
    owner.exit = Some(0);
    store.mark(&owner).unwrap();
    let mut recovery = record.clone();
    recovery.state = AttemptState::Indeterminate;

    assert!(!store.mark_if_unchanged(&record, &recovery).unwrap());
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Finished
    );
    assert!(store.mark_if_unchanged(&owner, &recovery).unwrap());
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Indeterminate
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ambiguous_create_is_left_pending_and_never_dispatches() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(Script {
        create_unreadable: true,
        ..script(vec![exit_line(0)])
    });
    let (_dir, store) = store();
    let runner = runner_with(&mock, store.clone(), options());
    let content = content();

    let refusal = runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap_err();

    assert!(matches!(refusal, RunnerRefusal::RunnerUnavailable(_)));
    assert_eq!(mock.count("POST", "/cmd"), 0, "nothing was dispatched");
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Rejected);
    // The provider's failed-create cleanup deletes the possibly-created
    // sandbox by name; the executor then confirms it is gone before calling
    // cleanup unnecessary.
    assert!(
        mock.requests()
            .iter()
            .any(|req| req.method == "DELETE" && req.path().starts_with("/v2/sandboxes/")),
        "the cleanup delete ran"
    );
    assert_eq!(
        record.cleanup,
        Cleanup::NotNeeded,
        "the sandbox was confirmed gone"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_handle_that_cannot_be_persisted_is_indeterminate_not_success() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (_dir, store) = store();
    let ledger = store.dir().to_path_buf();
    let moved = ledger.with_extension("moved");
    let (hook_ledger, hook_moved) = (ledger.clone(), moved.clone());
    // When the command start arrives the ledger has just recorded `Dispatching`:
    // swap the ledger directory for a symlink so the next write is refused.
    let (mock, _fs) = start(Script {
        on_start: Some(Arc::new(move || {
            std::fs::rename(&hook_ledger, &hook_moved).unwrap();
            std::os::unix::fs::symlink(&hook_moved, &hook_ledger).unwrap();
        })),
        ..script(vec![exit_line(0)])
    });
    let runner = runner_with(&mock, store.clone(), options());

    let receipt = finished_attempt(&runner).await;

    std::fs::remove_file(&ledger).unwrap();
    std::fs::rename(&moved, &ledger).unwrap();
    assert_eq!(receipt.completion, SelectedCompletion::Indeterminate);
    assert_eq!(mock.count("POST", "/cmd"), 1);
    assert_eq!(mock.count("POST", "/kill"), 1, "the command was stopped");
    assert_eq!(mock.count("DELETE", &receipt.sandbox), 1);
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(
        record.state,
        AttemptState::Dispatching,
        "no success on record"
    );
    assert_eq!(record.command, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_attempt_after_the_create_records_an_estimated_spend() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, _fs) = start(Script {
        start_rejected: true,
        ..script(Vec::new())
    });
    let (_dir, store) = store();
    let runner = runner_with(
        &mock,
        store.clone(),
        AttemptOptions {
            micros_per_second: 1_000_000,
            ..options()
        },
    );
    let content = content();

    let refusal = runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap_err();

    assert!(matches!(refusal, RunnerRefusal::RunnerUnavailable(_)));
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Rejected);
    assert_eq!(record.cleanup, Cleanup::Done);
    assert!(
        record.spent_micros.is_some_and(|spent| spent > 0),
        "the sandbox's elapsed time is billed: {:?}",
        record.spent_micros
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sandbox_timeout_is_bounded_by_the_configured_lifetime_ceiling() {
    let _state = thegn_core::testenv::IsolatedState::new();
    // The default 15 min transfer budget + 60 s + 5 min slack > a 600 s ceiling.
    let (mock, _fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    let runner = VercelSelectedRunner::with_options(
        provider_with(&mock, 600),
        JobBinding::new("proj", TEAM, PROJECT).unwrap(),
        store.clone(),
        Box::new(Fixed(content())),
        tokio::runtime::Handle::current(),
        options(),
    )
    .unwrap();
    let content = content();

    let refusal = runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap_err();

    assert!(
        matches!(refusal, RunnerRefusal::InvalidSpec(_)),
        "{refusal:?}"
    );
    assert!(mock.requests().is_empty(), "refused before any request");
    assert!(store.records().unwrap().is_empty(), "and before the ledger");

    // A budget that fits is sent as the create timeout.
    let (mock, _fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = self::store();
    let runner = VercelSelectedRunner::with_options(
        provider_with(&mock, 600),
        JobBinding::new("proj", TEAM, PROJECT).unwrap(),
        store,
        Box::new(Fixed(content.clone())),
        tokio::runtime::Handle::current(),
        AttemptOptions {
            transfer_budget: Duration::from_secs(60),
            ..options()
        },
    )
    .unwrap();
    runner
        .run(
            Recipe::CargoBuild,
            &content.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap();
    let create = mock
        .requests()
        .into_iter()
        .find(|req| req.method == "POST" && req.path() == "/v3/sandboxes")
        .unwrap();
    // 60 s transfer + 60 s deadline + 5 min slack = 420 s <= 600 s.
    assert_eq!(create.json()["timeout"], serde_json::json!(420_000u64));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_owner_lease_refuses_recovery_even_past_the_staleness_margin() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    let receipt = finished_attempt(&runner).await;
    revive(&store, &fs, 0);
    let mut record = store.read(1).unwrap().unwrap();
    record.lease = Some(OwnerLease::current(thegn_core::util::now_ms()));
    store.mark(&record).unwrap();
    let (kills, deletes) = (
        mock.count("POST", "/kill"),
        mock.count("DELETE", &receipt.sandbox),
    );

    for error in [
        runner.reconcile(1).await.unwrap_err(),
        runner.cancel(1).await.unwrap_err(),
    ] {
        assert!(format!("{error:#}").contains("live run"), "{error:#}");
    }

    assert_eq!(mock.count("POST", "/kill"), kills, "nothing was killed");
    assert_eq!(mock.count("DELETE", &receipt.sandbox), deletes);
    assert_eq!(store.read(1).unwrap().unwrap(), record);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_whose_lease_owner_is_dead_and_heartbeat_old_is_reconciled() {
    let _state = thegn_core::testenv::IsolatedState::new();
    let (mock, fs) = start(script(vec![exit_line(0)]));
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone());
    finished_attempt(&runner).await;
    revive(&store, &fs, 0);
    let mut record = store.read(1).unwrap().unwrap();
    record.lease = Some(OwnerLease {
        pid: u32::MAX - 1,
        boot_id: OwnerLease::current(0).boot_id,
        start_ticks: None,
        started_at_ms: 0,
        heartbeat_ms: 0,
    });
    store.mark(&record).unwrap();

    let settled = runner.reconcile(1).await.unwrap();

    assert_eq!(settled.state, AttemptState::Indeterminate);
    assert_eq!(settled.cleanup, Cleanup::Done);
}

#[test]
fn a_lease_heartbeat_loses_to_a_concurrent_recovery_write_and_the_owner_fences() {
    let (_dir, store) = store();
    let source = content().digest().unwrap();
    let binding = JobBinding::new("proj", TEAM, PROJECT).unwrap();
    let mut owner = store
        .reserve(
            &binding,
            ReserveRequest {
                attempt: None,
                recipe: Recipe::CargoBuild,
                source: &source,
                checkout_root: DEFAULT_CHECKOUT_ROOT,
                deadline_ms: 0,
                spend_cap_micros: 0,
                spend_envelope_micros: 0,
            },
        )
        .unwrap();
    owner.lease = Some(OwnerLease::current(1_000));
    store.mark(&owner).unwrap();

    assert!(store.refresh_lease(&mut owner, 2_000).unwrap());
    assert_eq!(owner.lease.as_ref().unwrap().heartbeat_ms, 2_000);
    assert_eq!(store.read(1).unwrap().unwrap(), owner);

    let mut recovery = owner.clone();
    recovery.state = AttemptState::Indeterminate;
    assert!(store.mark_if_unchanged(&owner, &recovery).unwrap());

    let held = owner.clone();
    assert!(!store.refresh_lease(&mut owner, 3_000).unwrap());
    assert_eq!(owner, held);
    assert_eq!(store.read(1).unwrap().unwrap(), recovery);
}
