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
pub(crate) async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
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

/// Why an MPD endpoint string was rejected. Never carries the password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MpdEndpointError {
    /// The endpoint text is empty.
    Empty,
    /// No host before the port (e.g. `:6600`).
    EmptyHost,
    /// The port is missing, non-numeric, zero, or out of range.
    InvalidPort(String),
    /// Unterminated or malformed `[...]` IPv6 literal.
    InvalidIpv6(String),
    /// A bare host containing more than one `:` (IPv6 needs brackets).
    AmbiguousHost(String),
    /// Whitespace or control characters in the host.
    InvalidHost(String),
}

impl std::fmt::Display for MpdEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "mpd endpoint is empty"),
            Self::EmptyHost => write!(f, "mpd endpoint has an empty host"),
            Self::InvalidPort(p) => {
                write!(f, "mpd endpoint has an invalid port {:?}", sanitize_diag(p))
            }
            Self::InvalidIpv6(h) => write!(
                f,
                "mpd endpoint has an invalid IPv6 literal {:?}",
                sanitize_diag(h)
            ),
            Self::AmbiguousHost(h) => write!(
                f,
                "mpd endpoint {:?} is ambiguous; put IPv6 addresses in brackets, e.g. [::1]:6600",
                sanitize_diag(h)
            ),
            Self::InvalidHost(h) => {
                write!(f, "mpd endpoint has an invalid host {:?}", sanitize_diag(h))
            }
        }
    }
}

impl std::error::Error for MpdEndpointError {}

impl MpdEndpoint {
    /// Resolve a sanitized host-side socket into an endpoint. A leading `/`
    /// (unix) is a socket path; otherwise `host`, `host:port`, `[v6]` or
    /// `[v6]:port` (default port 6600). Invalid text is an error, never a
    /// silent fallback to another host or port.
    pub fn resolve(
        socket: &str,
        password: Option<crate::MpdPassword>,
    ) -> Result<MpdEndpoint, MpdEndpointError> {
        let trimmed = socket.trim();
        if trimmed.is_empty() {
            return Err(MpdEndpointError::Empty);
        }
        let kind = if trimmed.starts_with('/') {
            EndpointKind::Unix(trimmed.to_string())
        } else {
            parse_host_port(trimmed)?
        };
        Ok(MpdEndpoint { kind, password })
    }
}

fn parse_port(p: &str) -> Result<u16, MpdEndpointError> {
    match p.trim().parse::<u16>() {
        Ok(n) if n != 0 => Ok(n),
        _ => Err(MpdEndpointError::InvalidPort(p.to_string())),
    }
}

/// Split `host[:port]` / `[v6][:port]` (default port 6600).
fn parse_host_port(s: &str) -> Result<EndpointKind, MpdEndpointError> {
    let (host, port) = if let Some(rest) = s.strip_prefix('[') {
        let (inner, after) = rest
            .split_once(']')
            .ok_or_else(|| MpdEndpointError::InvalidIpv6(s.to_string()))?;
        if inner.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(MpdEndpointError::InvalidIpv6(inner.to_string()));
        }
        let port = match after {
            "" => 6600,
            _ => parse_port(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| MpdEndpointError::InvalidIpv6(s.to_string()))?,
            )?,
        };
        (inner, port)
    } else {
        match s.matches(':').count() {
            0 => (s, 6600),
            1 => {
                let (h, p) = s.split_once(':').unwrap_or((s, ""));
                (h, parse_port(p)?)
            }
            _ => return Err(MpdEndpointError::AmbiguousHost(s.to_string())),
        }
    };
    if host.is_empty() {
        return Err(MpdEndpointError::EmptyHost);
    }
    if host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(MpdEndpointError::InvalidHost(host.to_string()));
    }
    Ok(EndpointKind::Tcp {
        host: host.to_string(),
        port,
    })
}

/// A live MPD connection with its greeting consumed and password (if any) sent.
struct MpdConn {
    io: BufReader<Box<dyn Duplex>>,
    // Bytes consumed from the transport but not yet terminated by `\n`.
    // Kept on the connection so dropping an `idle` future is safe.
    line_buffer: Vec<u8>,
    line_wire_bytes: usize,
    line_too_long: bool,
    idle_pending: bool,
    idle_response_bytes: usize,
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
            line_buffer: Vec::new(),
            line_wire_bytes: 0,
            line_too_long: false,
            idle_pending: false,
            idle_response_bytes: 0,
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
            let Some((buf, line_bytes)) = self
                .next_response_line(MAX_RESPONSE_BYTES - total)
                .await
                .map_err(|e| MediaError::Backend(format!("mpd read: {e}")))?
            else {
                return Err(MediaError::Backend("mpd closed the connection".into()));
            };
            total += line_bytes;
            if total > MAX_RESPONSE_BYTES {
                return Err(MediaError::Backend(format!(
                    "mpd response exceeds {MAX_RESPONSE_BYTES} bytes"
                )));
            }
            let Some(buf) = buf else {
                // A tag may contain a very large embedded cue sheet. Drop that
                // record, but keep framing the response at the next newline.
                continue;
            };
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

