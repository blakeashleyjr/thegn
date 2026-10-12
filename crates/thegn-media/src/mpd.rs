//! Native MPD backend — speaks the MPD line protocol directly over TCP
//! (`127.0.0.1:6600`) or a unix socket, so the whole MPD family (mpd, mpc, rmpc,
//! ncmpcpp, cantata) is picked up with **no `mpd-mpris` bridge**. This is what
//! makes MPD-driven playback show up out of the box under `auto`.
//!
//! Reads use a fresh connection per call (like the mpv backend — MPD connections
//! are cheap and the protocol is stateless per command). The push [`MpdWatch`]
//! holds its own long-lived connection blocked on MPD's `idle` command, which
//! returns the moment a subsystem changes — a real event stream that keeps the
//! ~0%-idle contract, parallel to MPRIS D-Bus signals.

use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::model::{LoopMode, MediaState};
use crate::mpd_parse::{self, Pair};
use crate::{MediaBackend, MediaCaps, MediaError, MediaWatch};

/// Maximum bytes in the `OK MPD <version>` greeting line.
const MAX_GREETING_BYTES: usize = 256;
/// Maximum bytes in any single reply line (tags can be long, but not unbounded).
const MAX_LINE_BYTES: usize = 16 * 1024;
/// Maximum total bytes accepted for one command's reply.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
/// Maximum `key: value` rows accepted for one command's reply.
const MAX_RESPONSE_PAIRS: usize = 4096;
/// Deadline for the greeting after the transport connects.
const GREETING_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for writing one command frame.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Total budget (write + every reply line) for a finite command. `idle` is the
/// one long poll and is exempt from this, but not from the size caps.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum characters of peer-supplied text echoed into an error.
const MAX_DIAG_CHARS: usize = 120;

/// Peer text made safe for an error or log: control chars replaced, length capped.
fn sanitize_diag(s: &str) -> String {
    let mut out: String = s
        .chars()
        .take(MAX_DIAG_CHARS)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    if s.chars().count() > MAX_DIAG_CHARS {
        out.push_str("...");
    }
    out
}

/// Read one `\n`-terminated line of at most `max` bytes (terminator excluded)
/// without ever buffering more than `max` bytes. `Ok(None)` is a clean EOF.
async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    io: &mut R,
    max: usize,
) -> Result<Option<String>, String> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let buf = io.fill_buf().await.map_err(|e| e.to_string())?;
        if buf.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err("unterminated line before EOF".into())
            };
        }
        let nl = buf.iter().position(|b| *b == b'\n');
        let take = nl.unwrap_or(buf.len());
        if line.len() + take > max {
            return Err(format!("line exceeds {max} bytes"));
        }
        line.extend_from_slice(&buf[..take]);
        let consumed = nl.map_or(take, |i| i + 1);
        io.consume(consumed);
        if nl.is_some() {
            return String::from_utf8(line)
                .map(Some)
                .map_err(|_| "line is not valid UTF-8".to_string());
        }
    }
}

/// Any bidirectional MPD transport (TCP or unix socket), boxed so one code path
/// drives both.
trait Duplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Duplex for T {}

/// Where the MPD daemon lives, plus an optional password.
pub struct MpdEndpoint {
    kind: EndpointKind,
    password: Option<crate::MpdPassword>,
}

impl std::fmt::Debug for MpdEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpdEndpoint")
            .field("kind", &self.kind)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

#[derive(Debug, Clone)]
enum EndpointKind {
    Tcp {
        host: String,
        port: u16,
    },
    /// A unix-socket path (unix only; ignored elsewhere).
    Unix(String),
}

impl MpdEndpoint {
    /// Resolve a sanitized host-side socket into an endpoint. A leading `/`
    /// (unix) is a socket path; otherwise `host:port` (default port 6600).
    pub fn resolve(socket: &str, password: Option<crate::MpdPassword>) -> MpdEndpoint {
        let trimmed = socket.trim();
        let kind = if trimmed.starts_with('/') {
            EndpointKind::Unix(trimmed.to_string())
        } else {
            parse_host_port(trimmed)
        };
        MpdEndpoint { kind, password }
    }
}

