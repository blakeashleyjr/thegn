//! The `/interactive` WebSocket PTY for a Vercel sandbox (in-process callers:
//! the `vercel-exec` bridge). Native panes stay off: `exec_api` is false.
//!
//! Flow: resolve the session (resuming under [`super::Wake::Allow`]), mint the
//! `/interactive` ticket, wait for the sandbox's `/health`, connect the socket
//! with the ticket token in the query, send the start frame, then pump frames
//! in a driver task. The ticket token never reaches a log or an error string:
//! every connect error is reduced to a fixed message plus an HTTP status.

use std::time::Duration;

use anyhow::{Result, anyhow};
use thegn_core::vercel::admit::valid_sandbox_name;
use thegn_core::vercel::api::{WsControl, interactive_start_frame, parse_ws_control, resize_frame};

use super::VercelProvider;
use crate::provider::{
    EXEC_EXIT_STATUS_UNKNOWN, ExecControl, ExecFrame, ExecSession, ExecSpec, exec_ws_config,
};

/// Largest output item queued at once (provider.rs `EXEC_DATA_CHUNK`).
pub(super) const DATA_CHUNK: usize = 32 << 10;
/// Item capacity of the output channel (provider.rs `EXEC_CHANNEL_CAPACITY`).
pub(super) const CHANNEL_CAPACITY: usize = 256;
const _: () = assert!(DATA_CHUNK * CHANNEL_CAPACITY <= 8 << 20);

/// `/health` attempts after the first one.
const HEALTH_RETRIES: u32 = 5;
/// Base backoff between `/health` attempts (doubled per attempt, plus jitter).
const HEALTH_BASE_BACKOFF: Duration = Duration::from_millis(100);
/// Per-attempt `/health` deadline.
const HEALTH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(5);
/// The WebSocket handshake deadline.
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// `http(s)://<host[:port]>/health` for the ticket URL (`ws`→`http`,
/// `wss`→`https`). The query (which may carry a token) and the path are
/// dropped. Built from the admitted ticket URL only, so `/health` goes to the
/// same pinned Vercel host as the socket.
pub(super) fn health_url(ticket: &url::Url) -> Result<url::Url> {
    let scheme = match ticket.scheme() {
        "wss" | "https" => "https",
        "ws" | "http" => "http",
        _ => return Err(anyhow!("vercel: unsupported interactive url scheme")),
    };
    let host = ticket
        .host_str()
        .ok_or_else(|| anyhow!("vercel: interactive url has no host"))?;
    let authority = match ticket.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    url::Url::parse(&format!("{scheme}://{authority}/health"))
        .map_err(|_| anyhow!("vercel: bad interactive health url"))
}

/// The socket URL: the ticket URL (`http`→`ws`, `https`→`wss`) with the ticket
/// token appended as the `token` query pair (url-encoded). The result is a
/// secret: never log or format it.
pub(super) fn ws_url_with_token(ticket: &url::Url, token: &str) -> Result<url::Url> {
    let mut url = ticket.clone();
    let scheme = match ticket.scheme() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return Err(anyhow!("vercel: unsupported interactive url scheme")),
    };
    if url.scheme() != scheme {
        // `set_scheme` refuses special→non-special switches, so rebuild.
        let rest = url
            .as_str()
            .split_once("://")
            .map(|(_, rest)| rest.to_string());
        let rest = rest.ok_or_else(|| anyhow!("vercel: bad interactive url"))?;
        url = url::Url::parse(&format!("{scheme}://{rest}"))
            .map_err(|_| anyhow!("vercel: bad interactive url"))?;
    }
    url.query_pairs_mut().append_pair("token", token);
    Ok(url)
}

/// The wait before `/health` attempt `attempt` (1-based retry index):
/// `base * 2^(attempt-1)` plus up to `base` of jitter (`jitter_ms` is reduced
/// modulo the base).
pub(super) fn health_backoff(attempt: u32, jitter_ms: u64) -> Duration {
    let base = HEALTH_BASE_BACKOFF.as_millis() as u64;
    let exp = base.saturating_mul(1u64 << attempt.saturating_sub(1).min(16));
    Duration::from_millis(exp.saturating_add(jitter_ms % base.max(1)))
}