    /// Read one response line, retaining partial bytes across cancellation.
    /// An overlong line is drained through its newline and returned as `None`;
    /// its actual wire size is still charged against the response byte cap.
    async fn next_response_line(
        &mut self,
        remaining_response_bytes: usize,
    ) -> Result<Option<(Option<String>, usize)>, String> {
        loop {
            let buf = self.io.fill_buf().await.map_err(|e| e.to_string())?;
            if buf.is_empty() {
                return if self.line_wire_bytes == 0 {
                    Ok(None)
                } else {
                    Err("unterminated line before EOF".into())
                };
            }
            let nl = buf.iter().position(|b| *b == b'\n');
            let take = nl.unwrap_or(buf.len());
            self.line_wire_bytes = self.line_wire_bytes.saturating_add(take);
            if self.line_wire_bytes + 1 > remaining_response_bytes {
                return Err(format!("mpd response exceeds {MAX_RESPONSE_BYTES} bytes"));
            }
            if !self.line_too_long {
                if self.line_buffer.len().saturating_add(take) > MAX_LINE_BYTES {
                    self.line_too_long = true;
                    self.line_buffer.clear();
                } else {
                    self.line_buffer.extend_from_slice(&buf[..take]);
                }
            }
            let consumed = nl.map_or(take, |i| i + 1);
            self.io.consume(consumed);
            if nl.is_some() {
                let parsed = if self.line_too_long {
                    None
                } else {
                    Some(
                        String::from_utf8(std::mem::take(&mut self.line_buffer))
                            .map_err(|_| "line is not valid UTF-8".to_string())?,
                    )
                };
                let line_bytes = self.line_wire_bytes + 1;
                self.line_buffer.clear();
                self.line_wire_bytes = 0;
                self.line_too_long = false;
                return Ok(Some((parsed, line_bytes)));
            }
        }
    }

