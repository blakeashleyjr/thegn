//! Fixture-backed selected Job attempts on a project-bound Sprite.
//!
//! Every test drives the real `SpritesSelectedRunner` against a loopback server
//! that speaks the shapes the provider actually uses: `PUT
//! /sprites/{name}/fs/write` and `GET /sprites/{name}/fs/read` for the
//! materialized tree, and a `/sprites/{name}/exec` WebSocket for the attempt
//! itself. Nothing is stubbed at the Rust boundary — the argv that leaves the
//! provider, the bytes that cross `fs/write`, the exit frame and the durable
//! ledger record are all observed as the provider emits them.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use thegn_core::sprite_hold::HOLD_SCRIPT;
use thegn_job_spec::{Recipe, RunnerRefusal, SelectedCompletion, SelectedRunner};
use thegn_svc::provider::SpritesProvider;
use thegn_svc::sprites::job::{
    AttemptOptions, AttemptState, AttemptStore, Cleanup, DEFAULT_CHECKOUT_ROOT, DirSnapshot,
    ProjectBinding, ReserveRefusal, ReserveRequest, Resume, SnapshotContent, SnapshotSource,
    SpritesSelectedRunner,
};

// ---------------------------------------------------------------- the fixture

type Fs = Arc<Mutex<HashMap<String, Vec<u8>>>>;

#[derive(Clone, Copy)]
enum Hs {
    Accept,
    Status(u16),
}

enum Out {
    /// The `session_info` control the server sends on connect.
    Info(&'static str),
    Stdout(&'static str),
    Stderr(&'static str),
    Exit(i32),
    /// Send nothing more and hold the socket open until the client closes it.
    Silence,
}

struct Script {
    handshake: Hs,
    out: Vec<Out>,
}

#[derive(Debug, Clone)]
struct Exec {
    target: String,
    authorization: String,
}

struct Mock {
    base: String,
    fs: Fs,
    execs: Arc<Mutex<Vec<Exec>>>,
    writes: Arc<Mutex<Vec<String>>>,
    write_status: Arc<Mutex<Option<u16>>>,
}

#[derive(Clone)]
struct State {
    fs: Fs,
    execs: Arc<Mutex<Vec<Exec>>>,
    writes: Arc<Mutex<Vec<String>>>,
    write_status: Arc<Mutex<Option<u16>>>,
    scripts: Arc<Mutex<VecDeque<Script>>>,
}

fn start_mock(scripts: Vec<Script>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let fs: Fs = Arc::new(Mutex::new(HashMap::new()));
    let execs = Arc::new(Mutex::new(Vec::new()));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let write_status = Arc::new(Mutex::new(None));
    let state = State {
        fs: Arc::clone(&fs),
        execs: Arc::clone(&execs),
        writes: Arc::clone(&writes),
        write_status: Arc::clone(&write_status),
        scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
    };
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let state = state.clone();
            thread::spawn(move || serve(stream, state));
        }
    });
    Mock {
        base: format!("http://127.0.0.1:{port}/v1"),
        fs,
        execs,
        writes,
        write_status,
    }
}

/// Refuse every `fs/write` with `status` — a denied transfer, not a denied exec.
fn deny_writes(mock: &Mock, status: u16) {
    *mock.write_status.lock().unwrap() = Some(status);
}

fn script(out: Vec<Out>) -> Script {
    Script {
        handshake: Hs::Accept,
        out,
    }
}