/// Split `host:port` (default port 6600, default host 127.0.0.1). IPv6 literals
/// aren't special-cased — MPD endpoints are hostnames/IPv4 in practice.
fn parse_host_port(s: &str) -> EndpointKind {
    let (host, port) = match s.rsplit_once(':') {
        Some((h, p)) => (h, p.trim().parse().unwrap_or(6600)),
        None => (s, 6600),
    };
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    EndpointKind::Tcp {
        host: host.to_string(),
        port,
    }
}

/// A live MPD connection with its greeting consumed and password (if any) sent.
struct MpdConn {
    io: BufReader<Box<dyn Duplex>>,
}

impl MpdConn {
    async fn dial(ep: &MpdEndpoint) -> Result<MpdConn, MediaError> {
        // Bound the connect so an unreachable remote endpoint can't hang the
        // watcher task (localhost refusal is already instant).
        let connect = async {
            let s: Box<dyn Duplex> = match &ep.kind {
                EndpointKind::Tcp { host, port } => {
                    let s = tokio::net::TcpStream::connect((host.as_str(), *port))
                        .await
                        .map_err(|e| {
                            MediaError::Unavailable(format!("mpd tcp {host}:{port}: {e}"))
                        })?;
                    Box::new(s)
                }
                EndpointKind::Unix(path) => dial_unix(path).await?,
            };
            Ok::<_, MediaError>(s)
        };
        let stream = tokio::time::timeout(Duration::from_secs(3), connect)
            .await
            .map_err(|_| MediaError::Unavailable("mpd connect timed out".into()))??;
        let mut conn = MpdConn {
            io: BufReader::new(stream),
        };
        // Greeting: `OK MPD <version>`, bounded in size and time.
        let line = tokio::time::timeout(
            GREETING_TIMEOUT,
            read_bounded_line(&mut conn.io, MAX_GREETING_BYTES),
        )
        .await
        .map_err(|_| MediaError::Unavailable("mpd greeting timed out".into()))?
        .map_err(|e| MediaError::Unavailable(format!("mpd greeting: {e}")))?
        .unwrap_or_default();
        if !line.starts_with("OK MPD") {
            return Err(MediaError::Backend(format!(
                "unexpected MPD greeting: {:?}",
                sanitize_diag(&line)
            )));
        }
        if let Some(pw) = &ep.password {
            // Best-effort auth; a rejection surfaces on the first real command.
            let mut auth_command = zeroize::Zeroizing::new(String::from("password "));
            quote_into(&mut auth_command, pw.expose())?;
            conn.command(&auth_command).await?;
        }
        Ok(conn)
    }

    /// Run one finite command under [`COMMAND_TIMEOUT`] and collect its
    /// `key: value` reply up to the terminating `OK` (an `ACK …` line becomes a
    /// [`MediaError::Backend`]).
    async fn command(&mut self, cmd: &str) -> Result<Vec<Pair>, MediaError> {
        self.command_budget(cmd, Some(COMMAND_TIMEOUT)).await
    }

