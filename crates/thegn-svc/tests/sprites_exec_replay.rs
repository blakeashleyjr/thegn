//! Loopback WebSocket replays for the Sprites native exec stream.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use thegn_svc::provider::{ExecFrame, ExecProtocolError, ExecSpec, SpritesProvider};

#[derive(Clone, Copy)]
enum Hs {
    Accept,
    Status(u16),
}

enum Out {
    Text(&'static str),
    Bin(Vec<u8>),
    WaitForStdinEof,
    Close,
}

struct Script {
    handshake: Hs,
    out: Vec<Out>,
}

#[derive(Debug, Clone)]
struct Request {
    target: String,
    authorization: String,
    websocket_key: String,
}

struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

fn start_mock(scripts: Vec<Script>) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let scripts_for_thread = Arc::clone(&scripts);
    let requests_for_thread = Arc::clone(&requests);

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let Some(script) = scripts_for_thread.lock().unwrap().pop_front() else {
                continue;
            };
            let requests = Arc::clone(&requests_for_thread);
            thread::spawn(move || handle_connection(stream, script, requests));
        }
    });

    Mock {
        base: format!("http://127.0.0.1:{port}/v1"),
        requests,
    }
}

fn provider(mock: &Mock) -> SpritesProvider {
    SpritesProvider::new(&mock.base, "tok", "tgtest1")
}

fn tty_spec() -> ExecSpec {
    ExecSpec {
        argv: vec!["sh".into()],
        tty: true,
        cols: 80,
        rows: 24,
        env: Vec::new(),
        cwd: None,
    }
}

fn script(out: Vec<Out>) -> Script {
    Script {
        handshake: Hs::Accept,
        out,
    }
}

fn session_info() -> Out {
    Out::Text(
        r#"{"type":"session_info","session_id":"1847","command":"sh","created":1767609000,"cols":80,"rows":24,"is_owner":true,"tty":true}"#,
    )
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
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

    let request = String::from_utf8_lossy(&bytes);
    let mut lines = request.split("\r\n");
    let target = lines.next()?.split_whitespace().nth(1)?.to_string();
    let authorization = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_default();
    let websocket_key = request
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_string())
        })
        .unwrap_or_default();
    Some(Request {
        target,
        authorization,
        websocket_key,
    })
}

fn handle_connection(mut stream: TcpStream, script: Script, requests: Arc<Mutex<Vec<Request>>>) {
    let Some(request) = read_request(&mut stream) else {
        return;
    };
    requests.lock().unwrap().push(request.clone());

    match script.handshake {
        Hs::Status(code) => {
            let response =
                format!("HTTP/1.1 {code} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            if stream.write_all(response.as_bytes()).is_err() || stream.flush().is_err() {
                return;
            }
            return;
        }
        Hs::Accept => {}
    }

    if request.websocket_key.is_empty() {
        return;
    }
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
        request.websocket_key.as_bytes(),
    );
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }

    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::protocol::{Role, WebSocket};

    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    for output in script.out {
        if matches!(&output, Out::WaitForStdinEof) {
            loop {
                match socket.read() {
                    Ok(Message::Binary(bytes)) if bytes.as_ref() == [4] => break,
                    Ok(Message::Close(_)) | Err(_) => return,
                    Ok(_) => {}
                }
            }
            continue;
        }
        let message = match output {
            Out::Text(text) => Message::Text(text.into()),
            Out::Bin(bytes) => Message::Binary(bytes.into()),
            Out::WaitForStdinEof => unreachable!("handled before message construction"),
            Out::Close => Message::Close(None),
        };
        if socket.send(message).is_err() {
            return;
        }
    }

    // Drain what the client sends until it closes. Closing a socket that still
    // holds unread received data makes Linux answer with an RST instead of a
    // FIN and can discard the frames just written, so the scripted exit frame
    // used to be lost to that race. Reading to the close handshake (the client
    // answers our Close, or drops after an exit frame) settles it first; the
    // read timeout bounds a client that keeps the stream open.
    if socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(2)))
        .is_err()
    {
        return;
    }
    while socket.read().is_ok() {}
}

fn requests(mock: &Mock) -> Vec<Request> {
    mock.requests.lock().unwrap().clone()
}