fn session_info() -> Out {
    Out::Info(r#"{"type":"session_info","session_id":"1847","tty":false}"#)
}

/// One request head, with the body already drained.
struct Head {
    method: String,
    target: String,
    authorization: String,
    websocket_key: String,
    body: Vec<u8>,
}

fn serve(mut stream: TcpStream, state: State) {
    let Some(head) = read_head(&mut stream) else {
        return;
    };
    if head.target.contains("/exec?") {
        let exec = Exec {
            target: head.target.clone(),
            authorization: head.authorization.clone(),
        };
        state.execs.lock().unwrap().push(exec);
        let next = state.scripts.lock().unwrap().pop_front();
        match next {
            Some(script) => exec_ws(stream, &head, script),
            None => respond(&mut stream, 500, b""),
        }
        return;
    }
    let Some(path) = param(&head.target, "path") else {
        respond(&mut stream, 400, b"");
        return;
    };
    if head.method == "PUT" && head.target.contains("/fs/write?") {
        let status = *state.write_status.lock().unwrap();
        if let Some(status) = status {
            respond(&mut stream, status, b"");
            return;
        }
        state.writes.lock().unwrap().push(path.clone());
        state.fs.lock().unwrap().insert(path, head.body.clone());
        respond(&mut stream, 200, b"{}");
        return;
    }
    if head.method == "GET" && head.target.contains("/fs/read?") {
        let bytes = state.fs.lock().unwrap().get(&path).cloned();
        match bytes {
            Some(bytes) => respond(&mut stream, 200, &bytes),
            None => respond(&mut stream, 404, b""),
        }
        return;
    }
    respond(&mut stream, 404, b"");
}

fn read_head(stream: &mut TcpStream) -> Option<Head> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read_exact(&mut byte).is_err() {
            return None;
        }
        bytes.push(byte[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break;
        }
        if bytes.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&bytes).into_owned();
    let mut lines = head.split("\r\n");
    let mut request = lines.next()?.split_whitespace();
    let method = request.next()?.to_owned();
    let target = request.next()?.to_owned();
    let authorization = header(&head, "authorization").unwrap_or_default();
    let websocket_key = header(&head, "sec-websocket-key").unwrap_or_default();
    let length = header(&head, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 && stream.read_exact(&mut body).is_err() {
        return None;
    }
    Some(Head {
        method,
        target,
        authorization,
        websocket_key,
        body,
    })
}

fn header(head: &str, name: &str) -> Option<String> {
    head.split("\r\n").find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

fn respond(stream: &mut TcpStream, status: u16, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let delivered = stream.write_all(head.as_bytes()).is_ok()
        && stream.write_all(body).is_ok()
        && stream.flush().is_ok();
    if !delivered {
        // The client hung up first (a deadline test closes its socket). The
        // assertion that cares reports it, and a panic here would only kill the
        // mock thread.
        tracing::debug!("mock response was not delivered");
    }
}

fn exec_ws(mut stream: TcpStream, head: &Head, script: Script) {
    if let Hs::Status(code) = script.handshake {
        respond(&mut stream, code, b"");
        return;
    }
    if head.websocket_key.is_empty() {
        return;
    }
    let accept =
        tokio_tungstenite::tungstenite::handshake::derive_accept_key(head.websocket_key.as_bytes());
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }

    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::{Role, WebSocket};

    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    for out in script.out {
        let message = match out {
            Out::Info(text) => Message::Text(text.into()),
            Out::Stdout(text) => {
                Message::Binary([vec![1u8], text.as_bytes().to_vec()].concat().into())
            }
            Out::Stderr(text) => {
                Message::Binary([vec![2u8], text.as_bytes().to_vec()].concat().into())
            }
            Out::Exit(code) => {
                let byte = u8::try_from(code).unwrap_or(0);
                Message::Binary(vec![3u8, byte].into())
            }
            Out::Silence => loop {
                match socket.read() {
                    Ok(Message::Close(_)) | Err(_) => return,
                    Ok(_) => {}
                }
            },
        };
        if socket.send(message).is_err() {
            return;
        }
    }
}

/// Percent-decode one query parameter value.
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let pair = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(pair, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first value of `key` in a request target's query.
fn param(target: &str, key: &str) -> Option<String> {
    params(target, key).into_iter().next()
}

/// Every value of `key`, in request order.
fn params(target: &str, key: &str) -> Vec<String> {
    let Some(query) = target.split_once('?').map(|(_, query)| query) else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name == key).then(|| decode(value))
        })
        .collect()
}