    /// Start or resume MPD's long-poll command. `idle_pending` is set only
    /// after its complete frame is written; response bytes remain on `self`
    /// when the caller drops this future.
    async fn idle_changed(&mut self) -> Result<(), MediaError> {
        if !self.idle_pending {
            let line = b"idle player mixer options\n";
            tokio::time::timeout(WRITE_TIMEOUT, self.io.get_mut().write_all(line))
                .await
                .map_err(|_| MediaError::Backend("mpd write timed out".into()))?
                .map_err(|e| MediaError::Backend(format!("mpd write: {e}")))?;
            self.idle_pending = true;
        }
        loop {
            let Some((line, bytes)) = self
                .next_response_line(MAX_RESPONSE_BYTES - self.idle_response_bytes)
                .await
                .map_err(|e| MediaError::Backend(format!("mpd read: {e}")))?
            else {
                return Err(MediaError::Backend("mpd closed the connection".into()));
            };
            self.idle_response_bytes += bytes;
            let Some(line) = line else { continue };
            let t = line.trim_end_matches('\r');
            if t == "OK" {
                self.idle_pending = false;
                self.idle_response_bytes = 0;
                return Ok(());
            }
            if let Some(err) = t.strip_prefix("ACK") {
                self.idle_pending = false;
                self.idle_response_bytes = 0;
                return Err(MediaError::Backend(format!(
                    "mpd ACK{}",
                    sanitize_diag(err)
                )));
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
    caps: MediaCaps,
    commands: Vec<String>,
}

impl Mpd {
    /// Connect once to verify the daemon and discover its authenticated command
    /// set, then keep the endpoint for per-call dials.
    pub async fn connect(ep: MpdEndpoint) -> Result<Mpd, MediaError> {
        let mut conn = MpdConn::dial(&ep).await?;
        let commands = conn.command("commands").await?;
        let caps = caps_from_commands(&commands);
        let commands = commands
            .iter()
            .filter(|(key, _)| key == "command")
            .map(|(_, value)| value.clone())
            .collect();
        Ok(Mpd { ep, caps, commands })
    }

    async fn conn(&self) -> Result<MpdConn, MediaError> {
        MpdConn::dial(&self.ep).await
    }

    async fn reconcile_loop(
        &self,
        target: (u8, &str),
        previous: (u8, &str),
        stage: &str,
        cause: MediaError,
    ) -> Result<(), MediaError> {
        let mut conn = match self.conn().await {
            Ok(conn) => conn,
            Err(error) => {
                return self
                    .restore_loop_after_unreadable(previous, stage, error.to_string(), cause)
                    .await;
            }
        };
        let observed = match conn.command("status").await {
            Ok(status) => parse_loop_status(&status).map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        };

        match observed {
            Ok((repeat, single))
                if stage != "single" && repeat == target.0 && single == target.1 =>
            {
                Ok(())
            }
            Ok((repeat, single)) if repeat == previous.0 && single == previous.1 => {
                Err(MediaError::Backend(format!(
                    "mpd {stage} update failed; loop mode unchanged: {cause}"
                )))
            }
            Ok((repeat, single)) => {
                if repeat != previous.0 {
                    conn.command(&format!("repeat {}", previous.0)).await.map_err(|error| {
                        MediaError::Backend(format!(
                            "mpd {stage} update failed; loop mode rollback failed: {cause}; {error}"
                        ))
                    })?;
                }
                if single != previous.1 {
                    conn.command(&format!("single {}", previous.1)).await.map_err(|error| {
                        MediaError::Backend(format!(
                            "mpd {stage} update failed; loop mode rollback failed: {cause}; {error}"
                        ))
                    })?;
                }
                Err(MediaError::Backend(format!(
                    "mpd {stage} update failed; loop mode was rolled back: {cause}"
                )))
            }
            Err(error) => {
                self.restore_loop_after_unreadable(previous, stage, error, cause)
                    .await
            }
        }
    }

    async fn restore_loop_after_unreadable(
        &self,
        previous: (u8, &str),
        stage: &str,
        status_error: String,
        cause: MediaError,
    ) -> Result<(), MediaError> {
        let restore = match self.conn().await {
            Ok(mut conn) => match conn.command(&format!("repeat {}", previous.0)).await {
                Ok(_) => conn
                    .command(&format!("single {}", previous.1))
                    .await
                    .map(|_| ()),
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        match restore {
            Ok(()) => Err(MediaError::Backend(format!(
                "mpd {stage} update failed; status could not be re-read ({status_error}); restored previous loop mode: {cause}"
            ))),
            Err(error) => Err(MediaError::Backend(format!(
                "mpd {stage} update failed; status could not be re-read ({status_error}); restore failed: {cause}; {error}"
            ))),
        }
    }

    fn caps_from_command(&self, name: &str) -> bool {
        self.commands.iter().any(|command| command == name)
    }

    /// Open a push watcher on its own connection.
    pub async fn watch(&self) -> Result<MpdWatch, MediaError> {
        let conn = self.conn().await?;
        Ok(MpdWatch { conn: Some(conn) })
    }
}

fn parse_loop_status(status: &[Pair]) -> Result<(u8, String), MediaError> {
    let repeat = match mpd_parse::field(status, "repeat") {
        Some("0") => 0,
        Some("1") => 1,
        other => {
            return Err(MediaError::Backend(format!(
                "mpd status has an unknown repeat flag {:?}",
                other.map(sanitize_diag)
            )));
        }
    };
    let single = match mpd_parse::field(status, "single") {
        Some("0") => "0",
        Some("1") => "1",
        Some("oneshot") => "oneshot",
        other => {
            return Err(MediaError::Backend(format!(
                "mpd status has an unknown single flag {:?}",
                other.map(sanitize_diag)
            )));
        }
    };
    Ok((repeat, single.to_string()))
}

fn caps_from_commands(commands: &[Pair]) -> MediaCaps {
    let has = |name: &str| {
        commands
            .iter()
            .any(|(key, value)| key == "command" && value == name)
    };
    let repeat_single = has("status") && has("repeat") && has("single");
    MediaCaps {
        shuffle: has("random"),
        loop_mode: repeat_single,
        volume: has("status") && has("setvol"),
        playlists: false,
        signals: has("idle"),
        seek: has("seekcur"),
        art: false,
        queue: false,
        abs_volume: has("setvol"),
        chapters: false,
        fullscreen: false,
    }
}

/// Observed volume from a `status` reply: `Ok(None)` when MPD reports none
/// (no mixer: field absent or the `-1` sentinel); `Err` when malformed.
fn parse_volume(status: &[Pair]) -> Result<Option<u8>, MediaError> {
    let Some(v) = mpd_parse::field(status, "volume") else {
        return Ok(None);
    };
    match v.trim().parse::<i64>() {
        Ok(n) if n < 0 => Ok(None),
        Ok(n) => Ok(Some(n.clamp(0, 100) as u8)),
        Err(_) => Err(MediaError::Backend(
            "mpd status has a malformed volume".into(),
        )),
    }
}

/// Compute a relative volume target from an observed base. Pure.
fn relative_volume(cur: u8, delta: f64) -> u8 {
    (f64::from(cur) + delta * 100.0).clamp(0.0, 100.0).round() as u8
}

impl MediaBackend for Mpd {
    fn snapshot(&self) -> BoxFuture<'_, Result<Option<MediaState>, MediaError>> {
        Box::pin(async move {
            let mut c = self.conn().await?;
            let status = c.command("status").await?;
            match mpd_parse::field(&status, "state") {
                Some("play" | "pause" | "stop") => {}
                other => {
                    return Err(MediaError::Backend(format!(
                        "mpd status has an unknown state {:?}",
                        other.map(sanitize_diag)
                    )));
                }
            }
            // Nothing loaded (`state: stop` with no song) still yields a valid
            // stopped state; the badge hides it. A failed `currentsong` is an
            // error, not an empty song.
            let song = c.command("currentsong").await?;
            let mut state = mpd_parse::to_state(&status, &song);
            state.can_go_next &= self.caps_from_command("next");
            state.can_go_previous &= self.caps_from_command("previous");
            state.can_seek &= self.caps.seek && state.state.is_active();
            Ok(Some(state))
        })
    }

    fn play_pause(&self) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // `pause` with no argument toggles pause; from stopped, `play` starts.
            let mut c = self.conn().await?;
            let status = c.command("status").await?;
            // A missing or unknown state must not select an arbitrary mutation.
            match mpd_parse::field(&status, "state") {
                Some("stop") => c.command("play").await.map(|_| ()),
                Some("play" | "pause") => c.command("pause").await.map(|_| ()),
                other => Err(MediaError::Backend(format!(
                    "mpd status has an unknown state {:?}",
                    other.map(sanitize_diag)
                ))),
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
                LoopMode::None => (0, "0"),
                LoopMode::Playlist => (1, "0"),
                LoopMode::Track => (1, "1"),
            };
            let (previous, stage, cause) = {
                let mut c = self.conn().await?;
                let status = c.command("status").await?;
                let previous = parse_loop_status(&status)?;
                if let Err(error) = c.command(&format!("repeat {repeat}")).await {
                    (previous, "repeat", error)
                } else if let Err(error) = c.command(&format!("single {single}")).await {
                    (previous, "single", error)
                } else {
                    return Ok(());
                }
            };
            self.reconcile_loop(
                (repeat, single),
                (previous.0, previous.1.as_str()),
                stage,
                cause,
            )
            .await
        })
    }
    fn volume_step(&self, delta: f64) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            // One connection: observe, then write. Never write from a guess.
            let mut c = self.conn().await?;
            let status = c.command("status").await?;
            let Some(cur) = parse_volume(&status)? else {
                return Err(MediaError::Unsupported("mpd volume is unavailable".into()));
            };
            let next = relative_volume(cur, delta);
            c.command(&format!("setvol {next}")).await.map(|_| ())
        })
    }

    fn playlists(&self) -> BoxFuture<'_, Result<Vec<crate::model::Playlist>, MediaError>> {
        Box::pin(async {
            Err(MediaError::Unsupported(
                "mpd playlists are not implemented".into(),
            ))
        })
    }
    fn activate_playlist<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<(), MediaError>> {
        Box::pin(async move {
            Err(MediaError::Unsupported(
                "mpd playlist activation is not implemented".into(),
            ))
        })
    }

    fn seek(&self, offset: Duration, forward: bool) -> BoxFuture<'_, Result<(), MediaError>> {
        Box::pin(async move {
            let sign = if forward { '+' } else { '-' };
            self.conn()
                .await?
                .command(&format!("seekcur {sign}{:.3}", offset.as_secs_f64()))
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
                .command(&format!("seekcur {:.3}", pos.as_secs_f64()))
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
    fn players(&self) -> BoxFuture<'_, Result<Vec<String>, MediaError>> {
        Box::pin(async { Ok(vec!["mpd".to_string()]) })
    }

    /// The `idle` push watcher.
    fn watch(&self) -> BoxFuture<'_, Result<Option<Box<dyn MediaWatch + Send>>, MediaError>> {
        Box::pin(async move {
            match Mpd::watch(self).await {
                Ok(w) => Ok(Some(Box::new(w) as Box<dyn MediaWatch + Send>)),
                Err(e) => {
                    tracing::debug!(target: "thegn::media", error = %e, "MPD watcher unavailable");
                    Err(e)
                }
            }
        })
    }

    fn caps(&self) -> MediaCaps {
        self.caps
    }
}