#[tokio::test]
async fn tty_replay_matches_the_live_trace() {
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Text(r#"{"type":"port_opened","port":8080,"address":"0.0.0.0","pid":1}"#),
        Out::Bin(b"\x1b[>4;2m/dev/pts/0".to_vec()),
        Out::Bin(b"ok\r\n".to_vec()),
        Out::Text(r#"{"type":"exit","exit_code":0}"#),
    ])]);
    let provider = provider(&mock);

    let mut session = provider.open_exec("tgtest1", &tty_spec()).await.unwrap();
    let mut frames = Vec::new();
    while let Some(frame) = session.frames.recv().await {
        let done = matches!(frame, ExecFrame::Exit(_));
        frames.push(frame);
        if done {
            break;
        }
    }

    let stdout: Vec<u8> = frames
        .iter()
        .filter_map(|frame| match frame {
            ExecFrame::Stdout(bytes) => Some(bytes.as_slice()),
            ExecFrame::Exit(_) => None,
        })
        .flatten()
        .copied()
        .collect();
    assert_eq!(stdout, b"\x1b[>4;2m/dev/pts/0ok\r\n");
    assert_eq!(
        frames,
        vec![
            ExecFrame::Stdout(b"\x1b[>4;2m/dev/pts/0".to_vec()),
            ExecFrame::Stdout(b"ok\r\n".to_vec()),
            ExecFrame::Exit(0),
        ]
    );
    assert_eq!(*session.session_id.borrow(), Some("1847".to_string()));

    let recorded = requests(&mock);
    assert_eq!(recorded.len(), 1);
    assert!(recorded[0].target.starts_with("/v1/sprites/tgtest1/exec?"));
    assert!(recorded[0].target.contains("tty=true"));
    assert!(recorded[0].target.contains("cmd="));
    assert_eq!(recorded[0].authorization, "Bearer tok");
}

#[tokio::test]
async fn tty_replay_preserves_a_newline_split_across_sprite_frames() {
    // Synthetic `tty && echo ok` output: Sprite may put the PTY's CRLF in its
    // own text frame between binary output chunks. Keep this separate from the
    // live symptom replay above, whose received bytes already lack that CRLF.
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Bin(b"\x1b[>4;2m/dev/pts/0".to_vec()),
        Out::Text("\r\n"),
        Out::Bin(b"ok\r\n".to_vec()),
        Out::Text(r#"{"type":"exit","exit_code":0}"#),
    ])]);
    let mut session = provider(&mock)
        .open_exec("tgtest1", &tty_spec())
        .await
        .unwrap();

    let mut output = Vec::new();
    let mut trace = Vec::new();
    while let Some(frame) = session.frames.recv().await {
        match frame {
            ExecFrame::Stdout(bytes) => {
                output.extend_from_slice(&bytes);
                trace.push(bytes);
            }
            ExecFrame::Exit(code) => {
                assert_eq!(code, 0);
                break;
            }
        }
    }

    assert_eq!(
        trace,
        vec![
            b"\x1b[>4;2m/dev/pts/0".to_vec(),
            b"\r\n".to_vec(),
            b"ok\r\n".to_vec(),
        ]
    );
    assert_eq!(output, b"\x1b[>4;2m/dev/pts/0\r\nok\r\n");
}

#[tokio::test]
async fn tty_forwards_plain_text_frames_as_output() {
    let mock = start_mock(vec![script(vec![
        Out::Text("hello"),
        Out::Text(r#"{"type":"exit","exit_code":0}"#),
    ])]);
    let mut session = provider(&mock)
        .open_exec("tgtest1", &tty_spec())
        .await
        .unwrap();

    assert_eq!(
        session.frames.recv().await,
        Some(ExecFrame::Stdout(b"hello".to_vec()))
    );
    assert_eq!(session.frames.recv().await, Some(ExecFrame::Exit(0)));
}

#[tokio::test]
async fn non_tty_replay_preserves_exit_after_stdout_stderr_and_normal_close() {
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Bin([vec![1], b"out\n".to_vec()].concat()),
        Out::Bin([vec![2], b"warn\n".to_vec()].concat()),
        Out::Bin(vec![3, 7]),
        Out::Close,
    ])]);

    let (code, output) = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap();
    assert_eq!(code, 7);
    assert_eq!(output, "out\n\n[stderr, last 5 bytes]\nwarn\n");
    assert_eq!(
        requests(&mock).len(),
        1,
        "replay must not retry the command"
    );
}