// ------------------------------------------------------------------ the tests

const OUTPUT: &str = "target/debug/app";

fn binding() -> ProjectBinding {
    ProjectBinding::new("proj-one", "acct", "tgtest1").unwrap()
}

/// The pinned snapshot: two files, one of them dirty relative to `HEAD`.
fn content() -> SnapshotContent {
    let mut content = SnapshotContent::new();
    content
        .insert("Cargo.toml", b"[package]\nname = \"demo\"\n".to_vec())
        .unwrap();
    content
        .insert(
            "src/main.rs",
            b"fn main() { println!(\"dirty\"); }\n".to_vec(),
        )
        .unwrap();
    content
}

struct Fixed(SnapshotContent);

impl SnapshotSource for Fixed {
    fn capture(&self) -> anyhow::Result<SnapshotContent> {
        Ok(self.0.clone())
    }
}

fn store() -> (tempfile::TempDir, AttemptStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = AttemptStore::at(dir.path().join("ledger"));
    (dir, store)
}

fn runner(mock: &Mock, store: AttemptStore, options: AttemptOptions) -> SpritesSelectedRunner {
    SpritesSelectedRunner::with_options(
        SpritesProvider::new(&mock.base, "tok", "tgtest1"),
        binding(),
        store,
        Box::new(Fixed(content())),
        tokio::runtime::Handle::current(),
        options,
    )
    .unwrap()
}

