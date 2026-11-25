//! Fixture-backed selected Job attempts on a project-bound Sprite.
//!
//! Every test drives the real `SpritesSelectedRunner` against a loopback server
//! that speaks the shapes the provider actually uses: `PUT
//! /sprites/{name}/fs/write`, `GET /sprites/{name}/fs/read`, `GET .../fs/list`
//! and `DELETE .../fs/delete` for the materialized tree, and a
//! `/sprites/{name}/exec` WebSocket for the attempt (and its kill) itself. Nothing is stubbed at the Rust boundary — the argv that leaves the
//! provider, the bytes that cross `fs/write`, the exit frame and the durable
//! ledger record are all observed as the provider emits them.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use thegn_cas::{Digest, Entry, encode_directory};
use thegn_core::sprite_hold::HOLD_SCRIPT;
use thegn_job_spec::{Recipe, RunnerRefusal, SelectedCancel, SelectedCompletion, SelectedRunner};
use thegn_svc::provider::SpritesProvider;
use thegn_svc::sprites::job::{
    AttemptOptions, AttemptRecord, AttemptState, AttemptStore, CancelToken, Cleanup,
    DEFAULT_CHECKOUT_ROOT, DirSnapshot, KILL_SCRIPT, LAUNCH_FAILED_MARKER, LAUNCH_SCRIPT,
    OwnerLease, ProjectBinding, ReconcileOutcome, Reconciled, ReserveRefusal, ReserveRequest,
    Resume, SNAPSHOT_MAX_FILE_BYTES, SnapshotContent, SnapshotSource, SpritesSelectedRunner,
};

// ---------------------------------------------------------------- the fixture

type Fs = Arc<Mutex<HashMap<String, Vec<u8>>>>;

#[derive(Clone, Copy)]
enum Hs {
    Accept,
    Status(u16),
    /// Hold the upgrade for this many milliseconds, then accept it.
    Delayed(u64),
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
    /// Files the "build" leaves in its checkout (relative path, bytes), made
    /// visible as soon as the command starts.
    produces: Vec<(&'static str, &'static [u8])>,
}

/// One accepted `fs/write`.
#[derive(Debug, Clone)]
struct Written {
    path: String,
    body: Vec<u8>,
    mode: String,
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
    writes: Arc<Mutex<Vec<Written>>>,
    deletes: Arc<Mutex<Vec<String>>>,
    write_status: Arc<Mutex<Option<u16>>>,
    stale: Arc<Mutex<bool>>,
    write_delay_ms: Arc<Mutex<u64>>,
    missing_parent: Arc<Mutex<Option<String>>>,
    absent: Arc<Mutex<bool>>,
}

#[derive(Clone)]
struct State {
    fs: Fs,
    execs: Arc<Mutex<Vec<Exec>>>,
    writes: Arc<Mutex<Vec<Written>>>,
    deletes: Arc<Mutex<Vec<String>>>,
    write_status: Arc<Mutex<Option<u16>>>,
    stale: Arc<Mutex<bool>>,
    write_delay_ms: Arc<Mutex<u64>>,
    missing_parent: Arc<Mutex<Option<String>>>,
    absent: Arc<Mutex<bool>>,
    scripts: Arc<Mutex<VecDeque<Script>>>,
}

fn start_mock(scripts: Vec<Script>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let fs: Fs = Arc::new(Mutex::new(HashMap::new()));
    let execs = Arc::new(Mutex::new(Vec::new()));
    let writes = Arc::new(Mutex::new(Vec::new()));
    let deletes = Arc::new(Mutex::new(Vec::new()));
    let write_status = Arc::new(Mutex::new(None));
    let stale = Arc::new(Mutex::new(false));
    let write_delay_ms = Arc::new(Mutex::new(0));
    let missing_parent = Arc::new(Mutex::new(None));
    let absent = Arc::new(Mutex::new(false));
    let state = State {
        fs: Arc::clone(&fs),
        execs: Arc::clone(&execs),
        writes: Arc::clone(&writes),
        deletes: Arc::clone(&deletes),
        write_status: Arc::clone(&write_status),
        stale: Arc::clone(&stale),
        write_delay_ms: Arc::clone(&write_delay_ms),
        missing_parent: Arc::clone(&missing_parent),
        absent: Arc::clone(&absent),
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
        deletes,
        write_status,
        stale,
        write_delay_ms,
        missing_parent,
        absent,
    }
}

/// Make every `fs/write` take this long, to model a slow transfer.
fn slow_writes(mock: &Mock, millis: u64) {
    *mock.write_delay_ms.lock().unwrap() = millis;
}

/// Make every written `Cargo.toml` leave a stray sibling behind, the way a stale
/// checkout or a failed deletion would.
fn plant_stale_files(mock: &Mock) {
    *mock.stale.lock().unwrap() = true;
}

/// Refuse every `fs/write` with `status` — a denied transfer, not a denied exec.
fn deny_writes(mock: &Mock, status: u16) {
    *mock.write_status.lock().unwrap() = Some(status);
}

/// Answer `404` to a write under `dir` until an exec runs `mkdir`, the way the
/// fs write API behaves when the parent directory does not exist.
fn require_parent(mock: &Mock, dir: &str) {
    *mock.missing_parent.lock().unwrap() = Some(dir.to_owned());
}

/// Make the sprite itself not exist: every fs call and the sprite lookup answer `404`.
fn remove_sprite(mock: &Mock) {
    *mock.absent.lock().unwrap() = true;
}

fn script(out: Vec<Out>) -> Script {
    Script {
        handshake: Hs::Accept,
        out,
        produces: Vec::new(),
    }
}

/// The scripted reply to the runner's kill exec: the group is confirmed gone
/// (`0`), survived SIGKILL (`3`), or there is nothing to confirm (`4`).
fn kill_reply(code: i32) -> Script {
    script(vec![session_info(), Out::Exit(code)])
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
    // A fresh dispatch is `/exec?cmd=...`; a reattach is `/exec/<session>?...`.
    let route = head.target.split('?').next().unwrap_or_default();
    if head.target.contains("/exec?") || route.ends_with("/exec") || route.contains("/exec/") {
        let exec = Exec {
            target: head.target.clone(),
            authorization: head.authorization.clone(),
        };
        state.execs.lock().unwrap().push(exec);
        if params(&head.target, "cmd").iter().any(|arg| arg == "mkdir") {
            *state.missing_parent.lock().unwrap() = None;
        }
        let next = state.scripts.lock().unwrap().pop_front();
        match next {
            Some(script) => {
                // The checkout is the launcher's second positional argument.
                if let Some(checkout) = params(&head.target, "cmd").get(11) {
                    // The recipe builds with `CARGO_TARGET_DIR=<cache>/target`
                    // (the launcher's fourth argument), so `target/...` outputs
                    // appear in the cache, not in the checkout.
                    let cache = params(&head.target, "cmd").get(13).cloned();
                    for (rel, bytes) in &script.produces {
                        let place = match (&cache, rel.strip_prefix("target/")) {
                            (Some(cache), Some(_)) => format!("{cache}/{rel}"),
                            _ => format!("{checkout}/{rel}"),
                        };
                        state.fs.lock().unwrap().insert(place, bytes.to_vec());
                    }
                }
                exec_ws(stream, &head, script)
            }
            None => respond(&mut stream, 500, b""),
        }
        return;
    }
    if head.method == "GET" && !route.contains("/fs/") {
        // `GET /v1/sprites/<name>`: the sprite lookup.
        let name = route.rsplit('/').next().unwrap_or_default();
        if *state.absent.lock().unwrap() {
            respond(&mut stream, 404, b"");
        } else {
            let body = serde_json::json!({"name": name, "status": "running"}).to_string();
            respond(&mut stream, 200, body.as_bytes());
        }
        return;
    }
    if *state.absent.lock().unwrap() {
        respond(&mut stream, 404, b"");
        return;
    }
    let Some(path) = param(&head.target, "path") else {
        respond(&mut stream, 400, b"");
        return;
    };
    if head.method == "PUT" && head.target.contains("/fs/write?") {
        let delay = *state.write_delay_ms.lock().unwrap();
        if delay > 0 {
            thread::sleep(Duration::from_millis(delay));
        }
        let missing = state.missing_parent.lock().unwrap().clone();
        if let Some(dir) = missing
            && path.starts_with(&format!("{dir}/"))
        {
            respond(&mut stream, 404, b"");
            return;
        }
        let status = *state.write_status.lock().unwrap();
        if let Some(status) = status {
            respond(&mut stream, status, b"");
            return;
        }
        state.writes.lock().unwrap().push(Written {
            path: path.clone(),
            body: head.body.clone(),
            mode: param(&head.target, "mode").unwrap_or_default(),
        });
        if *state.stale.lock().unwrap()
            && let Some(dir) = path.strip_suffix("/Cargo.toml")
        {
            state
                .fs
                .lock()
                .unwrap()
                .insert(format!("{dir}/stale.txt"), b"left over".to_vec());
        }
        state.fs.lock().unwrap().insert(path, head.body.clone());
        respond(&mut stream, 200, b"{}");
        return;
    }
    if head.method == "DELETE" && head.target.contains("/fs/delete?") {
        state.deletes.lock().unwrap().push(path.clone());
        let below = format!("{path}/");
        state
            .fs
            .lock()
            .unwrap()
            .retain(|key, _| key != &path && !key.starts_with(&below));
        respond(&mut stream, 200, b"{}");
        return;
    }
    if head.method == "GET" && head.target.contains("/fs/list?") {
        let below = format!("{path}/");
        let mut entries: std::collections::BTreeMap<String, bool> = Default::default();
        for key in state.fs.lock().unwrap().keys() {
            if let Some(rest) = key.strip_prefix(&below) {
                let name = rest.split('/').next().unwrap_or(rest).to_owned();
                let is_dir = rest.contains('/');
                let entry = entries.entry(name).or_insert(is_dir);
                *entry = *entry || is_dir;
            }
        }
        let listed: Vec<serde_json::Value> = entries
            .into_iter()
            .map(|(name, is_dir)| serde_json::json!({"name": name, "isDir": is_dir, "size": 0}))
            .collect();
        let body = serde_json::json!({ "entries": listed }).to_string();
        respond(&mut stream, 200, body.as_bytes());
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
    if let Hs::Delayed(millis) = script.handshake {
        thread::sleep(Duration::from_millis(millis));
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
    // Do not drop the socket right after the last frame. The real client writes
    // a Ping on connect (and a Pong/Close later) that this mock never reads;
    // closing a TCP socket with unread inbound bytes sends RST instead of FIN,
    // and an RST discards whatever the peer has not yet read, including the
    // scripted `Exit` frame. Under load the client is slow enough to lose that
    // race: it saw EOF without an exit, classed the attempt Indeterminate, and
    // spent the 45 s kill-exec budget against a mock with no kill script left.
    // Reading until the client closes (bounded) turns the close into a clean FIN
    // that arrives after every frame.
    if socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(10)))
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
const DEADLINE: Duration = Duration::from_secs(30);

fn binding() -> ProjectBinding {
    ProjectBinding::new("proj-one", "acct", "tgtest1").unwrap()
}

/// The REAPI root `Directory` digest, built here with `thegn_cas` and no help
/// from the module under test: this is what a real `Snapshot::Pinned` carries.
fn reapi(files: &[(&str, &[u8], bool)]) -> Digest {
    fn dir(files: &[(String, &[u8], bool)]) -> Digest {
        let mut names: Vec<String> = files
            .iter()
            .map(|(path, _, _)| path.split('/').next().unwrap().to_owned())
            .collect();
        names.dedup();
        let mut entries = Vec::new();
        for name in names {
            let direct = files.iter().find(|(path, _, _)| path == &name);
            if let Some((_, bytes, executable)) = direct {
                entries.push((
                    name,
                    Entry::File {
                        digest: Digest::of(bytes),
                        executable: *executable,
                    },
                ));
                continue;
            }
            let prefix = format!("{name}/");
            let below: Vec<(String, &[u8], bool)> = files
                .iter()
                .filter_map(|(path, bytes, executable)| {
                    path.strip_prefix(&prefix)
                        .map(|rest| (rest.to_owned(), *bytes, *executable))
                })
                .collect();
            entries.push((name, Entry::Directory(dir(&below))));
        }
        Digest::of(&encode_directory(entries).unwrap())
    }
    let mut sorted: Vec<(String, &[u8], bool)> = files
        .iter()
        .map(|(path, bytes, executable)| ((*path).to_owned(), *bytes, *executable))
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    dir(&sorted)
}

/// Pins a tuple's slice type at the call, so literals of different lengths
/// can share one array.
fn f<'a>(path: &'a str, bytes: &'a [u8], executable: bool) -> (&'a str, &'a [u8], bool) {
    (path, bytes, executable)
}