#[tokio::test]
async fn non_tty_streaming_owner_drop_half_closes_and_preserves_report() {
    let mock = start_mock(vec![script(vec![
        Out::Text(
            r#"{"type":"session_info","session_id":"1847","command":"sh","created":1767609000,"cols":0,"rows":0,"is_owner":true,"tty":false}"#,
        ),
        Out::WaitForStdinEof,
        Out::Bin([vec![1], b"out\n".to_vec()].concat()),
        Out::Bin([vec![2], b"warn\n".to_vec()].concat()),
        Out::Bin(vec![3, 7]),
        Out::Close,
    ])]);

    let spec = ExecSpec {
        argv: vec!["sh".into()],
        tty: false,
        cols: 0,
        rows: 0,
        env: Vec::new(),
        cwd: None,
    };
    let streaming = provider(&mock)
        .open_exec_streaming("tgtest1", &spec)
        .await
        .unwrap();
    let thegn_svc::provider::StreamingExec {
        session,
        stderr,
        verdict,
    } = streaming;
    let thegn_svc::provider::ExecSession {
        mut frames,
        control,
        session_id,
    } = session;
    drop(control);
    drop(session_id);

    let mut received = Vec::new();
    while let Some(frame) = frames.recv().await {
        received.push(frame);
    }
    assert_eq!(
        received,
        vec![ExecFrame::Stdout(b"out\n".to_vec()), ExecFrame::Exit(7)]
    );
    assert_eq!(stderr.total(), 5);
    assert_eq!(stderr.render(), "warn\n");
    assert_eq!(verdict.await.unwrap(), None);
    assert_eq!(
        requests(&mock).len(),
        1,
        "streaming must not retry the command"
    );
}

#[tokio::test]
async fn non_tty_streaming_owner_drop_preserves_typed_protocol_report() {
    let mock = start_mock(vec![script(vec![
        Out::WaitForStdinEof,
        Out::Bin(vec![3, 7]),
        Out::Bin(vec![3, 8]),
        Out::Close,
    ])]);
    let spec = ExecSpec {
        argv: vec!["sh".into()],
        tty: false,
        cols: 0,
        rows: 0,
        env: Vec::new(),
        cwd: None,
    };
    let streaming = provider(&mock)
        .open_exec_streaming("tgtest1", &spec)
        .await
        .unwrap();
    let thegn_svc::provider::StreamingExec {
        session, verdict, ..
    } = streaming;
    let thegn_svc::provider::ExecSession {
        mut frames,
        control,
        ..
    } = session;
    drop(control);

    assert_eq!(frames.recv().await, Some(ExecFrame::Exit(7)));
    assert_eq!(frames.recv().await, None);
    assert_eq!(
        verdict.await.unwrap(),
        Some(ExecProtocolError::DuplicateExit)
    );
}

#[tokio::test]
async fn non_tty_nonzero_exit_appends_the_stderr_tail() {
    let mock = start_mock(vec![script(vec![
        Out::Bin([vec![1], b"o".to_vec()].concat()),
        Out::Bin([vec![2], b"boom".to_vec()].concat()),
        Out::Bin(vec![3, 7]),
    ])]);

    let (code, output) = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap();
    assert_eq!(code, 7);
    assert!(output.contains("[stderr, last 4 bytes]"));
    assert!(output.contains("boom"));
}

#[tokio::test]
async fn non_tty_duplicate_exit_is_a_typed_protocol_error() {
    let mock = start_mock(vec![script(vec![
        Out::Bin(vec![3, 0]),
        Out::Bin(vec![3, 1]),
        Out::Close,
    ])]);

    let error = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ExecProtocolError>(),
        Some(&ExecProtocolError::DuplicateExit)
    );
}