fn outputs_options(deadline: Duration) -> AttemptOptions {
    AttemptOptions {
        deadline,
        outputs: vec![OUTPUT.to_owned()],
        ..AttemptOptions::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_attempt_materializes_the_pinned_tree_and_reports_the_exit() {
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Stdout("   Compiling demo v0.1.0\n"),
        Out::Stderr("warning: unused variable\n"),
        Out::Exit(0),
    ])]);
    let checked_out = binding().checkout(1, DEFAULT_CHECKOUT_ROOT);
    // The declared output lives in the checkout the attempt is about to create.
    mock.fs.lock().unwrap().insert(
        format!("{checked_out}/{OUTPUT}"),
        b"\x7fELF the built binary".to_vec(),
    );
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &content.digest(), &content)
        .await
        .unwrap();

    assert_eq!(receipt.provider, "sprites");
    assert_eq!(receipt.project, "proj-one");
    assert_eq!(receipt.sprite, "tgtest1");
    assert_eq!(receipt.attempt, 1);
    assert_eq!(receipt.checkout, checked_out);
    assert_eq!(receipt.source, content.digest());
    assert_eq!(receipt.materialized, content.digest());
    assert_eq!(receipt.completion, SelectedCompletion::Exited(0));
    assert_eq!(receipt.exit, Some(0));
    assert_eq!(receipt.cleanup, Cleanup::NotNeeded);
    assert!(!receipt.outputs_fenced);
    assert_eq!(receipt.session.as_deref(), Some("1847"));
    assert!(
        receipt
            .hold
            .as_deref()
            .is_some_and(|hold| hold.starts_with("thegn-hold-"))
    );
    assert_eq!(receipt.files_transferred, 2);
    assert_eq!(receipt.files_reused, 0);
    assert_eq!(receipt.bytes_transferred, content.total_bytes());
    assert_eq!(receipt.outputs_missing, Vec::<String>::new());
    let hashed = receipt.outputs.get(OUTPUT).expect("output was hashed");
    assert_eq!(hashed.len(), 64);
    assert!(receipt.stdout_tail.contains("Compiling demo"));
    assert_eq!(receipt.log_tail, receipt.stdout_tail);
    // No configured rate means no fabricated price.
    assert_eq!(receipt.cost_micros, None);

    // The tree that crossed `fs/write` is exactly the pinned content.
    let mut writes = mock.writes.lock().unwrap().clone();
    writes.sort();
    assert_eq!(
        writes,
        vec![
            format!("{checked_out}/Cargo.toml"),
            format!("{checked_out}/src/main.rs"),
        ]
    );
    assert_eq!(
        mock.fs
            .lock()
            .unwrap()
            .get(&format!("{checked_out}/Cargo.toml"))
            .map(Vec::as_slice),
        Some(b"[package]\nname = \"demo\"\n".as_slice())
    );

    // The argv is the hold-wrapped recipe behind the provider's own `cd`
    // wrapper, and the warm cache is the project-scoped one.
    let execs = mock.execs.lock().unwrap().clone();
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].authorization, "Bearer tok");
    let cmd = params(&execs[0].target, "cmd");
    assert_eq!(cmd.len(), 13);
    assert_eq!(cmd[0], "/bin/sh");
    assert_eq!(cmd[1], "-c");
    assert_eq!(cmd[3], "thegn-exec");
    assert_eq!(cmd[4], checked_out);
    assert_eq!(cmd[5], "/bin/sh");
    assert_eq!(cmd[6], "-c");
    assert_eq!(cmd[7], HOLD_SCRIPT);
    assert_eq!(cmd[8], "thegn-hold");
    assert_eq!(cmd[9], receipt.hold.clone().unwrap());
    // The hold outlives the deadline: 30s deadline plus one hold expiry (300s),
    // so a quiet build never loses its hold before its own deadline.
    assert_eq!(cmd[10], "330");
    assert_eq!(cmd[11], "cargo");
    assert_eq!(cmd[12], "build");
    assert_eq!(
        params(&execs[0].target, "env"),
        vec![format!(
            "CARGO_TARGET_DIR={}",
            binding().cache(DEFAULT_CHECKOUT_ROOT)
        )]
    );
    assert_eq!(params(&execs[0].target, "tty"), vec!["false".to_owned()]);

    // The ledger agrees with the receipt and never reuses attempt 1.
    let record = store.read(1).unwrap().expect("attempt 1 is recorded");
    assert_eq!(record.state, AttemptState::Finished);
    assert_eq!(record.exit, Some(0));
    assert_eq!(record.cleanup, Cleanup::NotNeeded);
    assert_eq!(record.epoch, 1);
    assert_eq!(store.next_attempt().unwrap(), 2);
    assert_eq!(
        store.resume_attempt(1).unwrap(),
        Resume::Terminal(SelectedCompletion::Exited(0))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_recipe_keeps_its_exit_code_and_adds_the_stderr_tail() {
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Stdout("error: could not compile `demo`\n"),
        Out::Stderr("error[E0308]: mismatched types\n"),
        Out::Exit(101),
    ])]);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let content = content();

    let receipt = runner
        .run(Recipe::CargoTest, &content.digest(), &content)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Exited(101));
    assert_eq!(receipt.exit, Some(101));
    assert_eq!(receipt.cleanup, Cleanup::NotNeeded);
    // A real exit still fences nothing: the doomed build's outputs are read.
    assert!(!receipt.outputs_fenced);
    assert!(receipt.stderr_tail.contains("E0308"));
    assert!(receipt.log_tail.contains("E0308"));
    assert!(receipt.log_tail.contains("could not compile"));
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Finished
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missed_exit_is_indeterminate_with_cleanup_pending_and_no_outputs() {
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Stdout("   Compiling demo v0.1.0\n"),
        Out::Silence,
    ])]);
    let (_dir, store) = store();
    let deadline = Duration::from_millis(300);
    let runner = runner(&mock, store.clone(), outputs_options(deadline));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &content.digest(), &content)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Cancelled);
    assert_eq!(receipt.exit, None);
    assert_eq!(receipt.cleanup, Cleanup::Pending);
    assert!(receipt.outputs_fenced);
    assert!(receipt.outputs.is_empty());
    assert_eq!(receipt.stdout_tail, "   Compiling demo v0.1.0\n");

    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Cancelled);
    assert_eq!(record.cleanup, Cleanup::Pending);
    assert!(record.outputs_fenced);
    assert_eq!(
        store.resume_attempt(1).unwrap(),
        Resume::Terminal(SelectedCompletion::Cancelled)
    );
    // A reconciler confirms the owned tree is gone, and the ledger says so.
    assert!(store.mark_cleaned(1).unwrap());
    assert_eq!(store.read(1).unwrap().unwrap().cleanup, Cleanup::Done);
    assert!(!store.mark_cleaned(1).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkout_that_never_opened_is_indeterminate_and_keeps_the_code() {
    let mock = start_mock(vec![script(vec![session_info(), Out::Exit(125)])]);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &content.digest(), &content)
        .await
        .unwrap();

    // 125 is the wrapper's `cd` failure: the recipe itself never ran.
    assert_eq!(receipt.completion, SelectedCompletion::Indeterminate);
    assert_eq!(receipt.exit, None);
    assert_eq!(receipt.cleanup, Cleanup::Pending);
    assert!(receipt.outputs_fenced);
    assert!(receipt.outputs.is_empty());
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Indeterminate
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_handshake_is_a_refusal_that_commits_no_spend() {
    let mock = start_mock(vec![Script {
        handshake: Hs::Status(401),
        out: Vec::new(),
    }]);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let content = content();

    let refusal = runner
        .run(Recipe::CargoBuild, &content.digest(), &content)
        .await
        .unwrap_err();

    let why = match refusal {
        RunnerRefusal::RunnerUnavailable(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("sprites-tgtest1"), "{why}");
    assert!(why.contains("401"), "{why}");
    // Nothing ran, so the reservation is closed with no cleanup and no spend,
    // and the caller may retry as a new attempt.
    let record = store.read(1).unwrap().expect("the reservation is closed");
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert_eq!(record.cleanup, Cleanup::NotNeeded);
    assert_eq!(record.committed_micros(), 0);
    assert!(record.outputs_fenced);
    assert!(record.error.unwrap_or_default().contains("401"));
    assert_eq!(store.next_attempt().unwrap(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_digest_mismatch_is_refused_before_anything_leaves_the_host() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let content = content();
    let mut other = SnapshotContent::new();
    other
        .insert("src/main.rs", b"fn main() {}\n".to_vec())
        .unwrap();

    let refusal = runner
        .run(Recipe::CargoBuild, &other.digest(), &content)
        .await
        .unwrap_err();

    match refusal {
        RunnerRefusal::SourceMismatch { expected, observed } => {
            assert_eq!(expected, other.digest());
            assert_eq!(observed, content.digest());
        }
        other => panic!("expected a source mismatch, got {other:?}"),
    }
    assert!(store.records().unwrap().is_empty());
    assert!(mock.writes.lock().unwrap().is_empty());
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_snapshot_is_refused_because_there_is_no_checkout_to_run_in() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let empty = SnapshotContent::new();

    let refusal = runner
        .run(Recipe::CargoBuild, &empty.digest(), &empty)
        .await
        .unwrap_err();

    assert!(matches!(refusal, RunnerRefusal::InvalidSpec(_)));
    assert!(store.records().unwrap().is_empty());
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_spend_envelope_stops_the_attempt_that_would_exceed_it() {
    let mock = start_mock(vec![script(vec![session_info(), Out::Exit(0)])]);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        AttemptOptions {
            spend_cap_micros: 600,
            spend_envelope_micros: 1_000,
            ..outputs_options(Duration::from_secs(30))
        },
    );
    let content = content();
    let source = content.digest();

    let first = runner
        .run(Recipe::CargoBuild, &source, &content)
        .await
        .unwrap();
    assert_eq!(first.attempt, 1);
    // Unpriced, so the whole reserved cap still counts against the envelope.
    assert_eq!(store.read(1).unwrap().unwrap().committed_micros(), 600);

    let refusal = runner
        .run(Recipe::CargoBuild, &source, &content)
        .await
        .unwrap_err();
    let why = match refusal {
        RunnerRefusal::RunnerUnavailable(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("spend envelope exhausted"), "{why}");
    // The refused attempt never existed: no row, no socket, no checkout.
    assert!(store.read(2).unwrap().is_none());
    assert_eq!(mock.execs.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attempt_number_that_is_already_recorded_is_never_rerun() {
    let one = script(vec![session_info(), Out::Exit(0)]);
    let two = script(vec![session_info(), Out::Exit(0)]);
    let mock = start_mock(vec![one, two]);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let content = content();
    let source = content.digest();
    let first = runner
        .run(Recipe::CargoBuild, &source, &content)
        .await
        .unwrap();
    assert_eq!(first.attempt, 1);

    // A second submission gets a fresh number rather than replaying attempt 1.
    let second = runner
        .run(Recipe::CargoBuild, &source, &content)
        .await
        .unwrap();
    assert_eq!(second.attempt, 2);
    assert_eq!(
        second.checkout,
        binding().checkout(2, DEFAULT_CHECKOUT_ROOT)
    );
    assert_ne!(second.checkout, first.checkout);
    assert_eq!(mock.execs.lock().unwrap().len(), 2);

    // Reserving a recorded number is a refusal, not a rerun.
    let refusal = store
        .reserve(
            &binding(),
            ReserveRequest {
                attempt: 1,
                recipe: Recipe::CargoBuild,
                source: &source,
                checkout: binding().checkout(1, DEFAULT_CHECKOUT_ROOT),
                deadline_ms: 0,
                spend_cap_micros: 600,
                spend_envelope_micros: 0,
            },
        )
        .unwrap_err();
    assert!(matches!(
        refusal,
        ReserveRefusal::Duplicate(AttemptState::Finished)
    ));
    assert_eq!(mock.execs.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_dispatch_reads_back_as_unknown_and_is_never_rerun() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let content = content();
    let source = content.digest();
    // Simulate a process that wrote the dispatch fence and then died.
    let mut record = store
        .reserve(
            &binding(),
            ReserveRequest {
                attempt: 1,
                recipe: Recipe::CargoBuild,
                source: &source,
                checkout: binding().checkout(1, DEFAULT_CHECKOUT_ROOT),
                deadline_ms: 0,
                spend_cap_micros: 600,
                spend_envelope_micros: 0,
            },
        )
        .unwrap();
    record.state = AttemptState::Dispatching;
    store.mark(&record).unwrap();

    assert_eq!(store.resume_attempt(1).unwrap(), Resume::UnknownDispatch);
    // The number stays taken, and the fence keeps counting its cap.
    assert_eq!(store.read(1).unwrap().unwrap().committed_micros(), 600);
    assert!(matches!(
        store
            .reserve(
                &binding(),
                ReserveRequest {
                    attempt: 1,
                    recipe: Recipe::CargoBuild,
                    source: &source,
                    checkout: binding().checkout(1, DEFAULT_CHECKOUT_ROOT),
                    deadline_ms: 0,
                    spend_cap_micros: 600,
                    spend_envelope_micros: 0,
                },
            )
            .unwrap_err(),
        ReserveRefusal::Duplicate(AttemptState::Dispatching)
    ));
    assert_eq!(store.next_attempt().unwrap(), 2);
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebinding_a_project_marks_older_attempts_stale() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let content = content();
    let source = content.digest();
    let first = store
        .reserve(
            &binding(),
            ReserveRequest {
                attempt: 1,
                recipe: Recipe::CargoBuild,
                source: &source,
                checkout: binding().checkout(1, DEFAULT_CHECKOUT_ROOT),
                deadline_ms: 0,
                spend_cap_micros: 600,
                spend_envelope_micros: 0,
            },
        )
        .unwrap();
    assert_eq!(first.epoch, 1);

    // The same project, rebound to a different sprite.
    let rebound = ProjectBinding::new("proj-one", "acct", "tgtest2").unwrap();
    let second = store
        .reserve(
            &rebound,
            ReserveRequest {
                attempt: 2,
                recipe: Recipe::CargoBuild,
                source: &source,
                checkout: rebound.checkout(2, DEFAULT_CHECKOUT_ROOT),
                deadline_ms: 0,
                spend_cap_micros: 600,
                spend_envelope_micros: 0,
            },
        )
        .unwrap();
    assert_eq!(second.epoch, 2);
    assert_eq!(store.resume_attempt(1).unwrap(), Resume::StaleEpoch);
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sync_runner_contract_reaches_the_sprites_provider() {
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Stdout("   Compiling demo v0.1.0\n"),
        Out::Exit(0),
    ])]);
    let (_dir, store) = store();
    let runner = runner(&mock, store, outputs_options(Duration::from_secs(30)));
    let content = content();
    let source = content.digest();

    // `submit` blocks on the runtime handle, so it must be called from a plain
    // thread — exactly how the dispatcher calls it.
    let submitted = source.clone();
    let worker = std::thread::spawn(move || {
        let wrong = runner
            .submit("sprites-someone-else", Recipe::CargoBuild, &submitted)
            .unwrap_err();
        assert!(matches!(wrong, RunnerRefusal::WrongRunner));
        runner.submit("sprites-tgtest1", Recipe::CargoBuild, &submitted)
    });
    let result = worker.join().unwrap().unwrap();

    assert_eq!(result.runner, "sprites-tgtest1");
    assert_eq!(result.recipe, Recipe::CargoBuild);
    assert_eq!(result.source, source);
    assert_eq!(result.completion, SelectedCompletion::Exited(0));
    assert!(result.log_tail.contains("Compiling demo"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_transfer_is_refused_before_a_command_can_run() {
    let mock = start_mock(Vec::new());
    deny_writes(&mock, 403);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );
    let content = content();

    let refusal = runner
        .run(Recipe::CargoBuild, &content.digest(), &content)
        .await
        .unwrap_err();

    let why = match refusal {
        RunnerRefusal::RunnerUnavailable(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("403"), "{why}");
    // The checkout was never complete, so no command ran, nothing is pending
    // and nothing is spent: the caller may retry as a new attempt.
    let record = store.read(1).unwrap().expect("the reservation is closed");
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert_eq!(record.cleanup, Cleanup::NotNeeded);
    assert_eq!(record.committed_micros(), 0);
    assert!(record.outputs_fenced);
    assert!(mock.execs.lock().unwrap().is_empty());
    assert!(mock.writes.lock().unwrap().is_empty());
    assert_eq!(store.next_attempt().unwrap(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_loop_retains_the_project_cache_but_never_the_checkout() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Exit(0)]),
        script(vec![session_info(), Out::Exit(0)]),
    ]);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        outputs_options(Duration::from_secs(30)),
    );

    // Cold: three files, one of which the warm loop deletes.
    let mut cold = SnapshotContent::new();
    cold.insert("Cargo.toml", b"[package]\nname = \"demo\"\n".to_vec())
        .unwrap();
    cold.insert("src/main.rs", b"fn main() {}\n".to_vec())
        .unwrap();
    cold.insert("src/old.rs", b"pub fn old() {}\n".to_vec())
        .unwrap();
    let cold_receipt = runner
        .run(Recipe::CargoBuild, &cold.digest(), &cold)
        .await
        .unwrap();
    assert_eq!(cold_receipt.files_transferred, 3);

    // Warm: the tracked entry point is dirty, one tracked file is deleted, and
    // an allowed untracked input appears beside them.
    let mut warm = SnapshotContent::new();
    warm.insert("Cargo.toml", b"[package]\nname = \"demo\"\n".to_vec())
        .unwrap();
    warm.insert(
        "src/main.rs",
        b"fn main() { println!(\"warmer\"); }\n".to_vec(),
    )
    .unwrap();
    warm.insert("notes.md", b"allowed untracked input\n".to_vec())
        .unwrap();
    let warm_receipt = runner
        .run(Recipe::CargoTest, &warm.digest(), &warm)
        .await
        .unwrap();

    assert_eq!(warm_receipt.attempt, 2);
    assert_eq!(
        warm_receipt.checkout,
        binding().checkout(2, DEFAULT_CHECKOUT_ROOT)
    );
    assert_ne!(warm_receipt.checkout, cold_receipt.checkout);
    // A per-attempt checkout holds nothing over, so the whole edited tree
    // crosses again: the project cache is the one state attempt 2 inherits.
    assert_eq!(warm_receipt.files_transferred, 3);
    assert_eq!(warm_receipt.files_reused, 0);
    assert_eq!(warm_receipt.materialized, warm.digest());
    assert_eq!(warm_receipt.completion, SelectedCompletion::Exited(0));

    let cache = format!(
        "CARGO_TARGET_DIR={}",
        binding().cache(DEFAULT_CHECKOUT_ROOT)
    );
    let execs = mock.execs.lock().unwrap().clone();
    assert_eq!(execs.len(), 2);
    assert_eq!(params(&execs[0].target, "env"), vec![cache.clone()]);
    assert_eq!(params(&execs[1].target, "env"), vec![cache]);
    assert_eq!(params(&execs[0].target, "cmd")[4], cold_receipt.checkout);
    assert_eq!(params(&execs[1].target, "cmd")[4], warm_receipt.checkout);
    assert_eq!(
        params(&execs[1].target, "cmd").last().map(String::as_str),
        Some("test")
    );

    // The deletion only ever applies to the warm checkout: the first attempt's
    // tree is untouched, and the cache is the only shared path.
    let fs = mock.fs.lock().unwrap();
    assert!(fs.contains_key(&format!("{}/src/old.rs", cold_receipt.checkout)));
    assert!(!fs.contains_key(&format!("{}/src/old.rs", warm_receipt.checkout)));
    assert_eq!(
        fs.get(&format!("{}/src/main.rs", warm_receipt.checkout))
            .map(Vec::as_slice),
        Some(b"fn main() { println!(\"warmer\"); }\n".as_slice())
    );
    assert!(!fs.contains_key(&binding().cache(DEFAULT_CHECKOUT_ROOT)));
}

#[test]
fn a_snapshot_carries_the_dirty_tree_and_its_allowed_untracked_input() {
    let root = tempfile::tempdir().unwrap();
    let write = |rel: &str, bytes: &[u8]| {
        let path = root.path().join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    };
    write("Cargo.toml", b"[package]\nname = \"demo\"\n");
    write("src/main.rs", b"fn main() {}\n");
    write("notes.md", b"an allowed untracked input\n");
    write(".git/config", b"[core]\n");
    write(".env", b"SECRET=1\n");
    write(".env.local", b"SECRET=2\n");
    write(".ssh/id_ed25519", b"private\n");
    write(".thegn/state.json", b"{}\n");

    let snapshot = DirSnapshot::new(root.path());
    let captured = snapshot.capture().unwrap();
    let paths: Vec<&str> = captured.files().map(|(path, _)| path).collect();
    assert_eq!(paths, vec!["Cargo.toml", "notes.md", "src/main.rs"]);
    // The credential trees are gone byte-for-byte, not just path-filtered.
    assert!(captured.get(".env").is_none());
    assert!(captured.get(".ssh/id_ed25519").is_none());
    assert!(captured.get(".thegn/state.json").is_none());

    // Warm loop: a dirty edit, a deletion and an untracked addition all change
    // the digest, so the pinned identity cannot silently match a stale tree.
    let baseline = captured.digest();
    write("src/main.rs", b"fn main() { println!(\"edited\"); }\n");
    let edited = snapshot.capture().unwrap();
    assert_ne!(edited.digest(), baseline);

    std::fs::remove_file(root.path().join("notes.md")).unwrap();
    let deleted = snapshot.capture().unwrap();
    assert_ne!(deleted.digest(), edited.digest());
    assert!(deleted.get("notes.md").is_none());
    let unchanged = DirSnapshot::new(root.path()).capture().unwrap();
    assert_eq!(unchanged.digest(), deleted.digest());
}