fn jitter_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_millis()))
        .unwrap_or(0)
}

/// `GET /health` until a 2xx, with [`HEALTH_RETRIES`] retries. No auth header:
/// the sandbox endpoint is not the API authority.
async fn wait_healthy(url: &url::Url) -> Result<()> {
    let client = crate::provider::provider_http_client();
    let mut last_status: Option<u16> = None;
    for attempt in 0..=HEALTH_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(health_backoff(attempt, jitter_ms())).await;
        }
        match tokio::time::timeout(HEALTH_ATTEMPT_TIMEOUT, client.get(url.clone()).send()).await {
            Ok(Ok(resp)) if resp.status().is_success() => return Ok(()),
            Ok(Ok(resp)) => last_status = Some(resp.status().as_u16()),
            Ok(Err(_)) | Err(_) => {}
        }
        tracing::debug!(
            target: "thegn::provider",
            attempt,
            status = last_status,
            "vercel interactive endpoint not healthy yet"
        );
    }
    Err(match last_status {
        Some(code) => anyhow!("vercel: interactive endpoint unhealthy (http {code})"),
        None => anyhow!("vercel: interactive endpoint unreachable"),
    })
}

/// A fixed, value-free description of a WebSocket connect failure (the
/// tungstenite error may echo the URL, which carries the ticket token).
fn connect_error(error: &tokio_tungstenite::tungstenite::Error) -> anyhow::Error {
    match error {
        tokio_tungstenite::tungstenite::Error::Http(resp) => anyhow!(
            "vercel: interactive pty refused (http {})",
            resp.status().as_u16()
        ),
        _ => anyhow!("vercel: interactive pty connect failed"),
    }
}

impl VercelProvider {
    /// The `/interactive` start frame for `spec`, its env through the
    /// provider allowlist.
    pub(super) fn start_frame(&self, spec: &ExecSpec) -> Result<String> {
        interactive_start_frame(
            &spec.argv,
            spec.cwd.as_deref(),
            &self.sandbox_env(&spec.env),
            spec.cols,
            spec.rows,
        )
        .map_err(|e| anyhow!("vercel: interactive start frame: {e}"))
    }

    /// Open a PTY session over `/interactive`. Resumes a stopped sandbox. The
    /// `session_id` watch carries the Vercel session id (informational:
    /// Vercel has no reattach). `spec.env` passes the provider allowlist.
    pub async fn open_exec(&self, name: &str, spec: &ExecSpec) -> Result<ExecSession> {
        anyhow::ensure!(valid_sandbox_name(name), "vercel: invalid sandbox name");
        let start = self.start_frame(spec)?;
        let session = self.interactive_session(name).await?;
        let ticket = self.client.interactive(&session).await?;
        wait_healthy(&health_url(&ticket.url)?).await?;
        let url = zeroize::Zeroizing::new(
            ws_url_with_token(&ticket.url, ticket.token.as_str())?.to_string(),
        );
        let ws = {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let req = url
                .as_str()
                .into_client_request()
                .map_err(|_| anyhow!("vercel: bad interactive request"))?;
            match tokio::time::timeout(
                WS_CONNECT_TIMEOUT,
                tokio_tungstenite::connect_async_with_config(req, Some(exec_ws_config()), false),
            )
            .await
            {
                Ok(Ok((ws, _resp))) => ws,
                Ok(Err(e)) => return Err(connect_error(&e)),
                Err(_) => return Err(anyhow!("vercel: interactive pty connect timed out")),
            }
        };
        Ok(spawn_session(ws, start, session, parse_ws_control))
    }
}

/// Spawn [`drive_interactive`] on the ambient runtime and hand back the
/// channel handle.
pub(super) fn spawn_session(
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    start: String,
    session_id: String,
    classify: fn(&str) -> WsControl,
) -> ExecSession {
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(CHANNEL_CAPACITY);
    let (control_tx, control_rx) = tokio::sync::mpsc::channel::<ExecControl>(256);
    let session_rx = tokio::sync::watch::channel(Some(session_id)).1;
    tokio::spawn(drive_interactive(
        ws, start, frames_tx, control_rx, classify,
    ));
    ExecSession {
        frames: frames_rx,
        control: control_tx,
        session_id: session_rx,
    }
}