#[tokio::test]
async fn non_tty_exit_repeated_in_both_forms_is_accepted() {
    let mock = start_mock(vec![script(vec![
        Out::Bin(vec![1, b'o', b'k']),
        Out::Bin(vec![3, 0]),
        Out::Text(r#"{"type":"exit","exit_code":0}"#),
        Out::Close,
    ])]);

    let result = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap();
    assert_eq!(result, (0, "ok".to_string()));
}

#[tokio::test]
async fn non_tty_output_after_exit_is_a_typed_protocol_error() {
    let mock = start_mock(vec![script(vec![
        Out::Bin(vec![3, 0]),
        Out::Bin(vec![1, b'x']),
        Out::Close,
    ])]);

    let error = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ExecProtocolError>(),
        Some(&ExecProtocolError::OutputAfterExit)
    );
}

#[tokio::test]
async fn non_tty_unknown_stream_is_a_typed_protocol_error() {
    let mock = start_mock(vec![script(vec![Out::Bin(vec![5, b'x']), Out::Close])]);

    let error = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ExecProtocolError>(),
        Some(&ExecProtocolError::UnknownStream(5))
    );
}

#[tokio::test]
async fn non_tty_malformed_exit_is_a_typed_protocol_error() {
    let mock = start_mock(vec![script(vec![Out::Bin(vec![3]), Out::Close])]);

    let error = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<ExecProtocolError>(),
        Some(&ExecProtocolError::MalformedExit)
    );
}

#[tokio::test]
async fn non_tty_coalesced_output_is_kept_whole() {
    let mock = start_mock(vec![script(vec![
        Out::Bin([vec![1], b"a\nb".to_vec()].concat()),
        Out::Bin([vec![1], b"c".to_vec()].concat()),
        Out::Bin(vec![3, 0]),
        Out::Close,
    ])]);

    let result = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap();
    assert_eq!(result, (0, "a\nbc".to_string()));
}

#[tokio::test]
async fn non_tty_text_exit_ends_the_run() {
    let mock = start_mock(vec![script(vec![
        session_info(),
        Out::Bin([vec![1], b"x".to_vec()].concat()),
        Out::Bin([vec![2], b"e".to_vec()].concat()),
        Out::Text(r#"{"type":"exit","exit_code":3}"#),
    ])]);

    let (code, output) = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap();
    assert_eq!(code, 3);
    assert!(
        output.starts_with("x\n[stderr, last 1 bytes]\n"),
        "{output:?}"
    );
    assert!(output.ends_with('e'), "{output:?}");
}

#[tokio::test]
async fn close_without_exit_is_an_error() {
    let mock = start_mock(vec![script(vec![
        Out::Bin([vec![1], b"x".to_vec()].concat()),
        Out::Close,
    ])]);

    let error = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exec stream ended without an exit status")
    );
}

#[tokio::test]
async fn cold_open_retries_until_the_endpoint_answers() {
    let mock = start_mock(vec![
        Script {
            handshake: Hs::Status(503),
            out: Vec::new(),
        },
        script(vec![Out::Bin(vec![3, 0])]),
    ]);
    let started = Instant::now();
    let result = provider(&mock)
        .run_exec("tgtest1", &["sh".into()], None, &[])
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(2),
        "retry was too fast: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "retry took too long: {elapsed:?}"
    );
    assert_eq!(result.0, 0);
    assert_eq!(requests(&mock).len(), 2);
}

#[tokio::test]
async fn auth_rejection_is_not_retried() {
    let mock = start_mock(vec![Script {
        handshake: Hs::Status(401),
        out: Vec::new(),
    }]);
    let started = Instant::now();
    let error = match provider(&mock).open_exec("tgtest1", &tty_spec()).await {
        Ok(_) => panic!("auth rejection unexpectedly succeeded"),
        Err(error) => error,
    };
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(requests(&mock).len(), 1);
    assert!(format!("{error:#}").contains("401"));
}

#[tokio::test]
async fn reattach_does_not_retry_a_cold_endpoint() {
    let mock = start_mock(vec![Script {
        handshake: Hs::Status(503),
        out: Vec::new(),
    }]);
    let error = match provider(&mock).attach_exec("tgtest1", "1847", 80, 24).await {
        Ok(_) => panic!("cold reattach unexpectedly succeeded"),
        Err(error) => error,
    };

    assert!(format!("{error:#}").contains("503"));
    let recorded = requests(&mock);
    assert_eq!(recorded.len(), 1);
    assert!(
        recorded[0]
            .target
            .starts_with("/v1/sprites/tgtest1/exec/1847")
    );
}