    /// Like [`command`](Self::command) with an explicit total budget; `None` is
    /// the `idle` long poll (no deadline, but still size-capped; dropping the
    /// future/connection cancels it).
    async fn command_budget(
        &mut self,
        cmd: &str,
        budget: Option<Duration>,
    ) -> Result<Vec<Pair>, MediaError> {
        // One command is exactly one frame: no embedded line breaks or NUL.
        if cmd.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
            return Err(MediaError::Backend(
                "mpd command contains a forbidden control character".into(),
            ));
        }
        match budget {
            Some(d) => tokio::time::timeout(d, self.command_inner(cmd))
                .await
                .map_err(|_| MediaError::Backend("mpd command timed out".into()))?,
            None => self.command_inner(cmd).await,
        }
    }

    async fn command_inner(&mut self, cmd: &str) -> Result<Vec<Pair>, MediaError> {
        let line = zeroize::Zeroizing::new(format!("{cmd}\n"));
        tokio::time::timeout(WRITE_TIMEOUT, self.io.get_mut().write_all(line.as_bytes()))
            .await
            .map_err(|_| MediaError::Backend("mpd write timed out".into()))?
            .map_err(|e| MediaError::Backend(format!("mpd write: {e}")))?;
        let mut pairs = Vec::new();
        let mut total = 0usize;
        loop {
            let Some(buf) = read_bounded_line(&mut self.io, MAX_LINE_BYTES)
                .await
                .map_err(|e| MediaError::Backend(format!("mpd read: {e}")))?
            else {
                return Err(MediaError::Backend("mpd closed the connection".into()));
            };
            total += buf.len() + 1;
            if total > MAX_RESPONSE_BYTES {
                return Err(MediaError::Backend(format!(
                    "mpd response exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            let t = buf.trim_end_matches('\r');
            if t == "OK" {
                return Ok(pairs);
            }
            if let Some(err) = t.strip_prefix("ACK") {
                return Err(MediaError::Backend(format!(
                    "mpd ACK{}",
                    sanitize_diag(err)
                )));
            }
            if let Some(p) = mpd_parse::parse_line(t) {
                if pairs.len() >= MAX_RESPONSE_PAIRS {
                    return Err(MediaError::Backend(format!(
                        "mpd response exceeds {MAX_RESPONSE_PAIRS} rows"
                    )));
                }
                pairs.push(p);
            }
        }
    }
}

#[cfg(unix)]
async fn dial_unix(path: &str) -> Result<Box<dyn Duplex>, MediaError> {
    let s = tokio::net::UnixStream::connect(path)
        .await
        .map_err(|e| MediaError::Unavailable(format!("mpd unix {path}: {e}")))?;
    Ok(Box::new(s))
}
#[cfg(not(unix))]
async fn dial_unix(_path: &str) -> Result<Box<dyn Duplex>, MediaError> {
    Err(MediaError::Unavailable(
        "mpd unix socket requires a unix target".into(),
    ))
}

/// Quote a value for a single MPD command argument (only `"`/`\` need escaping).
#[cfg(test)]
fn quote(s: &str) -> String {
    if s.chars().any(|c| c == ' ' || c == '"' || c == '\\') {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// Append one quoted-if-needed command argument. Rejects every control
/// character (CR, LF, NUL, ...) before anything is written, so an argument can
/// never end the frame. The error never includes the value.
fn quote_into(out: &mut String, s: &str) -> Result<(), MediaError> {
    if s.chars().any(char::is_control) {
        return Err(MediaError::Backend(
            "mpd argument contains a forbidden control character".into(),
        ));
    }
    if s.bytes()
        .any(|b| b.is_ascii_whitespace() || b == b'"' || b == b'\\')
    {
        out.push('"');
        for ch in s.chars() {
            if ch == '"' || ch == '\\' {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push('"');
    } else {
        out.push_str(s);
    }
    Ok(())
}

/// The native MPD backend. Owns one credential-bearing endpoint and dials per read.
pub struct Mpd {
    ep: MpdEndpoint,
}

impl Mpd {
    /// Connect once to verify the daemon answers (so a dead endpoint doesn't sit
    /// in the aggregator), then keep the endpoint for per-call dials.
    pub async fn connect(ep: MpdEndpoint) -> Result<Mpd, MediaError> {
        // Probe: a successful dial (greeting + optional password) is enough.
        MpdConn::dial(&ep).await?;
        Ok(Mpd { ep })
    }

    async fn conn(&self) -> Result<MpdConn, MediaError> {
        MpdConn::dial(&self.ep).await
    }

    /// Open a push watcher on its own connection.
    pub async fn watch(&self) -> Result<MpdWatch, MediaError> {
        let conn = self.conn().await?;
        Ok(MpdWatch { conn: Some(conn) })
    }

    /// Current volume `0..=100` (for relative stepping), or 100 when unknown.
    async fn current_volume(&self) -> u8 {
        let Ok(mut c) = self.conn().await else {
            return 100;
        };
        let status = c.command("status").await.unwrap_or_default();
        mpd_parse::field(&status, "volume")
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|&v| v >= 0)
            .map(|v| v.clamp(0, 100) as u8)
            .unwrap_or(100)
    }
}

impl MediaBackend for Mpd {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            let mut c = match self.conn().await {
                Ok(c) => c,
                // Daemon went away — show nothing rather than erroring the watcher.
                Err(MediaError::Unavailable(_)) => return Ok(None),
                Err(e) => return Err(e),
            };
            let status = c.command("status").await?;
            // Nothing loaded (`state: stop` with no song) still yields a valid
            // stopped state; the badge hides it.
            let song = c.command("currentsong").await.unwrap_or_default();
            Ok(Some(mpd_parse::to_state(&status, &song)))
        })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // `pause` with no argument toggles pause; from stopped, `play` starts.
            let mut c = self.conn().await?;
            let status = c.command("status").await?;
            match mpd_parse::field(&status, "state").unwrap_or("stop") {
                "stop" => c.command("play").await.map(|_| ()),
                _ => c.command("pause").await.map(|_| ()),
            }
        })
    }
    fn next(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.conn().await?.command("next").await.map(|_| ()) })
    }
    fn previous(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move { self.conn().await?.command("previous").await.map(|_| ()) })
    }
    fn set_shuffle(&self, on: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            self.conn()
                .await?
                .command(&format!("random {}", on as u8))
                .await
                .map(|_| ())
        })
    }
    fn set_loop(&self, mode: LoopMode) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let (repeat, single) = match mode {
                LoopMode::None => (0, 0),
                LoopMode::Playlist => (1, 0),
                LoopMode::Track => (1, 1),
            };
            let mut c = self.conn().await?;
            c.command(&format!("repeat {repeat}")).await?;
            c.command(&format!("single {single}")).await.map(|_| ())
        })
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let cur = self.current_volume().await as f64;
            let next = (cur + delta * 100.0).clamp(0.0, 100.0).round() as u8;
            self.conn()
                .await?
                .command(&format!("setvol {next}"))
                .await
                .map(|_| ())
        })
    }

    fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
        Box::pin(async move {
            Ok(Vec::new()) // stored playlists deferred
        })
    }
    fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move { Ok(()) })
    }

    fn seek(&self, offset: Duration, forward: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let sign = if forward { '+' } else { '-' };
            self.conn()
                .await?
                .command(&format!("seekcur {sign}{}", offset.as_secs()))
                .await
                .map(|_| ())
        })
    }
    fn set_position<'a>(
        &'a self,
        pos: Duration,
        _track_id: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            self.conn()
                .await?
                .command(&format!("seekcur {}", pos.as_secs()))
                .await
                .map(|_| ())
        })
    }
    fn set_volume(&self, level: u8) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            self.conn()
                .await?
                .command(&format!("setvol {}", level.min(100)))
                .await
                .map(|_| ())
        })
    }

    /// One daemon per endpoint.
    fn players(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { vec!["mpd".to_string()] })
    }

    /// The `idle` push watcher.
    fn watch(&self) -> BoxFuture<'_, Option<Box<dyn MediaWatch + Send>>> {
        Box::pin(async move {
            Mpd::watch(self)
                .await
                .ok()
                .map(|w| Box::new(w) as Box<dyn MediaWatch + Send>)
        })
    }

    fn caps(&self) -> MediaCaps {
        MediaCaps {
            shuffle: true,
            loop_mode: true,
            volume: true,
            playlists: false,
            signals: true, // push via `idle`
            seek: true,
            art: false, // readpicture/albumart deferred
            queue: false,
            abs_volume: true,
            chapters: false,
            fullscreen: false,
        }
    }
}