/// A push watcher blocked on MPD's `idle` command. Each `changed()` starts
/// `idle player mixer options` once and resumes the same response after future
/// cancellation. A dropped connection ends the stream.
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
            match conn.idle_changed().await {
                Ok(()) => {
                    tracing::debug!(target: "thegn::media", "MPD idle tick");
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
        let ep = MpdEndpoint::resolve("127.0.0.1:6600", None).unwrap();
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
        )
        .unwrap();
        let rendered = format!("{endpoint:?}");
        assert!(!rendered.contains(sentinel));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn resolve_explicit_host_port() {
        let ep = MpdEndpoint::resolve("music.lan:6601", None).unwrap();
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
        match MpdEndpoint::resolve("music.lan", None).unwrap().kind {
            EndpointKind::Tcp { host, port } => {
                assert_eq!(host, "music.lan");
                assert_eq!(port, 6600);
            }
            _ => panic!("expected tcp"),
        }
    }

    #[test]
    fn resolve_unix_path() {
        match MpdEndpoint::resolve("/run/mpd/socket", None).unwrap().kind {
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
        MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None).unwrap()
    }

    #[tokio::test]
    async fn oversized_greeting_is_rejected() {
        let ep = fake_server(vec![b'x'; 4096], Vec::new()).await;
        assert!(MpdConn::dial(&ep).await.is_err());
    }

    #[tokio::test]
    async fn invalid_password_is_rejected_without_echoing_it() {
        let ep = fake_server(b"OK MPD 0.23.0\n".to_vec(), Vec::new()).await;
        let ep = MpdEndpoint::resolve(
            &format!(
                "127.0.0.1:{}",
                match ep.kind {
                    EndpointKind::Tcp { port, .. } => port,
                    EndpointKind::Unix(_) => unreachable!(),
                }
            ),
            Some(crate::MpdPassword::new("secret\nvalue".into())),
        )
        .unwrap();
        let err = MpdConn::dial(&ep).await.err().unwrap().to_string();
        assert!(err.contains("forbidden control character"));
        assert!(!err.contains("secret"));
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
    async fn oversized_reply_line_is_discarded_and_later_rows_survive() {
        let reply = format!(
            "comment: {}\nTitle: visible\nOK\n",
            "x".repeat(MAX_LINE_BYTES + 100)
        );
        let ep = fake_server(b"OK MPD 0.23.0\n".to_vec(), reply.into_bytes()).await;
        let mut c = MpdConn::dial(&ep).await.unwrap();
        let rows = c.command("currentsong").await.unwrap();
        assert_eq!(mpd_parse::field(&rows, "Title"), Some("visible"));
        assert_eq!(rows.len(), 1);
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

    #[tokio::test]
    async fn idle_resume_after_cancellation_does_not_send_a_second_frame() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (idle_tx, idle_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (r, mut w) = tokio::io::split(sock);
            w.write_all(b"OK MPD 0.23.0\n").await.unwrap();
            let mut r = BufReader::new(r);
            let first = read_bounded_line(&mut r, 1024).await.unwrap().unwrap();
            idle_tx
                .send(first)
                .expect("idle receiver remains alive until the first frame arrives");
            let second =
                tokio::time::timeout(Duration::from_millis(100), read_bounded_line(&mut r, 1024))
                    .await;
            w.write_all(b"changed: player\nOK\n").await.unwrap();
            second
        });
        let ep = MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None).unwrap();
        let watch = MpdWatch {
            conn: Some(MpdConn::dial(&ep).await.unwrap()),
        };
        let mut watch = watch;
        async fn cancel_after_write(
            watch: &mut MpdWatch,
            idle_rx: tokio::sync::oneshot::Receiver<String>,
        ) {
            let changed = watch.changed();
            tokio::pin!(changed);
            tokio::select! {
                _ = &mut changed => panic!("idle unexpectedly completed"),
                first = idle_rx => assert_eq!(first.unwrap(), "idle player mixer options"),
            }
        }
        cancel_after_write(&mut watch, idle_rx).await;
        assert!(watch.changed().await);
        assert!(
            server.await.unwrap().is_err(),
            "server received a duplicate idle frame"
        );
    }

    fn tcp(s: &str) -> Result<(String, u16), MpdEndpointError> {
        match MpdEndpoint::resolve(s, None)?.kind {
            EndpointKind::Tcp { host, port } => Ok((host, port)),
            EndpointKind::Unix(_) => panic!("expected tcp"),
        }
    }

    #[test]
    fn endpoint_parser_table() {
        assert_eq!(tcp("[::1]:6601"), Ok(("::1".into(), 6601)));
        assert_eq!(tcp("[::1]"), Ok(("::1".into(), 6600)));
        assert_eq!(tcp("host:65535"), Ok(("host".into(), 65535)));
        assert!(matches!(
            tcp("host:0"),
            Err(MpdEndpointError::InvalidPort(_))
        ));
        assert!(matches!(
            tcp("host:65536"),
            Err(MpdEndpointError::InvalidPort(_))
        ));
        assert!(matches!(
            tcp("host:abc"),
            Err(MpdEndpointError::InvalidPort(_))
        ));
        assert!(matches!(
            tcp("host:"),
            Err(MpdEndpointError::InvalidPort(_))
        ));
        assert_eq!(tcp(":6600"), Err(MpdEndpointError::EmptyHost));
        assert!(matches!(
            tcp("::1"),
            Err(MpdEndpointError::AmbiguousHost(_))
        ));
        assert!(matches!(tcp("[::1"), Err(MpdEndpointError::InvalidIpv6(_))));
        assert!(matches!(
            tcp("[nope]:1"),
            Err(MpdEndpointError::InvalidIpv6(_))
        ));
        assert!(matches!(
            tcp("[::1]x"),
            Err(MpdEndpointError::InvalidIpv6(_))
        ));
        assert!(matches!(
            tcp("a b:1"),
            Err(MpdEndpointError::InvalidHost(_))
        ));
        assert_eq!(tcp("   ").unwrap_err(), MpdEndpointError::Empty);
        assert!(MpdEndpoint::resolve("/run/mpd/socket", None).is_ok());
    }

    #[test]
    fn volume_parse_never_fabricates() {
        let p = |v: &str| mpd_parse::parse_line(&format!("volume: {v}")).unwrap();
        assert_eq!(parse_volume(&[]).unwrap(), None);
        assert_eq!(parse_volume(&[p("-1")]).unwrap(), None);
        assert_eq!(parse_volume(&[p("40")]).unwrap(), Some(40));
        assert!(parse_volume(&[p("loud")]).is_err());
        assert_eq!(relative_volume(40, 0.05), 45);
        assert_eq!(relative_volume(98, 0.05), 100);
    }

    #[test]
    fn command_capabilities_require_each_advertised_command() {
        let pairs = |names: &[&str]| {
            names
                .iter()
                .map(|name| ("command".to_string(), (*name).to_string()))
                .collect::<Vec<_>>()
        };
        let caps = caps_from_commands(&pairs(&[
            "status", "setvol", "random", "repeat", "single", "seekcur", "idle",
        ]));
        assert!(caps.volume && caps.abs_volume);
        assert!(caps.shuffle && caps.loop_mode && caps.seek && caps.signals);
        assert!(!caps.playlists && !caps.queue && !caps.art);
        let caps = caps_from_commands(&pairs(&["status", "play", "repeat", "seekcur"]));
        assert!(!caps.volume && !caps.abs_volume);
        assert!(!caps.loop_mode && !caps.shuffle && !caps.signals);
        assert!(caps.seek);
        let caps = caps_from_commands(&pairs(&["repeat", "single"]));
        assert!(!caps.loop_mode);
    }

    /// Fake server that answers every command with `reply` and records them.
    async fn recording_server(
        reply: &'static str,
    ) -> (MpdEndpoint, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = tokio::io::split(sock);
            if w.write_all(b"OK MPD 0.23.0\n").await.is_err() {
                return;
            }
            let mut r = BufReader::new(r);
            while let Ok(Some(l)) = read_bounded_line(&mut r, 1024).await {
                log.lock().unwrap().push(l);
                if w.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        (
            MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None).unwrap(),
            seen,
        )
    }

    /// Fake server that gives each received command its corresponding reply.
    async fn scripted_server(
        replies: Vec<&'static str>,
    ) -> (MpdEndpoint, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let index = std::sync::Arc::new(std::sync::Mutex::new(0));
            while let Ok((sock, _)) = listener.accept().await {
                let (r, mut w) = tokio::io::split(sock);
                if w.write_all(b"OK MPD 0.23.0\n").await.is_err() {
                    continue;
                }
                let mut r = BufReader::new(r);
                let log = log.clone();
                let replies = replies.clone();
                let index = index.clone();
                tokio::spawn(async move {
                    while let Ok(Some(line)) = read_bounded_line(&mut r, 1024).await {
                        log.lock().unwrap().push(line);
                        let reply = {
                            let mut next = index.lock().unwrap();
                            let reply = replies.get(*next).copied().unwrap_or("OK\n");
                            *next += 1;
                            reply
                        };
                        if w.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (
            MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None).unwrap(),
            seen,
        )
    }

    /// Fake server that leaves `single` unanswered on the first connection,
    /// then answers the rollback only on a newly accepted connection.
    async fn single_timeout_server() -> (MpdEndpoint, std::sync::Arc<std::sync::Mutex<Vec<String>>>)
    {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = tokio::io::split(sock);
            if w.write_all(b"OK MPD 0.23.0\n").await.is_err() {
                return;
            }
            let first_log = log.clone();
            tokio::spawn(async move {
                let mut r = BufReader::new(r);
                while let Ok(Some(line)) = read_bounded_line(&mut r, 1024).await {
                    first_log.lock().unwrap().push(line.clone());
                    match line.as_str() {
                        "status" => {
                            if w.write_all(b"repeat: 0\nsingle: 0\nOK\n").await.is_err() {
                                return;
                            }
                        }
                        "repeat 1" if w.write_all(b"OK\n").await.is_err() => return,
                        "repeat 1" => {}
                        _ => {}
                    }
                }
            });

            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = tokio::io::split(sock);
            if w.write_all(b"OK MPD 0.23.0\n").await.is_err() {
                return;
            }
            let mut r = BufReader::new(r);
            while let Ok(Some(line)) = read_bounded_line(&mut r, 1024).await {
                log.lock().unwrap().push(line.clone());
                let reply = match line.as_str() {
                    "status" => "repeat: 1\nsingle: 0\nOK\n",
                    "repeat 0" => "OK\n",
                    _ => continue,
                };
                if let Err(error) = w.write_all(reply.as_bytes()).await {
                    tracing::debug!(target: "thegn::media", %error, "fake MPD rollback response failed");
                    return;
                }
            }
        });
        (
            MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None).unwrap(),
            seen,
        )
    }

    /// Fake server that applies `repeat 1` but withholds its reply, then reports
    /// that committed value to the fresh reconciliation connection.
    async fn repeat_timeout_server() -> (MpdEndpoint, std::sync::Arc<std::sync::Mutex<Vec<String>>>)
    {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = tokio::io::split(sock);
            if w.write_all(b"OK MPD 0.23.0\n").await.is_err() {
                return;
            }
            let mut r = BufReader::new(r);
            // Keep the socket open after `repeat 1`; the timeout must come from
            // the client deadline rather than an early EOF.
            while let Ok(Some(line)) = read_bounded_line(&mut r, 1024).await {
                log.lock().unwrap().push(line.clone());
                if line == "status" && w.write_all(b"repeat: 0\nsingle: 0\nOK\n").await.is_err() {
                    return;
                }
            }

            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            let (r, mut w) = tokio::io::split(sock);
            if w.write_all(b"OK MPD 0.23.0\n").await.is_err() {
                return;
            }
            let mut r = BufReader::new(r);
            while let Ok(Some(line)) = read_bounded_line(&mut r, 1024).await {
                log.lock().unwrap().push(line.clone());
                let reply = match line.as_str() {
                    "status" => "repeat: 1\nsingle: 0\nOK\n",
                    "repeat 0" => "OK\n",
                    _ => continue,
                };
                if w.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
        (
            MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None).unwrap(),
            seen,
        )
    }

    #[tokio::test]
    async fn volume_step_without_observed_volume_writes_nothing() {
        let (ep, seen) = recording_server("volume: -1\nstate: play\nOK\n").await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let err = m.volume_step(0.05).await.unwrap_err();
        assert!(matches!(err, MediaError::Unsupported(_)), "{err:?}");
        let cmds = seen.lock().unwrap().clone();
        assert_eq!(cmds, vec!["status".to_string()]);
    }

    #[tokio::test]
    async fn volume_step_writes_relative_to_observed_on_one_connection() {
        let (ep, seen) = recording_server("volume: 40\nstate: play\nOK\n").await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        m.volume_step(0.05).await.unwrap();
        let cmds = seen.lock().unwrap().clone();
        assert_eq!(cmds, vec!["status".to_string(), "setvol 45".to_string()]);
    }

    #[tokio::test]
    async fn play_pause_with_missing_state_does_not_mutate() {
        let (ep, seen) = recording_server("volume: 40\nOK\n").await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        assert!(m.play_pause().await.is_err());
        assert_eq!(seen.lock().unwrap().clone(), vec!["status".to_string()]);
    }

    #[tokio::test]
    async fn stopped_snapshot_does_not_advertise_seek() {
        let (ep, _) = recording_server("state: stop\nOK\n").await;
        let m = Mpd {
            ep,
            caps: MediaCaps {
                seek: true,
                ..MediaCaps::default()
            },
            commands: vec!["seekcur".into()],
        };

        let snapshot = m.snapshot().await.unwrap().unwrap();
        assert!(!snapshot.can_seek);
    }

    #[tokio::test]
    async fn activate_playlist_is_unsupported() {
        let (ep, _seen) = recording_server("OK\n").await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        assert!(matches!(
            m.playlists().await,
            Err(MediaError::Unsupported(_))
        ));
        assert!(matches!(
            m.activate_playlist("x").await,
            Err(MediaError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn connect_discovers_conservative_command_capabilities() {
        let (ep, _seen) = recording_server("command: status\ncommand: play\nOK\n").await;
        let m = Mpd::connect(ep).await.unwrap();
        assert!(!m.caps().volume);
        assert!(!m.caps().seek);
        assert!(!m.caps().signals);
    }

    #[tokio::test]
    async fn snapshot_rejects_unknown_state() {
        let (ep, _seen) = recording_server("volume: 40\nOK\n").await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        assert!(matches!(m.snapshot().await, Err(MediaError::Backend(_))));
    }

    #[tokio::test]
    async fn snapshot_reports_daemon_disappearance() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let ep = MpdEndpoint::resolve(&format!("127.0.0.1:{port}"), None).unwrap();
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        assert!(matches!(
            m.snapshot().await,
            Err(MediaError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn set_loop_rolls_back_repeat_when_single_fails() {
        let (ep, seen) = scripted_server(vec![
            "repeat: 0\nsingle: 0\nOK\n",
            "OK\n",
            "ACK [4@0] {single} denied\n",
            "repeat: 1\nsingle: 0\nOK\n",
            "OK\n",
        ])
        .await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let error = m.set_loop(LoopMode::Track).await.unwrap_err();
        assert!(error.to_string().contains("rolled back"), "{error}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "status".to_string(),
                "repeat 1".to_string(),
                "single 1".to_string(),
                "status".to_string(),
                "repeat 0".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_loop_rolls_back_on_a_fresh_connection_after_single_timeout() {
        // COMMAND_TIMEOUT is fixed at ten seconds, so exercising the timeout
        // path costs ten seconds rather than shortening the production budget.
        let (ep, seen) = single_timeout_server().await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let error = m.set_loop(LoopMode::Track).await.unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(error.to_string().contains("rolled back"), "{error}");
        assert!(!error.to_string().contains("rollback failed"), "{error}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "status".to_string(),
                "repeat 1".to_string(),
                "single 1".to_string(),
                "status".to_string(),
                "repeat 0".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_loop_with_unknown_repeat_does_not_mutate() {
        let (ep, seen) = scripted_server(vec!["single: 0\nOK\n"]).await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        assert!(matches!(
            m.set_loop(LoopMode::Track).await,
            Err(MediaError::Backend(_))
        ));
        assert_eq!(seen.lock().unwrap().clone(), vec!["status".to_string()]);
    }

    #[tokio::test]
    async fn set_loop_repeat_ack_with_unchanged_status_reports_unchanged() {
        let (ep, seen) = scripted_server(vec![
            "repeat: 0\nsingle: 0\nOK\n",
            "ACK [4@0] {repeat} denied\n",
            "repeat: 0\nsingle: 0\nOK\n",
        ])
        .await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let error = m.set_loop(LoopMode::Track).await.unwrap_err();
        assert!(error.to_string().contains("unchanged"), "{error}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "status".to_string(),
                "repeat 1".to_string(),
                "status".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_loop_repeat_timeout_that_committed_is_rolled_back() {
        let (ep, seen) = repeat_timeout_server().await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let error = m.set_loop(LoopMode::Track).await.unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(error.to_string().contains("rolled back"), "{error}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "status".to_string(),
                "repeat 1".to_string(),
                "status".to_string(),
                "repeat 0".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_loop_single_ack_preserves_oneshot_and_restores_repeat() {
        let (ep, seen) = scripted_server(vec![
            "repeat: 0\nsingle: oneshot\nOK\n",
            "OK\n",
            "ACK [4@0] {single} denied\n",
            "repeat: 1\nsingle: 0\nOK\n",
            "OK\n",
            "OK\n",
        ])
        .await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let error = m.set_loop(LoopMode::Playlist).await.unwrap_err();
        assert!(error.to_string().contains("rolled back"), "{error}");
        assert!(error.to_string().contains("single} denied"), "{error}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "status".to_string(),
                "repeat 1".to_string(),
                "single 0".to_string(),
                "status".to_string(),
                "repeat 0".to_string(),
                "single oneshot".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_loop_unreadable_status_reports_cause_and_stops_after_failed_restore_repeat() {
        let (ep, seen) = scripted_server(vec![
            "repeat: 0\nsingle: 0\nOK\n",
            "OK\n",
            "ACK [4@0] {single} denied\n",
            "ACK [4@0] {status} unavailable\n",
            "ACK [4@0] {repeat} denied\n",
        ])
        .await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let error = m.set_loop(LoopMode::Track).await.unwrap_err();
        assert!(
            error.to_string().contains("status could not be re-read"),
            "{error}"
        );
        assert!(error.to_string().contains("unavailable"), "{error}");
        assert!(error.to_string().contains("single} denied"), "{error}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "status".to_string(),
                "repeat 1".to_string(),
                "single 1".to_string(),
                "status".to_string(),
                "repeat 0".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn set_loop_with_unknown_single_does_not_mutate() {
        let (ep, seen) = scripted_server(vec!["repeat: 0\nsingle: 2\nOK\n"]).await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        assert!(matches!(
            m.set_loop(LoopMode::Track).await,
            Err(MediaError::Backend(_))
        ));
        assert_eq!(seen.lock().unwrap().clone(), vec!["status".to_string()]);
    }

    #[tokio::test]
    async fn set_loop_single_failure_observing_target_restores_and_reports_cause() {
        let (ep, seen) = scripted_server(vec![
            "repeat: 0\nsingle: 0\nOK\n",
            "OK\n",
            "ACK [4@0] {single} late\n",
            "repeat: 1\nsingle: 1\nOK\n",
            "OK\n",
            "OK\n",
        ])
        .await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        let error = m.set_loop(LoopMode::Track).await.unwrap_err();
        assert!(error.to_string().contains("single} late"), "{error}");
        assert!(error.to_string().contains("rolled back"), "{error}");
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "status".to_string(),
                "repeat 1".to_string(),
                "single 1".to_string(),
                "status".to_string(),
                "repeat 0".to_string(),
                "single 0".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn seek_preserves_subsecond_precision() {
        let (ep, seen) = recording_server("OK\n").await;
        let m = Mpd {
            ep,
            caps: MediaCaps::default(),
            commands: vec![],
        };
        m.seek(Duration::from_millis(1500), true).await.unwrap();
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec!["seekcur +1.500".to_string()]
        );
    }
}