/// Forward `bytes` as items of at most [`DATA_CHUNK`]. `send().await` is the
/// backpressure. `false` once the receiver is gone.
pub(super) async fn send_chunked(
    tx: &tokio::sync::mpsc::Sender<ExecFrame>,
    bytes: Vec<u8>,
) -> bool {
    if bytes.len() <= DATA_CHUNK {
        return tx.send(ExecFrame::Stdout(bytes)).await.is_ok();
    }
    for chunk in bytes.chunks(DATA_CHUNK) {
        if tx.send(ExecFrame::Stdout(chunk.to_vec())).await.is_err() {
            return false;
        }
    }
    true
}

/// The exit code for a classified exit frame (`None` ⇒ unknown).
pub(super) fn exit_code(code: Option<i32>) -> i32 {
    code.unwrap_or(EXEC_EXIT_STATUS_UNKNOWN)
}

/// Pump one `/interactive` socket ⇄ the [`ExecSession`] channels. The start
/// frame goes first. Binary frames are output; text frames are classified
/// (`classify` is [`parse_ws_control`] in production): an exit ends the
/// session with its code, anything else is output. A socket that closes
/// without an exit just closes the frames channel (the bridge decides).
pub(super) async fn drive_interactive<S>(
    ws: S,
    start: String,
    frames_tx: tokio::sync::mpsc::Sender<ExecFrame>,
    mut control_rx: tokio::sync::mpsc::Receiver<ExecControl>,
    classify: fn(&str) -> WsControl,
) where
    S: futures_util::Sink<tokio_tungstenite::tungstenite::Message>
        + futures_util::Stream<
            Item = std::result::Result<
                tokio_tungstenite::tungstenite::Message,
                tokio_tungstenite::tungstenite::Error,
            >,
        > + Unpin,
{
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message;
    let (mut write, mut read) = ws.split();
    if write.send(Message::Text(start.into())).await.is_err() {
        tracing::debug!(target: "thegn::provider", "vercel start frame could not be sent");
        return;
    }
    loop {
        tokio::select! {
            ctrl = control_rx.recv() => match ctrl {
                Some(ExecControl::Stdin(bytes)) => {
                    if write.send(Message::Binary(bytes.into())).await.is_err() {
                        break;
                    }
                }
                Some(ExecControl::Resize { cols, rows }) => {
                    let frame = resize_frame(cols, rows);
                    if !frame.is_empty() && write.send(Message::Text(frame.into())).await.is_err() {
                        break;
                    }
                }
                Some(ExecControl::Close) | None => {
                    if write.send(Message::Close(None)).await.is_err() {
                        tracing::debug!(target: "thegn::provider", "vercel pty close could not be sent");
                    }
                    break;
                }
            },
            msg = read.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => {
                    if !send_chunked(&frames_tx, bytes.into()).await {
                        break;
                    }
                }
                Some(Ok(Message::Text(text))) => match classify(text.as_str()) {
                    WsControl::Exit(code) => {
                        if frames_tx.send(ExecFrame::Exit(exit_code(code))).await.is_err() {
                            tracing::debug!(target: "thegn::provider", "vercel pty exit had no consumer");
                        }
                        break;
                    }
                    WsControl::Output(out) => {
                        if !send_chunked(&frames_tx, out.into_bytes()).await {
                            break;
                        }
                    }
                },
                Some(Ok(Message::Ping(payload))) => {
                    if write.send(Message::Pong(payload)).await.is_err() {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::Message;

    /// A loopback `/interactive` fake: accepts one socket and hands its
    /// server half to `script`.
    async fn fake<F, Fut>(script: F) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>)
    where
        F: FnOnce(tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let task = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let ws = tokio_tungstenite::accept_async(tcp)
                .await
                .expect("handshake");
            script(ws).await;
        });
        (addr, task)
    }

    async fn client(addr: std::net::SocketAddr) -> ExecSession {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/interactive"))
            .await
            .expect("connect");
        spawn_session(
            ws,
            r#"{"type":"start","argv":["sh"],"cols":80,"rows":24}"#.into(),
            "sess_1".into(),
            test_classify,
        )
    }

    /// A deterministic classifier for the driver tests (the production one is
    /// lane 2's `parse_ws_control`).
    fn test_classify(text: &str) -> WsControl {
        match text.strip_prefix("exit:") {
            Some("") => WsControl::Exit(None),
            Some(code) => WsControl::Exit(code.parse().ok()),
            None => WsControl::Output(text.to_string()),
        }
    }

    async fn next_text(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    ) -> String {
        loop {
            match ws.next().await.expect("frame").expect("ok") {
                Message::Text(t) => return t.as_str().to_string(),
                Message::Ping(_) | Message::Pong(_) => {}
                other => panic!("expected text, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn start_frame_goes_first_then_stdin_binary_resize_and_exit() {
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let (addr, server) = fake(move |mut ws| async move {
            let start = next_text(&mut ws).await;
            seen_tx.send(Message::Text(start.into())).expect("record");
            // stdin, plus the resize frame when lane 2's builder emits one.
            let expected = if resize_frame(100, 30).is_empty() {
                1
            } else {
                2
            };
            for _ in 0..expected {
                let msg = ws.next().await.expect("frame").expect("ok");
                seen_tx.send(msg).expect("record");
            }
            ws.send(Message::Binary(b"hello".to_vec().into()))
                .await
                .expect("out");
            ws.send(Message::Text("exit:7".into())).await.expect("exit");
        })
        .await;
        let mut sess = client(addr).await;
        assert_eq!(sess.session_id.borrow().as_deref(), Some("sess_1"));
        sess.control
            .send(ExecControl::Stdin(b"ls\r".to_vec()))
            .await
            .expect("stdin");
        sess.control
            .send(ExecControl::Resize {
                cols: 100,
                rows: 30,
            })
            .await
            .expect("resize");
        assert_eq!(
            sess.frames.recv().await,
            Some(ExecFrame::Stdout(b"hello".to_vec()))
        );
        assert_eq!(sess.frames.recv().await, Some(ExecFrame::Exit(7)));
        server.await.expect("server");

        let start = seen_rx.recv().await.expect("start");
        assert_eq!(
            start,
            Message::Text(r#"{"type":"start","argv":["sh"],"cols":80,"rows":24}"#.into()),
            "the start frame is the first frame, verbatim"
        );
        assert_eq!(
            seen_rx.recv().await,
            Some(Message::Binary(b"ls\r".to_vec().into())),
            "stdin is a binary frame"
        );
        // The resize frame is lane 2's `resize_frame`; an empty frame is not sent.
        let expected_resize = resize_frame(100, 30);
        if !expected_resize.is_empty() {
            assert_eq!(
                seen_rx.recv().await,
                Some(Message::Text(expected_resize.into()))
            );
        }
    }

    #[tokio::test]
    async fn exit_without_a_code_maps_to_unknown() {
        let (addr, server) = fake(|mut ws| async move {
            next_text(&mut ws).await;
            ws.send(Message::Text("exit:".into())).await.expect("exit");
        })
        .await;
        let mut sess = client(addr).await;
        assert_eq!(
            sess.frames.recv().await,
            Some(ExecFrame::Exit(EXEC_EXIT_STATUS_UNKNOWN))
        );
        assert_eq!(
            sess.frames.recv().await,
            None,
            "the session ends after exit"
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn socket_close_without_exit_just_closes_the_frames() {
        let (addr, server) = fake(|mut ws| async move {
            next_text(&mut ws).await;
            ws.send(Message::Binary(b"partial".to_vec().into()))
                .await
                .expect("out");
            ws.close(None).await.expect("close");
        })
        .await;
        let mut sess = client(addr).await;
        assert_eq!(
            sess.frames.recv().await,
            Some(ExecFrame::Stdout(b"partial".to_vec()))
        );
        assert_eq!(sess.frames.recv().await, None, "no synthesized exit");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn non_json_text_is_output_under_the_production_classifier() {
        let (addr, server) = fake(|mut ws| async move {
            next_text(&mut ws).await;
            ws.send(Message::Text("plain text, not json".into()))
                .await
                .expect("out");
            ws.close(None).await.expect("close");
        })
        .await;
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
            .await
            .expect("connect");
        let mut sess = spawn_session(ws, "{}".into(), "s".into(), parse_ws_control);
        assert_eq!(
            sess.frames.recv().await,
            Some(ExecFrame::Stdout(b"plain text, not json".to_vec()))
        );
        server.await.expect("server");
    }

    #[tokio::test]
    async fn close_control_sends_a_websocket_close() {
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let (addr, server) = fake(move |mut ws| async move {
            next_text(&mut ws).await;
            let got_close = matches!(ws.next().await, Some(Ok(Message::Close(_))) | None);
            closed_tx.send(got_close).expect("report");
        })
        .await;
        let mut sess = client(addr).await;
        sess.control.send(ExecControl::Close).await.expect("close");
        assert!(
            closed_rx.await.expect("server report"),
            "server saw a close"
        );
        assert_eq!(sess.frames.recv().await, None);
        server.await.expect("server");
    }

    #[tokio::test]
    async fn large_output_is_chunked() {
        let big = vec![b'x'; DATA_CHUNK * 2 + 5];
        let sent = big.clone();
        let (addr, server) = fake(move |mut ws| async move {
            next_text(&mut ws).await;
            ws.send(Message::Binary(sent.into())).await.expect("out");
            ws.send(Message::Text("exit:0".into())).await.expect("exit");
        })
        .await;
        let mut sess = client(addr).await;
        let mut got = Vec::new();
        let mut items = 0;
        loop {
            match sess.frames.recv().await {
                Some(ExecFrame::Stdout(b)) => {
                    assert!(b.len() <= DATA_CHUNK);
                    items += 1;
                    got.extend(b);
                }
                Some(ExecFrame::Exit(code)) => {
                    assert_eq!(code, 0);
                    break;
                }
                None => panic!("ended without exit"),
            }
        }
        assert_eq!(items, 3);
        assert_eq!(got, big);
        server.await.expect("server");
    }

    #[test]
    fn health_url_drops_path_and_query_and_maps_the_scheme() {
        let t = url::Url::parse("wss://sb-1.vercel.run/interactive?x=1").expect("url");
        assert_eq!(
            health_url(&t).expect("h").as_str(),
            "https://sb-1.vercel.run/health"
        );
        let t = url::Url::parse("ws://127.0.0.1:8080/pty").expect("url");
        assert_eq!(
            health_url(&t).expect("h").as_str(),
            "http://127.0.0.1:8080/health"
        );
        let t = url::Url::parse("ftp://h/").expect("url");
        assert!(health_url(&t).is_err());
    }

    #[test]
    fn ws_url_carries_the_urlencoded_token_and_a_ws_scheme() {
        let t = url::Url::parse("https://sb-1.vercel.run/interactive").expect("url");
        let u = ws_url_with_token(&t, "a b&c=d").expect("ws");
        assert_eq!(u.scheme(), "wss");
        assert_eq!(u.path(), "/interactive");
        assert_eq!(u.query(), Some("token=a+b%26c%3Dd"));
        let t = url::Url::parse("ws://127.0.0.1:9/i?k=v").expect("url");
        let u = ws_url_with_token(&t, "t").expect("ws");
        assert_eq!(u.as_str(), "ws://127.0.0.1:9/i?k=v&token=t");
    }

    #[test]
    fn health_backoff_doubles_from_100ms_with_bounded_jitter() {
        assert_eq!(health_backoff(1, 0), Duration::from_millis(100));
        assert_eq!(health_backoff(2, 0), Duration::from_millis(200));
        assert_eq!(health_backoff(5, 0), Duration::from_millis(1600));
        assert_eq!(health_backoff(1, 42), Duration::from_millis(142));
        assert_eq!(
            health_backoff(1, 250),
            Duration::from_millis(150),
            "jitter < base"
        );
    }

    #[test]
    fn exit_code_maps_none_to_unknown() {
        assert_eq!(exit_code(Some(3)), 3);
        assert_eq!(exit_code(None), EXEC_EXIT_STATUS_UNKNOWN);
    }

    #[test]
    fn connect_errors_never_echo_the_url() {
        let e = tokio_tungstenite::tungstenite::Error::Url(
            tokio_tungstenite::tungstenite::error::UrlError::UnsupportedUrlScheme,
        );
        let msg = format!("{:#}", connect_error(&e));
        assert_eq!(msg, "vercel: interactive pty connect failed");
    }
}