/// A push watcher blocked on MPD's `idle` command. Each `changed()` issues
/// `idle player mixer options` (the subsystems that move the badge) and resolves
/// when MPD reports one changed. A dropped connection ends the stream.
pub struct MpdWatch {
    conn: Option<MpdConn>,
}

impl MediaWatch for MpdWatch {
    fn changed(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
        Box::pin(async move {
            let Some(conn) = self.conn.as_mut() else {
                return false;
            };
            match conn.command_budget("idle player mixer options", None).await {
                Ok(changed) => {
                    tracing::debug!(target: "thegn::media", ?changed, "MPD idle tick");
                    true
                }
                Err(e) => {
                    // Connection lost — end the stream so the host reconnects.
                    tracing::debug!(target: "thegn::media", error = %e, "MPD idle stream ended");
                    self.conn = None;
                    false
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_to_localhost_tcp() {
        // Environment parsing belongs to the audited host boundary.
        let ep = MpdEndpoint::resolve("127.0.0.1:6600", None);
        match ep.kind {
            EndpointKind::Tcp { host, port } => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, 6600);
            }
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn endpoint_debug_redacts_password() {
        let sentinel = "mpd-debug-secret-sentinel";
        let endpoint = MpdEndpoint::resolve(
            "music.lan:6601",
            Some(crate::MpdPassword::new(sentinel.to_string())),
        );
        let rendered = format!("{endpoint:?}");
        assert!(!rendered.contains(sentinel));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn resolve_explicit_host_port() {
        let ep = MpdEndpoint::resolve("music.lan:6601", None);
        match ep.kind {
            EndpointKind::Tcp { host, port } => {
                assert_eq!(host, "music.lan");
                assert_eq!(port, 6601);
            }
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn resolve_bare_host_defaults_port() {
        match MpdEndpoint::resolve("music.lan", None).kind {
            EndpointKind::Tcp { host, port } => {
                assert_eq!(host, "music.lan");
                assert_eq!(port, 6600);
            }
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn resolve_unix_path() {
        match MpdEndpoint::resolve("/run/mpd/socket", None).kind {
            EndpointKind::Unix(p) => assert_eq!(p, "/run/mpd/socket"),
            _ => panic!("expected unix"),
        }
    }

    #[test]
    fn quote_escapes_only_when_needed() {
        assert_eq!(quote("hunter2"), "hunter2");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn quote_into_rejects_control_chars_without_echoing() {
        for bad in ["a\nOK", "a\rb", "a\0b", "tab\there", "x\u{7f}"] {
            let mut out = String::from("password ");
            let err = quote_into(&mut out, bad).unwrap_err().to_string();
            assert_eq!(out, "password ");
            assert!(!err.contains("OK"));
        }
        let mut out = String::new();
        quote_into(&mut out, "two words").unwrap();
        assert_eq!(out, "\"two words\"");
    }

    #[test]
    fn sanitize_diag_caps_and_strips_controls() {
        let long = format!("a\x1b[31m{}", "b".repeat(500));
        let d = sanitize_diag(&long);
        assert!(!d.contains('\x1b'));
        assert!(d.chars().count() <= MAX_DIAG_CHARS + 3);
    }

    #[tokio::test]
    async fn bounded_line_rejects_unterminated_overlong_input() {
        let data = vec![b'a'; 10_000];
        let mut r = BufReader::new(&data[..]);
        assert!(read_bounded_line(&mut r, 64).await.is_err());
    }

    #[tokio::test]
    async fn bounded_line_splits_and_rejects_bad_utf8() {
        let mut r = BufReader::new(&b"one\ntwo\n"[..]);
        assert_eq!(read_bounded_line(&mut r, 16).await.unwrap().unwrap(), "one");
        assert_eq!(read_bounded_line(&mut r, 16).await.unwrap().unwrap(), "two");
        assert!(read_bounded_line(&mut r, 16).await.unwrap().is_none());
        let mut bad = BufReader::new(&[0xff, 0xfe, b'\n'][..]);
        assert!(read_bounded_line(&mut bad, 16).await.is_err());
    }

    /// Serve `script` to the first TCP client after sending `greeting`, then
    /// keep the socket open (silent) until the test ends.
    async fn fake_server(greeting: Vec<u8>, reply: Vec<u8>) -> MpdEndpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut sock, _) = listener.accept().await.unwrap();
            sock.write_all(&greeting).await.unwrap();
            let mut buf = [0u8; 256];
            if sock.read(&mut buf).await.is_err() {
                return;
            }
            if sock.write_all(&reply).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None)
    }

    #[tokio::test]
    async fn oversized_greeting_is_rejected() {
        let ep = fake_server(vec![b'x'; 4096], Vec::new()).await;
        assert!(MpdConn::dial(&ep).await.is_err());
    }

    #[tokio::test]
    async fn endless_rows_hit_the_pair_cap() {
        let reply = "a: b\n".repeat(MAX_RESPONSE_PAIRS + 10).into_bytes();
        let ep = fake_server(b"OK MPD 0.23.0\n".to_vec(), reply).await;
        let mut c = MpdConn::dial(&ep).await.unwrap();
        let err = c.command("status").await.unwrap_err().to_string();
        assert!(err.contains("rows"), "{err}");
    }

    #[tokio::test]
    async fn silent_peer_times_out_within_budget() {
        let ep = fake_server(b"OK MPD 0.23.0\n".to_vec(), Vec::new()).await;
        let mut c = MpdConn::dial(&ep).await.unwrap();
        let err = c
            .command_budget("status", Some(Duration::from_millis(100)))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn command_with_line_break_is_refused_before_writing() {
        let ep = fake_server(b"OK MPD 0.23.0\n".to_vec(), b"OK\n".to_vec()).await;
        let mut c = MpdConn::dial(&ep).await.unwrap();
        assert!(c.command("status\nclear").await.is_err());
    }
}