const CARGO_TOML: &[u8] = b"[package]\nname = \"demo\"\n";
const MAIN_RS: &[u8] = b"fn main() { println!(\"dirty\"); }\n";

/// The pinned snapshot: two files, one of them dirty relative to `HEAD`.
fn content() -> SnapshotContent {
    let mut content = SnapshotContent::new();
    content.insert("Cargo.toml", CARGO_TOML.to_vec()).unwrap();
    content.insert("src/main.rs", MAIN_RS.to_vec()).unwrap();
    content
}

fn pinned() -> Digest {
    reapi(&[
        f("Cargo.toml", CARGO_TOML, false),
        f("src/main.rs", MAIN_RS, false),
    ])
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

fn runner_for(
    mock: &Mock,
    binding: ProjectBinding,
    store: AttemptStore,
    options: AttemptOptions,
) -> SpritesSelectedRunner {
    SpritesSelectedRunner::with_options(
        SpritesProvider::new(&mock.base, "tok", "tgtest1"),
        binding,
        store,
        Box::new(Fixed(content())),
        tokio::runtime::Handle::current(),
        options,
    )
    .unwrap()
}

fn runner(mock: &Mock, store: AttemptStore, options: AttemptOptions) -> SpritesSelectedRunner {
    runner_for(mock, binding(), store, options)
}

fn outputs_options(deadline: Duration) -> AttemptOptions {
    AttemptOptions {
        deadline,
        outputs: vec![OUTPUT.to_owned()],
        ..AttemptOptions::default()
    }
}

/// The attempt's own writes, without the one-time sprite owner marker.
fn attempt_writes(mock: &Mock) -> Vec<Written> {
    mock.writes
        .lock()
        .unwrap()
        .iter()
        .filter(|written| !written.path.ends_with("/.owner"))
        .cloned()
        .collect()
}

/// Every `/exec` the runner made, split into job dispatches and kill execs.
fn execs_of(mock: &Mock) -> (Vec<Exec>, Vec<Exec>) {
    mock.execs
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .partition(|exec| {
            params(&exec.target, "cmd").get(3).map(String::as_str) == Some("thegn-hold")
        })
}

fn fs_has_under(mock: &Mock, dir: &str) -> bool {
    let below = format!("{dir}/");
    mock.fs
        .lock()
        .unwrap()
        .keys()
        .any(|key| key == dir || key.starts_with(&below))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_attempt_materializes_the_pinned_tree_and_reports_the_exit() {
    let mut build = script(vec![
        session_info(),
        Out::Stdout("   Compiling demo v0.1.0\n"),
        Out::Stderr("warning: unused variable\n"),
        Out::Exit(0),
    ]);
    build.produces = vec![(OUTPUT, b"\x7fELF the built binary")];
    let mock = start_mock(vec![build]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();
    // The pinned identity is the REAPI root digest, not this module's own hash.
    assert_eq!(content.digest().unwrap(), pinned());

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();

    assert_eq!(receipt.provider, "sprites");
    assert_eq!(receipt.project, "proj-one");
    assert_eq!(receipt.sprite, "tgtest1");
    assert_eq!(receipt.attempt, 1);
    assert!(
        receipt
            .checkout
            .starts_with("/workspace/.thegn/jobs/proj-one/co/1-1-"),
        "{}",
        receipt.checkout
    );
    assert_eq!(receipt.source, pinned());
    assert_eq!(receipt.materialized, pinned());
    assert_eq!(receipt.completion, SelectedCompletion::Exited(0));
    assert_eq!(receipt.exit, Some(0));
    assert_eq!(receipt.signal, None);
    assert_eq!(receipt.cleanup, Cleanup::NotNeeded);
    assert!(!receipt.outputs_fenced);
    assert!(!receipt.truncated);
    assert_eq!(receipt.session.as_deref(), Some("1847"));
    assert!(
        receipt
            .hold
            .as_deref()
            .is_some_and(|hold| hold.starts_with("thegn-hold-"))
    );
    assert_eq!(receipt.files_transferred, 2);
    assert_eq!(receipt.bytes_transferred, content.total_bytes());
    assert_eq!(receipt.outputs_missing, Vec::<String>::new());
    let hashed = receipt.outputs.get(OUTPUT).expect("output was hashed");
    assert_eq!(hashed.len(), 64);
    assert!(receipt.stdout_tail.contains("Compiling demo"));
    assert_eq!(receipt.log_tail, receipt.stdout_tail);
    // No configured rate means no fabricated price.
    assert_eq!(receipt.cost_micros, None);

    // The bytes that crossed `fs/write` are exactly the pinned content, with the
    // directory modes the snapshot carries.
    let mut writes = attempt_writes(&mock);
    writes.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(writes.len(), 2);
    assert_eq!(writes[0].path, format!("{}/Cargo.toml", receipt.checkout));
    assert_eq!(writes[0].body, CARGO_TOML);
    assert_eq!(writes[0].mode, "0644");
    assert_eq!(writes[1].path, format!("{}/src/main.rs", receipt.checkout));
    assert_eq!(writes[1].body, MAIN_RS);

    // The argv is the hold-wrapped launcher around the bare recipe. The cwd, the
    // environment scrub and the kill handle live in the launcher, not in the
    // provider's `cd` wrapper or the exec request's `env`.
    let (jobs, kills) = execs_of(&mock);
    assert_eq!(jobs.len(), 1);
    assert!(kills.is_empty(), "an exited attempt needs no kill");
    assert_eq!(jobs[0].authorization, "Bearer tok");
    let cmd = params(&jobs[0].target, "cmd");
    assert_eq!(cmd.len(), 16);
    assert_eq!(cmd[0], "/bin/sh");
    assert_eq!(cmd[1], "-c");
    assert_eq!(cmd[2], HOLD_SCRIPT);
    assert_eq!(cmd[3], "thegn-hold");
    assert_eq!(cmd[4], receipt.hold.clone().unwrap());
    // The hold outlives the forwarded deadline by one hold expiry (300s).
    assert_eq!(cmd[5], "330");
    assert_eq!(cmd[6], "/bin/sh");
    assert!(cmd[8].contains("env -i"));
    assert_eq!(cmd[9], "thegn-job");
    assert!(cmd[10].starts_with("/workspace/.thegn/jobs/proj-one/ctl/1-1-"));
    assert_eq!(cmd[11], receipt.checkout);
    assert!(cmd[12].starts_with("/workspace/.thegn/jobs/proj-one/home/1-1-"));
    assert_eq!(cmd[13], binding().cache(DEFAULT_CHECKOUT_ROOT));
    assert_eq!(cmd[14], "cargo");
    assert_eq!(cmd[15], "build");
    assert!(params(&jobs[0].target, "env").is_empty());
    assert_eq!(params(&jobs[0].target, "tty"), vec!["false".to_owned()]);

    // Only the project cache outlives the attempt: the checkout, the home and
    // the control file were deleted once the outputs were hashed.
    assert!(receipt.checkout_removed);
    assert!(!fs_has_under(&mock, &receipt.checkout));
    let deletes = mock.deletes.lock().unwrap().clone();
    assert!(deletes.contains(&receipt.checkout));
    assert!(deletes.iter().any(|path| path.contains("/proj-one/home/")));
    assert!(deletes.iter().any(|path| path.contains("/proj-one/ctl/")));
    assert!(
        !deletes
            .iter()
            .any(|path| path.starts_with(&binding().cache(DEFAULT_CHECKOUT_ROOT)))
    );

    // The ledger agrees with the receipt and never reuses attempt 1.
    let record = store.read(1).unwrap().expect("attempt 1 is recorded");
    assert_eq!(record.state, AttemptState::Finished);
    assert_eq!(record.exit, Some(0));
    assert_eq!(record.cleanup, Cleanup::NotNeeded);
    assert!(record.checkout_removed);
    assert_eq!(record.epoch, 1);
    assert_eq!(record.materialized, Some(pinned()));
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
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoTest, &pinned(), &content, DEADLINE)
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
async fn a_signal_exit_is_reported_as_signalled_with_no_exit_code() {
    let mock = start_mock(vec![script(vec![session_info(), Out::Exit(137)])]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Signalled(9));
    assert_eq!(receipt.exit, None);
    assert_eq!(receipt.signal, Some(9));
    // The tree ended by itself, so nothing is killed; the outputs of a build
    // that did not finish are still fenced.
    assert_eq!(receipt.cleanup, Cleanup::NotNeeded);
    assert!(receipt.outputs_fenced);
    assert!(execs_of(&mock).1.is_empty());
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Signalled);
    assert_eq!(
        store.resume_attempt(1).unwrap(),
        Resume::Terminal(SelectedCompletion::Signalled(9))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deadline_kills_the_owned_tree_and_reports_timed_out() {
    let mock = start_mock(vec![
        script(vec![
            session_info(),
            Out::Stdout("   Compiling demo v0.1.0\n"),
            Out::Silence,
        ]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let deadline = Duration::from_millis(300);
    let runner = runner(&mock, store.clone(), outputs_options(deadline));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned(), &content, deadline)
        .await
        .unwrap();

    // A deadline stop is TimedOut, not a user cancellation.
    assert_eq!(receipt.completion, SelectedCompletion::TimedOut);
    assert_eq!(receipt.exit, None);
    assert!(receipt.outputs_fenced);
    assert!(receipt.outputs.is_empty());
    assert_eq!(receipt.stdout_tail, "   Compiling demo v0.1.0\n");

    // The runner really issued the kill: a second exec, against this attempt's
    // control file and hold, and the confirmed kill makes cleanup Done.
    let (jobs, kills) = execs_of(&mock);
    assert_eq!(jobs.len(), 1);
    assert_eq!(kills.len(), 1);
    let kill = params(&kills[0].target, "cmd");
    assert_eq!(kill[3], "thegn-job-kill");
    assert_eq!(kill[4], params(&jobs[0].target, "cmd")[10]);
    assert_eq!(kill[5], receipt.hold.clone().unwrap());
    assert_eq!(receipt.cleanup, Cleanup::Done);
    // The hold was released by that kill, so the per-attempt paths are gone too.
    assert!(receipt.checkout_removed);
    assert!(!fs_has_under(&mock, &receipt.checkout));

    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::TimedOut);
    assert_eq!(record.cleanup, Cleanup::Done);
    assert!(record.outputs_fenced);
    assert_eq!(
        store.resume_attempt(1).unwrap(),
        Resume::Terminal(SelectedCompletion::TimedOut)
    );
    // Already confirmed, so a reconciler has nothing left to mark.
    assert!(!store.mark_cleaned(1).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfirmed_kill_leaves_cleanup_pending_and_keeps_the_control_state() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Silence]),
        // The group survived SIGKILL.
        kill_reply(3),
    ]);
    let (_dir, store) = store();
    let deadline = Duration::from_millis(300);
    let runner = runner(&mock, store.clone(), outputs_options(deadline));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned(), &content, deadline)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::TimedOut);
    assert_eq!(receipt.cleanup, Cleanup::Pending);
    // The tree may still run, so its checkout and control file stay for the
    // reconciler instead of being deleted underneath it.
    assert!(!receipt.checkout_removed);
    assert!(fs_has_under(&mock, &receipt.checkout));
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.cleanup, Cleanup::Pending);
    assert!(store.mark_cleaned(1).unwrap());
    assert_eq!(store.read(1).unwrap().unwrap().cleanup, Cleanup::Done);
    assert!(!store.mark_cleaned(1).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_that_ends_without_an_exit_is_indeterminate_and_fences_outputs() {
    // The socket closes after some output and never carries an exit frame.
    let mock = start_mock(vec![
        script(vec![
            session_info(),
            Out::Stdout("   Compiling demo v0.1.0\n"),
        ]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Indeterminate);
    assert_eq!(receipt.exit, None);
    assert!(receipt.outputs_fenced);
    assert!(receipt.outputs.is_empty());
    assert_eq!(
        execs_of(&mock).1.len(),
        1,
        "a lost stream still gets a kill"
    );
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert_eq!(
        store.resume_attempt(1).unwrap(),
        Resume::Terminal(SelectedCompletion::Indeterminate)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launcher_failure_is_indeterminate_but_a_recipe_exiting_125_is_not() {
    // The launcher announces its own failure on stderr before exiting 125.
    let mock = start_mock(vec![
        script(vec![
            session_info(),
            Out::Stderr("thegn-job-launch-failed: cwd\n"),
            Out::Exit(125),
        ]),
        script(vec![session_info(), Out::Exit(125)]),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let failed = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();
    assert_eq!(failed.completion, SelectedCompletion::Indeterminate);
    assert_eq!(failed.exit, None);
    assert!(failed.outputs_fenced);
    assert!(failed.stderr_tail.contains("thegn-job-launch-failed"));

    // The same code with no marker is the recipe's own exit, reported as such.
    let genuine = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();
    assert_eq!(genuine.completion, SelectedCompletion::Exited(125));
    assert_eq!(genuine.exit, Some(125));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_handshake_is_provably_never_run_and_commits_no_spend() {
    let mock = start_mock(vec![Script {
        handshake: Hs::Status(401),
        out: Vec::new(),
        produces: Vec::new(),
    }]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let refusal = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap_err();

    let why = match refusal {
        RunnerRefusal::NotStarted(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    // The user-facing refusal names the real sprite, never the runner alias.
    assert!(why.starts_with("sprite tgtest1:"), "{why}");
    assert!(!why.contains("sprites-"), "{why}");
    assert!(why.contains("401"), "{why}");
    // A definite 4xx answered before anything was dispatched: exactly one try,
    // and the ledger records the proof (state `rejected`), which is the only
    // state that says "never ran".
    let (jobs, kills) = execs_of(&mock);
    assert_eq!(jobs.len(), 1);
    assert!(kills.is_empty());
    let record = store.read(1).unwrap().expect("the reservation is closed");
    assert_eq!(record.state, AttemptState::Rejected);
    assert_eq!(record.cleanup, Cleanup::NotNeeded);
    assert_eq!(record.committed_micros(), 0);
    assert!(record.outputs_fenced);
    assert!(record.error.unwrap_or_default().contains("401"));
    assert_eq!(store.resume_attempt(1).unwrap(), Resume::Rejected);
    assert_eq!(store.next_attempt().unwrap(), 2);
    // The transferred checkout does not linger on the sprite.
    assert!(
        !mock
            .fs
            .lock()
            .unwrap()
            .keys()
            .any(|key| key.contains("/proj-one/co/"))
    );
    assert!(
        mock.deletes
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.contains("/proj-one/co/1-1-"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_upgrade_is_never_redispatched_and_reads_back_indeterminate() {
    // The upgrade answers after the provider's single 8s try has given up. The
    // old retrying open would have sent the request again; the job open must
    // not, because the first request may already have started the build.
    let mock = start_mock(vec![
        Script {
            handshake: Hs::Delayed(12_000),
            out: vec![session_info(), Out::Exit(0)],
            produces: Vec::new(),
        },
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Indeterminate);
    assert_eq!(receipt.exit, None);
    assert!(receipt.outputs_fenced);
    let (jobs, kills) = execs_of(&mock);
    assert_eq!(jobs.len(), 1, "the build must be dispatched exactly once");
    assert_eq!(kills.len(), 1, "whatever may have started gets a kill");
    // The dispatch outcome is unknown, so the cap stays committed and the
    // record says so.
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert!(
        record
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("outcome unknown")
    );
    assert_eq!(record.committed_micros(), record.reserved_micros);
    assert_ne!(store.resume_attempt(1).unwrap(), Resume::Rejected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_digest_mismatch_is_refused_before_anything_leaves_the_host() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();
    let mut other = SnapshotContent::new();
    other
        .insert("src/main.rs", b"fn main() {}\n".to_vec())
        .unwrap();

    let refusal = runner
        .run(
            Recipe::CargoBuild,
            &other.digest().unwrap(),
            &content,
            DEADLINE,
        )
        .await
        .unwrap_err();

    match refusal {
        RunnerRefusal::SourceMismatch { expected, observed } => {
            assert_eq!(expected, other.digest().unwrap());
            assert_eq!(observed, content.digest().unwrap());
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
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let empty = SnapshotContent::new();

    let refusal = runner
        .run(
            Recipe::CargoBuild,
            &empty.digest().unwrap(),
            &empty,
            DEADLINE,
        )
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
            ..outputs_options(DEADLINE)
        },
    );
    let content = content();

    let first = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();
    assert_eq!(first.attempt, 1);
    // Unpriced, so the whole reserved cap still counts against the envelope.
    assert_eq!(store.read(1).unwrap().unwrap().committed_micros(), 600);

    let refusal = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap_err();
    let why = match refusal {
        RunnerRefusal::NotStarted(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("spend envelope exhausted"), "{why}");
    // The refused attempt never existed: no row, no socket, no checkout.
    assert!(store.read(2).unwrap().is_none());
    assert_eq!(mock.execs.lock().unwrap().len(), 1);
}

fn reserve_request(attempt: Option<u64>, source: &Digest) -> ReserveRequest<'_> {
    ReserveRequest {
        attempt,
        recipe: Recipe::CargoBuild,
        source,
        checkout_root: DEFAULT_CHECKOUT_ROOT,
        deadline_ms: 0,
        spend_cap_micros: 600,
        spend_envelope_micros: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorded_attempt_number_is_never_rerun_and_a_new_run_gets_a_fresh_checkout() {
    let one = script(vec![session_info(), Out::Exit(0)]);
    let two = script(vec![session_info(), Out::Exit(0)]);
    let mock = start_mock(vec![one, two]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();
    let source = pinned();
    let first = runner
        .run(Recipe::CargoBuild, &source, &content, DEADLINE)
        .await
        .unwrap();
    assert_eq!(first.attempt, 1);

    // A second submission gets a fresh number rather than replaying attempt 1.
    let second = runner
        .run(Recipe::CargoBuild, &source, &content, DEADLINE)
        .await
        .unwrap();
    assert_eq!(second.attempt, 2);
    assert_ne!(second.checkout, first.checkout);
    assert_eq!(execs_of(&mock).0.len(), 2);

    // Reserving a recorded number is a refusal, not a rerun.
    let refusal = store
        .reserve(&binding(), reserve_request(Some(1), &source))
        .unwrap_err();
    assert!(matches!(
        refusal,
        ReserveRefusal::Duplicate(AttemptState::Finished)
    ));
    assert_eq!(execs_of(&mock).0.len(), 2);
}

#[test]
fn back_to_back_reservations_allocate_distinct_attempt_numbers() {
    let (_dir, store) = store();
    let source = pinned();
    let first = store
        .reserve(&binding(), reserve_request(None, &source))
        .unwrap();
    let second = store
        .reserve(&binding(), reserve_request(None, &source))
        .unwrap();
    // The number is allocated inside the ledger lock (no separate
    // read-then-reserve), so reservations advance it instead of colliding on
    // `Duplicate`.
    assert_eq!(first.attempt, 1);
    assert_eq!(second.attempt, 2);
    assert_ne!(first.key, second.key);
    assert_ne!(first.checkout, second.checkout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_dispatch_reads_back_as_unknown_and_is_never_rerun() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let source = pinned();
    // Simulate a process that wrote the dispatch fence and then died.
    let mut record = store
        .reserve(&binding(), reserve_request(Some(1), &source))
        .unwrap();
    record.state = AttemptState::Dispatching;
    store.mark(&record).unwrap();

    assert_eq!(store.resume_attempt(1).unwrap(), Resume::UnknownDispatch);
    // The number stays taken, and the fence keeps counting its cap.
    assert_eq!(store.read(1).unwrap().unwrap().committed_micros(), 600);
    assert!(matches!(
        store
            .reserve(&binding(), reserve_request(Some(1), &source))
            .unwrap_err(),
        ReserveRefusal::Duplicate(AttemptState::Dispatching)
    ));
    assert_eq!(store.next_attempt().unwrap(), 2);
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebinding_a_project_marks_older_attempts_stale_and_releases_the_old_sprite() {
    let (_dir, store) = store();
    let source = pinned();
    let first = store
        .reserve(&binding(), reserve_request(Some(1), &source))
        .unwrap();
    assert_eq!(first.epoch, 1);

    // The same project, rebound to a different sprite.
    let rebound = ProjectBinding::new("proj-one", "acct", "tgtest2").unwrap();
    let second = store
        .reserve(&rebound, reserve_request(Some(2), &source))
        .unwrap();
    assert_eq!(second.epoch, 2);
    assert_eq!(store.resume_attempt(1).unwrap(), Resume::StaleEpoch);

    // The first sprite is free again for another project.
    let other_dir = store.dir().parent().unwrap().join("ledger-other");
    let other = AttemptStore::at(other_dir);
    let taken = ProjectBinding::new("proj-two", "acct", "tgtest1").unwrap();
    other
        .reserve(&taken, reserve_request(None, &source))
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_project_cannot_bind_a_sprite_the_first_one_holds() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Exit(0)]),
        script(vec![session_info(), Out::Exit(0)]),
    ]);
    // Two projects, two ledgers beside each other (one host), one sprite.
    let parent = tempfile::tempdir().unwrap();
    let first_store = AttemptStore::at(parent.path().join("proj-one"));
    let second_store = AttemptStore::at(parent.path().join("proj-two"));
    let second_binding = ProjectBinding::new("proj-two", "acct", "tgtest1").unwrap();

    let first = runner(&mock, first_store.clone(), outputs_options(DEADLINE));
    let content = content();
    first
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap();
    let writes_before = mock.writes.lock().unwrap().len();
    let execs_before = mock.execs.lock().unwrap().len();

    // Refused by the local claim registry, before any ledger row or provider
    // call exists.
    let local = runner_for(
        &mock,
        second_binding.clone(),
        second_store.clone(),
        outputs_options(DEADLINE),
    );
    let refusal = local
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap_err();
    let why = match refusal {
        RunnerRefusal::NotStarted(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("already bound to project"), "{why}");
    assert!(why.contains("proj-one"), "{why}");
    assert!(second_store.records().unwrap().is_empty());

    // A host with no claim registry in common still cannot use the sprite: the
    // owner marker on the sprite itself names the first project.
    let (_elsewhere, third_store) = store();
    let remote = runner_for(
        &mock,
        second_binding,
        third_store.clone(),
        outputs_options(DEADLINE),
    );
    let refusal = remote
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap_err();
    let why = match refusal {
        RunnerRefusal::NotStarted(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("already bound to another project"), "{why}");
    assert_eq!(
        third_store.read(1).unwrap().unwrap().state,
        AttemptState::Rejected
    );

    // Neither refused attempt wrote a byte or opened a socket on the sprite, so
    // the first project's inputs and cache were never reachable.
    assert_eq!(mock.writes.lock().unwrap().len(), writes_before);
    assert_eq!(mock.execs.lock().unwrap().len(), execs_before);
    assert!(
        mock.fs
            .lock()
            .unwrap()
            .keys()
            .all(|key| !key.contains("/proj-two/"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sync_runner_contract_forwards_the_route_deadline_and_reaches_the_provider() {
    let mock = start_mock(vec![
        script(vec![
            session_info(),
            Out::Stdout("   Compiling demo v0.1.0\n"),
            Out::Exit(0),
        ]),
        // The second submission never finishes: only the forwarded deadline can
        // stop it.
        script(vec![session_info(), Out::Silence]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store, outputs_options(DEADLINE));
    let source = pinned();

    // `submit` blocks on the runtime handle, so it must be called from a plain
    // thread, exactly how the dispatcher calls it.
    let submitted = source.clone();
    let worker = std::thread::spawn(move || {
        let wrong = runner
            .submit(
                "sprites-someone-else",
                Recipe::CargoBuild,
                &submitted,
                Duration::from_secs(30),
                &SelectedCancel::new(),
            )
            .unwrap_err();
        assert!(matches!(wrong, RunnerRefusal::WrongRunner));
        let ok = runner.submit(
            "sprites-tgtest1",
            Recipe::CargoBuild,
            &submitted,
            Duration::from_secs(30),
            &SelectedCancel::new(),
        );
        let timed_out = runner.submit(
            "sprites-tgtest1",
            Recipe::CargoTest,
            &submitted,
            Duration::from_millis(300),
            &SelectedCancel::new(),
        );
        (ok, timed_out)
    });
    let (ok, timed_out) = worker.join().unwrap();
    let result = ok.unwrap();

    assert_eq!(result.runner, "sprites-tgtest1");
    assert_eq!(result.recipe, Recipe::CargoBuild);
    assert_eq!(result.source, source);
    assert_eq!(result.completion, SelectedCompletion::Exited(0));
    assert!(result.log_tail.contains("Compiling demo"));
    assert!(!result.truncated);

    let timed_out = timed_out.unwrap();
    assert_eq!(timed_out.completion, SelectedCompletion::TimedOut);
    // The hold ceiling followed the forwarded 300ms, not the binding's 30s:
    // 0s (whole seconds) plus one hold expiry.
    let (jobs, _) = execs_of(&mock);
    assert_eq!(params(&jobs[1].target, "cmd")[5], "300");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binding_clamps_a_longer_route_deadline() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Silence]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store, outputs_options(Duration::from_millis(300)));
    let content = content();

    let receipt = runner
        .run(
            Recipe::CargoBuild,
            &pinned(),
            &content,
            Duration::from_secs(3600),
        )
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::TimedOut);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transfer_time_is_not_charged_to_the_build_deadline() {
    // Every write takes 400ms, so the transfer outlasts the 200ms build
    // deadline. The build itself exits at once: only a build clock that starts
    // at dispatch, after the transfer, reports a clean exit.
    let mock = start_mock(vec![script(vec![session_info(), Out::Exit(0)])]);
    slow_writes(&mock, 400);
    let (_dir, store) = store();
    let deadline = Duration::from_millis(200);
    let runner = runner(&mock, store, outputs_options(deadline));
    let content = content();

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned(), &content, deadline)
        .await
        .unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Exited(0));
    assert!(receipt.elapsed_ms >= 400, "{}", receipt.elapsed_ms);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_over_its_own_budget_is_rejected_and_cleaned_up() {
    let mock = start_mock(Vec::new());
    slow_writes(&mock, 1_000);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        AttemptOptions {
            transfer_budget: Duration::from_millis(100),
            ..outputs_options(DEADLINE)
        },
    );
    let content = content();

    let refusal = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap_err();

    let why = match refusal {
        RunnerRefusal::NotStarted(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("transfer exceeded"), "{why}");
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Rejected
    );
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_transfer_is_refused_before_a_command_can_run() {
    let mock = start_mock(Vec::new());
    deny_writes(&mock, 403);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let refusal = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap_err();

    let why = match refusal {
        RunnerRefusal::NotStarted(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("403"), "{why}");
    // The checkout was never complete, so no command ran, nothing is pending
    // and nothing is spent: the caller may retry as a new attempt.
    let record = store.read(1).unwrap().expect("the reservation is closed");
    assert_eq!(record.state, AttemptState::Rejected);
    assert_eq!(record.cleanup, Cleanup::NotNeeded);
    assert_eq!(record.committed_micros(), 0);
    assert!(record.outputs_fenced);
    assert!(mock.execs.lock().unwrap().is_empty());
    assert!(mock.writes.lock().unwrap().is_empty());
    assert_eq!(store.next_attempt().unwrap(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkout_holding_a_stray_file_is_never_run() {
    let mock = start_mock(Vec::new());
    plant_stale_files(&mock);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();

    let refusal = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await
        .unwrap_err();

    let why = match refusal {
        RunnerRefusal::NotStarted(why) => why,
        other => panic!("expected an unavailable runner, got {other:?}"),
    };
    assert!(why.contains("does not hold exactly"), "{why}");
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Rejected
    );
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_executable_bit_survives_the_transfer_and_is_part_of_the_pinned_identity() {
    let mock = start_mock(vec![script(vec![session_info(), Out::Exit(0)])]);
    let (_dir, store) = store();
    let runner = runner(&mock, store, outputs_options(DEADLINE));
    let mut content = SnapshotContent::new();
    content.insert("Cargo.toml", CARGO_TOML.to_vec()).unwrap();
    content
        .insert_file("scripts/build.sh", b"#!/bin/sh\n".to_vec(), true)
        .unwrap();
    let pinned_exec = reapi(&[
        f("Cargo.toml", CARGO_TOML, false),
        f("scripts/build.sh", b"#!/bin/sh\n", true),
    ]);
    let pinned_plain = reapi(&[
        f("Cargo.toml", CARGO_TOML, false),
        f("scripts/build.sh", b"#!/bin/sh\n", false),
    ]);
    assert_eq!(content.digest().unwrap(), pinned_exec);
    assert_ne!(pinned_exec, pinned_plain);

    let receipt = runner
        .run(Recipe::CargoBuild, &pinned_exec, &content, DEADLINE)
        .await
        .unwrap();
    assert_eq!(receipt.materialized, pinned_exec);

    let writes = attempt_writes(&mock);
    let mode_of = |suffix: &str| {
        writes
            .iter()
            .find(|written| written.path.ends_with(suffix))
            .map(|written| written.mode.clone())
    };
    assert_eq!(mode_of("/scripts/build.sh").as_deref(), Some("0755"));
    assert_eq!(mode_of("/Cargo.toml").as_deref(), Some("0644"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_loop_retains_the_project_cache_but_never_a_checkout() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Exit(0)]),
        script(vec![session_info(), Out::Exit(0)]),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));

    // Cold: three files, one of which the warm loop deletes.
    let mut cold = SnapshotContent::new();
    cold.insert("Cargo.toml", CARGO_TOML.to_vec()).unwrap();
    cold.insert("src/main.rs", b"fn main() {}\n".to_vec())
        .unwrap();
    cold.insert("src/old.rs", b"pub fn old() {}\n".to_vec())
        .unwrap();
    let cold_receipt = runner
        .run(Recipe::CargoBuild, &cold.digest().unwrap(), &cold, DEADLINE)
        .await
        .unwrap();
    assert_eq!(cold_receipt.files_transferred, 3);

    // Warm: the tracked entry point is dirty, one tracked file is deleted, and
    // an allowed untracked input appears beside them.
    let mut warm = SnapshotContent::new();
    warm.insert("Cargo.toml", CARGO_TOML.to_vec()).unwrap();
    warm.insert(
        "src/main.rs",
        b"fn main() { println!(\"warmer\"); }\n".to_vec(),
    )
    .unwrap();
    warm.insert("notes.md", b"allowed untracked input\n".to_vec())
        .unwrap();
    let warm_receipt = runner
        .run(Recipe::CargoTest, &warm.digest().unwrap(), &warm, DEADLINE)
        .await
        .unwrap();

    assert_eq!(warm_receipt.attempt, 2);
    assert_ne!(warm_receipt.checkout, cold_receipt.checkout);
    // A per-attempt checkout holds nothing over, so the whole edited tree
    // crosses again and is verified against the pinned root digest.
    assert_eq!(warm_receipt.files_transferred, 3);
    assert_eq!(warm_receipt.materialized, warm.digest().unwrap());
    assert_eq!(warm_receipt.completion, SelectedCompletion::Exited(0));

    // The deleted file crossed only in the cold attempt, and the dirty edit only
    // in the warm one.
    let writes = attempt_writes(&mock);
    let in_attempt = |checkout: &str, rel: &str| {
        writes
            .iter()
            .any(|written| written.path == format!("{checkout}/{rel}"))
    };
    assert!(in_attempt(&cold_receipt.checkout, "src/old.rs"));
    assert!(!in_attempt(&warm_receipt.checkout, "src/old.rs"));
    assert!(in_attempt(&warm_receipt.checkout, "notes.md"));

    // Both checkouts are gone; the project cache is the one shared path and is
    // never written or deleted by the runner itself.
    assert!(!fs_has_under(&mock, &cold_receipt.checkout));
    assert!(!fs_has_under(&mock, &warm_receipt.checkout));
    let cache = binding().cache(DEFAULT_CHECKOUT_ROOT);
    assert!(
        !mock
            .deletes
            .lock()
            .unwrap()
            .iter()
            .any(|path| path.starts_with(&cache))
    );
    let (jobs, _) = execs_of(&mock);
    assert_eq!(params(&jobs[0].target, "cmd")[13], cache);
    assert_eq!(params(&jobs[1].target, "cmd")[13], cache);
    assert_eq!(params(&jobs[1].target, "cmd")[15], "test");
}

// -------------------------------------------------------------- the snapshot

fn write_tree(root: &std::path::Path, rel: &str, bytes: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_snapshot_carries_the_dirty_tree_and_its_allowed_untracked_input() {
    let root = tempfile::tempdir().unwrap();
    let write = |rel: &str, bytes: &[u8]| write_tree(root.path(), rel, bytes);
    write("Cargo.toml", b"[package]\nname = \"demo\"\n");
    write("src/main.rs", b"fn main() {}\n");
    write("notes.md", b"an allowed untracked input\n");
    write(".gitignore", b"target/\nnode_modules/\n.envrc.local\n");
    // Ignored build output and local state never enter the runner.
    write("target/debug/app", b"\x7fELF");
    write("node_modules/dep/index.js", b"module.exports = 1;\n");
    write(".envrc.local", b"export TOKEN=1\n");
    // Credentials and VCS state are denied at the root and at every depth.
    write(".git/config", b"[core]\n");
    write(".env", b"SECRET=1\n");
    write(".env.local", b"SECRET=2\n");
    write(".ssh/id_ed25519", b"private\n");
    write("certs/server.pem", b"-----BEGIN-----\n");
    write(".envrc", b"export TOKEN=1\n");
    write(".thegn/state.json", b"{}\n");
    write("services/api/.env", b"SECRET=3\n");
    write("web/.npmrc", b"//registry:_authToken=x\n");
    write("pkg/.claude/settings.json", b"{}\n");
    write("sub/.git", b"gitdir: ../.git/modules/sub\n");
    write("ci/.cargo/credentials.toml", b"[registry]\ntoken = \"x\"\n");

    let snapshot = DirSnapshot::new(root.path());
    let captured = snapshot.capture().unwrap();
    let paths: Vec<&str> = captured.files().map(|(path, _)| path).collect();
    assert_eq!(
        paths,
        vec![".gitignore", "Cargo.toml", "notes.md", "src/main.rs"]
    );
    // The identity is the REAPI root digest of exactly that tree.
    assert_eq!(
        captured.digest().unwrap(),
        reapi(&[
            f(
                ".gitignore",
                b"target/\nnode_modules/\n.envrc.local\n",
                false
            ),
            f("Cargo.toml", b"[package]\nname = \"demo\"\n", false),
            f("notes.md", b"an allowed untracked input\n", false),
            f("src/main.rs", b"fn main() {}\n", false),
        ])
    );

    // Warm loop: a dirty edit, a deletion and an untracked addition all change
    // the digest, so the pinned identity cannot silently match a stale tree.
    let baseline = captured.digest().unwrap();
    write("src/main.rs", b"fn main() { println!(\"edited\"); }\n");
    let edited = snapshot.capture().unwrap();
    assert_ne!(edited.digest().unwrap(), baseline);

    std::fs::remove_file(root.path().join("notes.md")).unwrap();
    let deleted = snapshot.capture().unwrap();
    assert_ne!(deleted.digest().unwrap(), edited.digest().unwrap());
    assert!(deleted.get("notes.md").is_none());
    let unchanged = DirSnapshot::new(root.path()).capture().unwrap();
    assert_eq!(unchanged.digest().unwrap(), deleted.digest().unwrap());
}

#[cfg(unix)]
#[test]
fn a_snapshot_preserves_the_executable_bit_and_refuses_symlinks() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), "Cargo.toml", b"[package]\n");
    write_tree(root.path(), "scripts/build.sh", b"#!/bin/sh\n");
    let script = root.path().join("scripts/build.sh");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let captured = DirSnapshot::new(root.path()).capture().unwrap();
    assert!(captured.is_executable("scripts/build.sh"));
    assert!(!captured.is_executable("Cargo.toml"));
    assert_eq!(
        captured.digest().unwrap(),
        reapi(&[
            f("Cargo.toml", b"[package]\n", false),
            f("scripts/build.sh", b"#!/bin/sh\n", true),
        ])
    );

    // A symlink is refused outright rather than silently dropped.
    std::os::unix::fs::symlink("Cargo.toml", root.path().join("link.toml")).unwrap();
    let error = DirSnapshot::new(root.path()).capture().unwrap_err();
    assert!(format!("{error:#}").contains("symlink"), "{error:#}");
}

#[test]
fn an_oversized_file_is_refused_from_its_metadata() {
    let root = tempfile::tempdir().unwrap();
    write_tree(root.path(), "Cargo.toml", b"[package]\n");
    // A sparse file: it costs no disk, and the cap trips on its length alone.
    let big = std::fs::File::create(root.path().join("big.bin")).unwrap();
    big.set_len(SNAPSHOT_MAX_FILE_BYTES + 1).unwrap();
    let error = DirSnapshot::new(root.path()).capture().unwrap_err();
    assert!(format!("{error:#}").contains("larger than"), "{error:#}");
}

// ---------------------------------------------------- the real launch scripts

fn have(tool: &str) -> bool {
    std::process::Command::new("/bin/sh")
        .args(["-c", &format!("command -v {tool} >/dev/null 2>&1")])
        .status()
        .is_ok_and(|status| status.success())
}

fn launcher(
    pidfile: &std::path::Path,
    checkout: &std::path::Path,
    home: &std::path::Path,
    cache: &std::path::Path,
) -> std::process::Command {
    let mut command = std::process::Command::new("/bin/sh");
    command
        .args(["-c", LAUNCH_SCRIPT, "thegn-job"])
        .arg(pidfile)
        .arg(checkout)
        .arg(home)
        .arg(cache);
    command
}

/// The launcher the runner really sends, under a real `/bin/sh` with a hostile
/// parent environment. Linux only: it needs `setsid`.
#[test]
fn the_launcher_scrubs_the_environment_and_records_the_recipes_group() {
    if !cfg!(target_os = "linux") || !have("setsid") {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let checkout = work.path().join("co");
    std::fs::create_dir_all(&checkout).unwrap();
    let pidfile = work.path().join("ctl/k.pid");
    let home = work.path().join("home/k");
    let cache = work.path().join("cache");
    let dump = work.path().join("env.txt");

    let output = launcher(&pidfile, &checkout, &home, &cache)
        .args(["/bin/sh", "-c"])
        .arg(format!("env > {0}; pwd >> {0}", dump.display()))
        .env("CLAUDE_OAUTH_TOKEN", "sk-secret")
        .env("AWS_SECRET_ACCESS_KEY", "aws-secret")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let seen = std::fs::read_to_string(&dump).unwrap();
    assert!(!seen.contains("sk-secret"), "{seen}");
    assert!(!seen.contains("aws-secret"), "{seen}");
    assert!(
        seen.contains(&format!("HOME={}\n", home.display())),
        "{seen}"
    );
    assert!(
        seen.contains(&format!("CARGO_TARGET_DIR={}/target\n", cache.display())),
        "{seen}"
    );
    assert!(
        seen.contains(&format!("CARGO_HOME={}/cargo\n", cache.display())),
        "{seen}"
    );
    assert!(
        seen.contains(&format!(
            "{}=1\n",
            thegn_svc::sprites::job::SELECTED_JOB_ENV
        )),
        "the recursion marker must reach the recipe: {seen}"
    );
    assert!(
        seen.trim_end().ends_with(&checkout.display().to_string()),
        "{seen}"
    );
    let recorded = std::fs::read_to_string(&pidfile).unwrap();
    let pid = recorded.split_whitespace().next().unwrap_or_default();
    assert!(pid.parse::<u32>().is_ok(), "{recorded}");
}

#[test]
fn the_launcher_reports_its_own_failure_with_the_marker() {
    if !cfg!(target_os = "linux") || !have("setsid") {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let output = launcher(
        &work.path().join("ctl/k.pid"),
        &work.path().join("no-such-checkout"),
        &work.path().join("home"),
        &work.path().join("cache"),
    )
    .arg("/bin/true")
    .output()
    .unwrap();
    assert_eq!(output.status.code(), Some(125));
    assert!(String::from_utf8_lossy(&output.stderr).contains(LAUNCH_FAILED_MARKER));
}

#[test]
fn the_kill_script_stops_the_recorded_group_and_confirms_it() {
    if !cfg!(target_os = "linux") || !have("setsid") || !std::path::Path::new("/bin/kill").exists()
    {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let checkout = work.path().join("co");
    std::fs::create_dir_all(&checkout).unwrap();
    let pidfile = work.path().join("ctl/k.pid");
    // Run the launcher under a reaping parent, as the hold shell does in
    // production, so a killed group leader never lingers as a zombie.
    let mut child = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "\"$@\" & wait $!",
            "reaper",
            "/bin/sh",
            "-c",
            LAUNCH_SCRIPT,
            "thegn-job",
        ])
        .arg(&pidfile)
        .arg(&checkout)
        .arg(work.path().join("home"))
        .arg(work.path().join("cache"))
        // A quiet build: a shell that spawns a long sleeper of its own.
        .args(["/bin/sh", "-c", "sleep 300 & wait"])
        .spawn()
        .unwrap();
    let mut waited = 0;
    while !pidfile.exists() && waited < 100 {
        std::thread::sleep(Duration::from_millis(50));
        waited += 1;
    }
    assert!(pidfile.exists(), "the launcher never recorded its group");

    let killed = std::process::Command::new("/bin/sh")
        .args(["-c", KILL_SCRIPT, "thegn-job-kill"])
        .arg(&pidfile)
        .arg("thegn-hold-0123456789abcdef")
        .output()
        .unwrap();
    assert_eq!(
        killed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    // The whole tree is gone, so the launcher's child is reaped by a signal.
    let status = child.wait().unwrap();
    assert!(!status.success());

    // With no usable control file nothing can be confirmed.
    let nothing = std::process::Command::new("/bin/sh")
        .args(["-c", KILL_SCRIPT, "thegn-job-kill"])
        .arg(work.path().join("absent.pid"))
        .arg("thegn-hold-0123456789abcdef")
        .status()
        .unwrap();
    assert_eq!(nothing.code(), Some(4));
}

// ------------------------------------------- cancel, reconcile and reattach

/// Cancel once the attempt is recorded as running, so the cancel always lands
/// on the streaming loop and never before dispatch.
async fn cancel_when_running(store: AttemptStore, token: CancelToken) {
    for _ in 0..500 {
        let running = store
            .read(1)
            .ok()
            .flatten()
            .is_some_and(|record| record.state == AttemptState::Running);
        if running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    token.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_cancel_kills_the_owned_tree_and_reports_cancelled_not_timed_out() {
    let mock = start_mock(vec![
        script(vec![
            session_info(),
            Out::Stdout("   Compiling demo v0.1.0\n"),
            Out::Silence,
        ]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();
    let token = CancelToken::new();

    let pin = pinned();
    let (receipt, ()) = tokio::join!(
        runner.run_cancellable(Recipe::CargoBuild, &pin, &content, DEADLINE, &token),
        cancel_when_running(store.clone(), token.clone()),
    );
    let receipt = receipt.unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Cancelled);
    assert_eq!(receipt.exit, None);
    assert!(receipt.outputs_fenced);
    assert!(receipt.outputs.is_empty());
    let (jobs, kills) = execs_of(&mock);
    assert_eq!(jobs.len(), 1);
    assert_eq!(kills.len(), 1);
    let kill = params(&kills[0].target, "cmd");
    assert_eq!(kill[3], "thegn-job-kill");
    assert_eq!(kill[4], params(&jobs[0].target, "cmd")[10]);
    assert_eq!(kill[5], receipt.hold.clone().unwrap());
    assert_eq!(receipt.cleanup, Cleanup::Done);
    assert!(receipt.checkout_removed);

    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Cancelled);
    assert_eq!(record.cleanup, Cleanup::Done);
    assert!(record.outputs_fenced);
    assert_eq!(
        store.resume_attempt(1).unwrap(),
        Resume::Terminal(SelectedCompletion::Cancelled)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_whose_group_survives_the_kill_leaves_cleanup_pending() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Silence]),
        // The group survived SIGKILL.
        kill_reply(3),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();
    let token = CancelToken::new();

    let pin = pinned();
    let (receipt, ()) = tokio::join!(
        runner.run_cancellable(Recipe::CargoBuild, &pin, &content, DEADLINE, &token),
        cancel_when_running(store.clone(), token.clone()),
    );
    let receipt = receipt.unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Cancelled);
    assert_eq!(receipt.cleanup, Cleanup::Pending);
    // The tree may still run, so its directories stay for the reconciler.
    assert!(!receipt.checkout_removed);
    assert!(fs_has_under(&mock, &receipt.checkout));
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Cancelled);
    assert_eq!(record.cleanup, Cleanup::Pending);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_token_cancelled_before_dispatch_never_sends_a_command() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();
    let token = CancelToken::new();
    token.cancel();

    let refusal = runner
        .run_cancellable(Recipe::CargoBuild, &pinned(), &content, DEADLINE, &token)
        .await
        .unwrap_err();

    assert!(matches!(refusal, RunnerRefusal::NotStarted(_)));
    assert!(mock.execs.lock().unwrap().is_empty());
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Rejected
    );
}

const HOLD: &str = "thegn-hold-0123456789abcdef";

/// A ledger row as a crashed process would have left it.
fn seeded(
    store: &AttemptStore,
    attempt: u64,
    state: AttemptState,
    cleanup: Cleanup,
    deadline_ms: i64,
) -> AttemptRecord {
    let mut record = store
        .reserve(&binding(), reserve_request(Some(attempt), &pinned()))
        .unwrap();
    record.state = state;
    record.cleanup = cleanup;
    record.deadline_ms = deadline_ms;
    record.hold = Some(HOLD.to_owned());
    if state == AttemptState::Running {
        record.session = Some("1847".to_owned());
    }
    store.mark(&record).unwrap();
    record
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_reconciler_cleans_a_pending_row_and_never_reruns_an_unknown_dispatch() {
    let mock = start_mock(vec![kill_reply(0), kill_reply(0)]);
    let (_dir, store) = store();
    let timed_out = seeded(&store, 1, AttemptState::TimedOut, Cleanup::Pending, 0);
    let unknown = seeded(&store, 2, AttemptState::Dispatching, Cleanup::NotNeeded, 0);
    // Still inside its deadline: it may belong to a live run, so it is left alone.
    let live_deadline = 2_000_000_000_000;
    seeded(
        &store,
        3,
        AttemptState::Running,
        Cleanup::NotNeeded,
        live_deadline,
    );
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let touched = runner
        .reconcile_pending_at(1_000_000_000_000)
        .await
        .unwrap();

    assert_eq!(
        touched,
        vec![
            Reconciled {
                attempt: 1,
                outcome: ReconcileOutcome::Cleaned
            },
            Reconciled {
                attempt: 2,
                outcome: ReconcileOutcome::Cleaned
            },
        ]
    );
    let first = store.read(1).unwrap().unwrap();
    assert_eq!(first.state, AttemptState::TimedOut);
    assert_eq!(first.cleanup, Cleanup::Done);
    assert!(first.checkout_removed);
    // The unknown dispatch is closed as indeterminate, never as a rerun.
    let second = store.read(2).unwrap().unwrap();
    assert_eq!(second.state, AttemptState::Indeterminate);
    assert_eq!(second.cleanup, Cleanup::Done);
    assert_eq!(
        store.resume_attempt(2).unwrap(),
        Resume::Terminal(SelectedCompletion::Indeterminate)
    );
    assert_eq!(store.read(3).unwrap().unwrap().state, AttemptState::Running);

    // Only kill execs: no job was dispatched, and each kill targets the row's
    // own control file and hold.
    let (jobs, kills) = execs_of(&mock);
    assert!(jobs.is_empty());
    assert_eq!(kills.len(), 2);
    for (kill, record) in kills.iter().zip([&timed_out, &unknown]) {
        let argv = params(&kill.target, "cmd");
        assert_eq!(argv[3], "thegn-job-kill");
        assert_eq!(
            argv[4],
            binding().control_file(&record.key, DEFAULT_CHECKOUT_ROOT)
        );
        assert_eq!(argv[5], HOLD);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfirmed_reconcile_stays_pending_until_a_later_pass_confirms() {
    let mock = start_mock(vec![kill_reply(3), kill_reply(0)]);
    let (_dir, store) = store();
    seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let first = runner
        .reconcile_pending_at(1_000_000_000_000)
        .await
        .unwrap();
    assert_eq!(
        first,
        vec![Reconciled {
            attempt: 1,
            outcome: ReconcileOutcome::StillPending
        }]
    );
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert_eq!(record.cleanup, Cleanup::Pending);
    assert!(!record.checkout_removed);

    let second = runner
        .reconcile_pending_at(1_000_000_000_000)
        .await
        .unwrap();
    assert_eq!(
        second,
        vec![Reconciled {
            attempt: 1,
            outcome: ReconcileOutcome::Cleaned
        }]
    );
    assert_eq!(store.read(1).unwrap().unwrap().cleanup, Cleanup::Done);
    // Nothing is left to reconcile, and no job was ever dispatched.
    assert!(
        runner
            .reconcile_pending_at(1_000_000_000_000)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(execs_of(&mock).0.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reattaching_to_a_finished_attempt_reads_the_exit_from_the_provider() {
    // A reattach is a PTY stream, so the exit arrives as the JSON control form.
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Info(r#"{"type":"exit","exit_code":0}"#),
    ])]);
    let (_dir, store) = store();
    seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let report = runner.reattach(1).await.unwrap();

    assert!(report.reattached);
    assert_eq!(report.completion, SelectedCompletion::Exited(0));
    assert_eq!(report.cleanup, Cleanup::NotNeeded);
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Finished);
    assert_eq!(record.exit, Some(0));
    assert!(record.checkout_removed);
    // One socket, to the recorded session; no job and no kill.
    let execs = mock.execs.lock().unwrap().clone();
    assert_eq!(execs.len(), 1);
    assert!(execs[0].target.contains("/exec/1847"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_the_provider_no_longer_holds_reconciles_to_indeterminate() {
    let gone = Script {
        handshake: Hs::Status(404),
        out: Vec::new(),
        produces: Vec::new(),
    };
    let mock = start_mock(vec![gone, kill_reply(0)]);
    let (_dir, store) = store();
    seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let report = runner.reattach(1).await.unwrap();

    assert!(!report.reattached);
    assert_eq!(report.completion, SelectedCompletion::Indeterminate);
    assert_eq!(report.cleanup, Cleanup::Done);
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert!(record.outputs_fenced);
    assert!(execs_of(&mock).0.is_empty());
    assert_eq!(mock.execs.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reattaching_to_a_terminal_attempt_replays_the_ledger_without_a_socket() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let mut record = seeded(&store, 1, AttemptState::Finished, Cleanup::NotNeeded, 0);
    record.exit = Some(3);
    store.mark(&record).unwrap();
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let report = runner.reattach(1).await.unwrap();

    assert!(!report.reattached);
    assert_eq!(report.completion, SelectedCompletion::Exited(3));
    assert!(report.receipt.is_none());
    assert!(runner.reattach(9).await.is_err());
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transient_attach_error_leaves_a_running_row_untouched() {
    let flaky = Script {
        handshake: Hs::Status(503),
        out: Vec::new(),
        produces: Vec::new(),
    };
    let mock = start_mock(vec![flaky, kill_reply(0)]);
    let (_dir, store) = store();
    let before = seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let report = runner.reattach(1).await.unwrap();

    assert!(report.receipt.is_none());
    assert!(!report.reattached);
    assert!(report.note.is_some());
    assert_eq!(store.read(1).unwrap().unwrap(), before);
    assert_eq!(store.read(1).unwrap().unwrap().state, AttemptState::Running);
    // Only the attach was tried: no kill and no job.
    assert!(execs_of(&mock).0.is_empty());
    assert_eq!(mock.execs.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_inside_its_staleness_age_is_never_killed_by_reattach() {
    let mock = start_mock(vec![kill_reply(0)]);
    let (_dir, store) = store();
    let live_deadline = 4_000_000_000_000;
    let before = seeded(
        &store,
        1,
        AttemptState::Dispatching,
        Cleanup::NotNeeded,
        live_deadline,
    );
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let report = runner.reattach(1).await.unwrap();

    assert!(report.note.is_some());
    assert_eq!(store.read(1).unwrap().unwrap(), before);
    assert!(mock.execs.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_dispatching_row_with_no_control_file_settles_done() {
    // Kill exit 4: no pidfile, so the launcher never ran and nothing is left.
    let mock = start_mock(vec![kill_reply(4)]);
    let (_dir, store) = store();
    seeded(&store, 1, AttemptState::Dispatching, Cleanup::NotNeeded, 0);
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let touched = runner
        .reconcile_pending_at(1_000_000_000_000)
        .await
        .unwrap();

    assert_eq!(
        touched,
        vec![Reconciled {
            attempt: 1,
            outcome: ReconcileOutcome::Cleaned
        }]
    );
    let record = store.read(1).unwrap().unwrap();
    assert_eq!(record.state, AttemptState::Indeterminate);
    assert_eq!(record.cleanup, Cleanup::Done);
}

#[test]
fn a_recovery_write_is_skipped_when_the_row_changed_since_the_scan() {
    let (_dir, store) = store();
    let scanned = seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);

    // The owner moves the row on after the recovery pass scanned it.
    let mut owner = scanned.clone();
    owner.state = AttemptState::Finished;
    owner.exit = Some(0);
    store.mark(&owner).unwrap();

    let mut recovery = scanned.clone();
    recovery.state = AttemptState::Indeterminate;
    assert!(!store.mark_if_unchanged(&scanned, &recovery).unwrap());
    assert_eq!(store.read(1).unwrap().unwrap(), owner);

    // Against the row as it now stands, the write lands.
    assert!(store.mark_if_unchanged(&owner, &recovery).unwrap());
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Indeterminate
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_racing_a_finished_exit_never_kills_a_proven_completion() {
    // The exit is queued right behind the session handle; the cancel lands as
    // soon as the attempt is dispatched. Whichever the loop sees first, a proven
    // exit must not be reported as cancelled with a kill spent on it.
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Stdout("done\n"), Out::Exit(0)]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let content = content();
    let token = CancelToken::new();

    let canceller = {
        let store = store.clone();
        let token = token.clone();
        async move {
            for _ in 0..5000 {
                let seen = store.read(1).ok().flatten().is_some_and(|record| {
                    matches!(record.state, AttemptState::Running | AttemptState::Finished)
                });
                if seen {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            token.cancel();
        }
    };
    let pin = pinned();
    let (receipt, ()) = tokio::join!(
        runner.run_cancellable(Recipe::CargoBuild, &pin, &content, DEADLINE, &token),
        canceller,
    );
    let receipt = receipt.unwrap();

    let (_, kills) = execs_of(&mock);
    match receipt.completion {
        SelectedCompletion::Exited(0) => assert!(kills.is_empty()),
        SelectedCompletion::Cancelled => assert_eq!(kills.len(), 1),
        other => panic!("unexpected completion {other:?}"),
    }
}

/// A lease whose owner is gone: a pid no process has, an old heartbeat.
fn dead_lease() -> OwnerLease {
    OwnerLease {
        pid: u32::MAX - 1,
        boot_id: OwnerLease::current(0).boot_id,
        start_ticks: None,
        started_at_ms: 0,
        heartbeat_ms: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_with_a_live_owner_lease_is_never_reconciled_even_past_the_staleness_margin() {
    let mock = start_mock(vec![]);
    let (_dir, store) = store();
    let mut record = seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    record.lease = Some(OwnerLease::current(thegn_core::util::now_ms()));
    store.mark(&record).unwrap();
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    // A realistic clock: ten seconds after the heartbeat, inside the window.
    let touched = runner
        .reconcile_pending_at(thegn_core::util::now_ms() + 10_000)
        .await
        .unwrap();

    assert!(touched.is_empty());
    assert_eq!(store.read(1).unwrap().unwrap(), record);
    assert!(mock.execs.lock().unwrap().is_empty());

    // The heartbeat is old, but this very process still owns the row: on Linux
    // the pid and boot id alone keep it alive.
    if cfg!(target_os = "linux") {
        record.lease = Some(OwnerLease {
            heartbeat_ms: 0,
            ..OwnerLease::current(0)
        });
        store.mark(&record).unwrap();
        let touched = runner
            .reconcile_pending_at(1_000_000_000_000)
            .await
            .unwrap();
        assert!(touched.is_empty());
        assert_eq!(store.read(1).unwrap().unwrap(), record);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_row_whose_lease_owner_is_dead_and_heartbeat_old_is_reconciled() {
    let mock = start_mock(vec![kill_reply(0)]);
    let (_dir, store) = store();
    let mut record = seeded(&store, 1, AttemptState::Dispatching, Cleanup::NotNeeded, 0);
    record.lease = Some(dead_lease());
    store.mark(&record).unwrap();
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let touched = runner
        .reconcile_pending_at(1_000_000_000_000)
        .await
        .unwrap();

    assert_eq!(
        touched,
        vec![Reconciled {
            attempt: 1,
            outcome: ReconcileOutcome::Cleaned
        }]
    );
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Indeterminate
    );
}

#[test]
fn a_lease_heartbeat_loses_to_a_concurrent_recovery_write_and_the_owner_fences() {
    let (_dir, store) = store();
    let mut owner = seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    owner.lease = Some(OwnerLease::current(1_000));
    store.mark(&owner).unwrap();

    // While nothing intervenes the heartbeat moves and the held row follows it.
    assert!(store.refresh_lease(&mut owner, 2_000).unwrap());
    assert_eq!(owner.lease.as_ref().unwrap().heartbeat_ms, 2_000);
    assert_eq!(store.read(1).unwrap().unwrap(), owner);

    // A recovery settles the row between two heartbeats.
    let mut recovery = owner.clone();
    recovery.state = AttemptState::Indeterminate;
    assert!(store.mark_if_unchanged(&owner, &recovery).unwrap());

    // The next heartbeat is refused, writes nothing, and leaves the held row alone.
    let held = owner.clone();
    assert!(!store.refresh_lease(&mut owner, 3_000).unwrap());
    assert_eq!(owner, held);
    assert_eq!(store.read(1).unwrap().unwrap(), recovery);
}

#[test]
fn a_lease_is_live_on_a_fresh_heartbeat_and_dead_with_a_gone_owner() {
    let now = 1_000_000;
    assert!(OwnerLease::current(now).is_live(now));
    assert!(OwnerLease::current(now).is_live(now + 29_000));
    assert!(!dead_lease().is_live(now));
    // A little skew ahead is tolerated; a heartbeat far in the future is not
    // proof of anything (the dead owner's pid fallback decides).
    assert!(OwnerLease::current(now + 2_000).is_live(now));
    let far = OwnerLease {
        heartbeat_ms: now + 3_600_000,
        ..dead_lease()
    };
    assert!(!far.is_live(now));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reattach_leaves_a_live_leased_row_untouched_before_any_provider_call() {
    let mock = start_mock(vec![
        script(vec![
            session_info(),
            Out::Info(r#"{"type":"exit","exit_code":0}"#),
        ]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let mut record = seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    record.lease = Some(OwnerLease::current(thegn_core::util::now_ms()));
    store.mark(&record).unwrap();
    let runner = runner(&mock, store.clone(), AttemptOptions::default());

    let report = runner.reattach(1).await.unwrap();

    assert!(!report.reattached);
    assert!(report.note.is_some());
    assert_eq!(store.read(1).unwrap().unwrap(), record);
    assert!(
        mock.execs.lock().unwrap().is_empty(),
        "no attach and no kill"
    );
}

/// Once the owner's heartbeat has advanced the stored lease, play a recovery
/// pass that settles the row (a terminal row with no lease) and return it.
async fn recover_after_heartbeat(store: AttemptStore) -> AttemptRecord {
    for _ in 0..500 {
        let beaten = store.read(1).ok().flatten().is_some_and(|record| {
            record.state == AttemptState::Running
                && record
                    .lease
                    .as_ref()
                    .is_some_and(|lease| lease.heartbeat_ms > lease.started_at_ms)
        });
        if beaten {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut row = store.read(1).unwrap().unwrap();
    row.state = AttemptState::Indeterminate;
    row.cleanup = Cleanup::Done;
    row.lease = None;
    store.mark(&row).unwrap();
    row
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_heartbeats_then_fences_when_a_recovery_settles_its_row() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Silence]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(
        &mock,
        store.clone(),
        AttemptOptions {
            lease_heartbeat: Duration::from_millis(50),
            ..outputs_options(DEADLINE)
        },
    );
    let content = content();
    let pin = pinned();

    let (receipt, settled) = tokio::join!(
        runner.run(Recipe::CargoBuild, &pin, &content, DEADLINE),
        recover_after_heartbeat(store.clone()),
    );
    let receipt = receipt.unwrap();

    assert_eq!(receipt.completion, SelectedCompletion::Indeterminate);
    assert!(receipt.outputs_fenced);
    // The fenced owner never overwrote what the recovery settled.
    assert_eq!(store.read(1).unwrap().unwrap(), settled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_selected_cancel_through_submit_reads_back_cancelled_and_sends_nothing() {
    let mock = start_mock(Vec::new());
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let source = pinned();
    let cancel = SelectedCancel::new();
    cancel.cancel();

    let worker = std::thread::spawn(move || {
        runner.submit(
            "sprites-tgtest1",
            Recipe::CargoBuild,
            &source,
            Duration::from_secs(30),
            &cancel,
        )
    });
    let result = worker.join().unwrap().unwrap();

    assert_eq!(result.completion, SelectedCompletion::Cancelled);
    assert!(mock.execs.lock().unwrap().is_empty());
    // The route short-circuits before reserving, so no attempt row exists.
    assert!(store.read(1).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_selected_cancel_from_another_thread_kills_the_running_command_through_submit() {
    let mock = start_mock(vec![
        script(vec![session_info(), Out::Silence]),
        kill_reply(0),
    ]);
    let (_dir, store) = store();
    let runner = runner(&mock, store.clone(), outputs_options(DEADLINE));
    let source = pinned();
    let cancel = SelectedCancel::new();

    let submitted = cancel.clone();
    let worker = std::thread::spawn(move || {
        runner.submit(
            "sprites-tgtest1",
            Recipe::CargoBuild,
            &source,
            Duration::from_secs(30),
            &submitted,
        )
    });
    for _ in 0..500 {
        let running = store
            .read(1)
            .ok()
            .flatten()
            .is_some_and(|record| record.state == AttemptState::Running);
        if running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cancel.cancel();
    let result = worker.join().unwrap().unwrap();

    assert_eq!(result.completion, SelectedCompletion::Cancelled);
    let (jobs, kills) = execs_of(&mock);
    assert_eq!(jobs.len(), 1);
    assert_eq!(kills.len(), 1);
    assert_eq!(
        store.read(1).unwrap().unwrap().state,
        AttemptState::Cancelled
    );
}

#[test]
fn a_row_without_a_lease_field_still_reads_and_stays_without_one() {
    let (_dir, store) = store();
    let record = seeded(&store, 1, AttemptState::Running, Cleanup::NotNeeded, 0);
    let json = serde_json::to_string(&record).unwrap();
    assert!(!json.contains("lease"), "{json}");
    let back: AttemptRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back.lease, None);
    assert_eq!(store.read(1).unwrap().unwrap(), record);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_sprite_gets_its_marker_directory_made_before_the_owner_marker() {
    // The first exec is the `mkdir -p`; the attempt itself then has no script
    // and is refused after the transfer, which is all this test needs.
    let mock = start_mock(vec![script(vec![session_info(), Out::Exit(0)])]);
    require_parent(&mock, DEFAULT_CHECKOUT_ROOT);
    let (_dir, store) = store();
    let runner = runner(&mock, store, outputs_options(DEADLINE));
    let content = content();

    let _outcome = runner
        .run(Recipe::CargoBuild, &pinned(), &content, DEADLINE)
        .await;

    let execs = mock.execs.lock().unwrap().clone();
    let make = params(&execs[0].target, "cmd");
    assert_eq!(make[..3], ["mkdir", "-p", "--"], "{make:?}");
    assert_eq!(make[3], DEFAULT_CHECKOUT_ROOT);
    let marker = ProjectBinding::owner_marker(DEFAULT_CHECKOUT_ROOT);
    assert_eq!(
        mock.fs.lock().unwrap().get(&marker).map(Vec::as_slice),
        Some(binding().project().as_bytes())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_sprite_is_named_plainly_not_as_a_404_chain() {
    let mock = start_mock(Vec::new());
    remove_sprite(&mock);
    let (_dir, store) = store();
    let runner = runner(&mock, store, outputs_options(DEADLINE));

    let outcome = runner
        .run(Recipe::CargoBuild, &pinned(), &content(), DEADLINE)
        .await;

    let text = format!("{outcome:?}");
    assert!(text.contains("does not exist"), "{text}");
    assert!(text.contains("tgtest1"), "{text}");
    assert!(text.contains("thegn env up"), "{text}");
    assert!(!text.contains("404"), "{text}");
    assert!(!text.contains("sprites-"), "{text}");
}

/// The launcher keeps the stock toolchain resolvable: `$CARGO_HOME/bin` holds
/// links to the caller's real cargo home binaries.
#[test]
fn the_launcher_links_the_real_toolchain_into_the_job_cargo_home() {
    if !cfg!(target_os = "linux") || !have("setsid") {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let checkout = work.path().join("co");
    std::fs::create_dir_all(&checkout).unwrap();
    let real = work.path().join("realcargo");
    std::fs::create_dir_all(real.join("bin")).unwrap();
    std::fs::write(real.join("bin/cargo"), "#!/bin/sh\n").unwrap();
    let cache = work.path().join("cache");
    let output = launcher(
        &work.path().join("ctl/k.pid"),
        &checkout,
        &work.path().join("home/k"),
        &cache,
    )
    .args(["/bin/sh", "-c", "true"])
    .env("CARGO_HOME", &real)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::symlink_metadata(cache.join("cargo/bin/cargo")).is_ok(),
        "cargo must resolve under the job CARGO_HOME"
    );
}
