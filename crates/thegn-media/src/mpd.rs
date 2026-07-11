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

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::model::{LoopMode, MediaState};
use crate::mpd_parse::{self, Pair};
use crate::{MediaBackend, MediaCaps, MediaError, MediaWatch};

/// Any bidirectional MPD transport (TCP or unix socket), boxed so one code path
/// drives both.
trait Duplex: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Duplex for T {}

/// Where the MPD daemon lives, plus an optional password.
#[derive(Debug, Clone)]
pub struct MpdEndpoint {
    kind: EndpointKind,
    password: Option<String>,
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
    /// Resolve a `[media.mpd] socket` string into an endpoint. A leading `/`
    /// (unix) is a socket path; otherwise `host:port` (default port 6600). When
    /// `socket` is empty or the default `127.0.0.1:6600`, `$MPD_HOST`/`$MPD_PORT`
    /// override — matching how every MPD client resolves its connection.
    pub fn resolve(socket: &str, password: Option<String>) -> MpdEndpoint {
        let trimmed = socket.trim();
        let is_default = trimmed.is_empty() || trimmed == "127.0.0.1:6600";
        let mut password = password;

        if is_default
            && let Ok(host) = std::env::var("MPD_HOST")
            && !host.is_empty()
        {
            // `MPD_HOST` may carry a `password@host` prefix.
            let (pw, host) = match host.split_once('@') {
                // Leading `@host` = abstract socket, not a password.
                Some((maybe_pw, rest)) if !maybe_pw.is_empty() => {
                    (Some(maybe_pw.to_string()), rest.to_string())
                }
                _ => (None, host.clone()),
            };
            if password.is_none() {
                password = pw;
            }
            let kind = if host.starts_with('/') {
                EndpointKind::Unix(host)
            } else {
                let port = std::env::var("MPD_PORT")
                    .ok()
                    .and_then(|p| p.trim().parse().ok())
                    .unwrap_or(6600);
                EndpointKind::Tcp { host, port }
            };
            return MpdEndpoint { kind, password };
        }

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
        // Greeting: `OK MPD <version>`.
        let mut line = String::new();
        conn.io
            .read_line(&mut line)
            .await
            .map_err(|e| MediaError::Unavailable(format!("mpd greeting: {e}")))?;
        if !line.starts_with("OK MPD") {
            return Err(MediaError::Backend(format!(
                "unexpected MPD greeting: {line:?}"
            )));
        }
        if let Some(pw) = &ep.password {
            // Best-effort auth; a rejection surfaces on the first real command.
            conn.command(&format!("password {}", quote(pw))).await?;
        }
        Ok(conn)
    }

    /// Run one command and collect its `key: value` reply up to the terminating
    /// `OK` (an `ACK …` line becomes a [`MediaError::Backend`]).
    async fn command(&mut self, cmd: &str) -> Result<Vec<Pair>, MediaError> {
        let line = format!("{cmd}\n");
        self.io
            .get_mut()
            .write_all(line.as_bytes())
            .await
            .map_err(|e| MediaError::Backend(format!("mpd write: {e}")))?;
        let mut pairs = Vec::new();
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self
                .io
                .read_line(&mut buf)
                .await
                .map_err(|e| MediaError::Backend(format!("mpd read: {e}")))?;
            if n == 0 {
                return Err(MediaError::Backend("mpd closed the connection".into()));
            }
            let t = buf.trim_end_matches(['\r', '\n']);
            if t == "OK" {
                return Ok(pairs);
            }
            if let Some(err) = t.strip_prefix("ACK") {
                return Err(MediaError::Backend(format!("mpd ACK{err}")));
            }
            if let Some(p) = mpd_parse::parse_line(t) {
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
fn quote(s: &str) -> String {
    if s.chars().any(|c| c == ' ' || c == '"' || c == '\\') {
        let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        s.to_string()
    }
}

/// The native MPD backend. Cloneable endpoint; dials per read.
pub struct Mpd {
    ep: MpdEndpoint,
}

impl Mpd {
    /// Connect once to verify the daemon answers (so a dead endpoint doesn't sit
    /// in the aggregator), then keep the endpoint for per-call dials.
    pub async fn connect(ep: MpdEndpoint) -> Result<Mpd, MediaError> {
        // Probe: a successful dial (greeting + optional password) is enough.
        let _ = MpdConn::dial(&ep).await?;
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
    async fn snapshot(&self) -> Result<Option<MediaState>, MediaError> {
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
    }

    async fn play_pause(&self) -> Result<(), MediaError> {
        // `pause` with no argument toggles pause; from stopped, `play` starts.
        let mut c = self.conn().await?;
        let status = c.command("status").await?;
        match mpd_parse::field(&status, "state").unwrap_or("stop") {
            "stop" => c.command("play").await.map(|_| ()),
            _ => c.command("pause").await.map(|_| ()),
        }
    }
    async fn next(&self) -> Result<(), MediaError> {
        self.conn().await?.command("next").await.map(|_| ())
    }
    async fn previous(&self) -> Result<(), MediaError> {
        self.conn().await?.command("previous").await.map(|_| ())
    }
    async fn set_shuffle(&self, on: bool) -> Result<(), MediaError> {
        self.conn()
            .await?
            .command(&format!("random {}", on as u8))
            .await
            .map(|_| ())
    }
    async fn set_loop(&self, mode: LoopMode) -> Result<(), MediaError> {
        let (repeat, single) = match mode {
            LoopMode::None => (0, 0),
            LoopMode::Playlist => (1, 0),
            LoopMode::Track => (1, 1),
        };
        let mut c = self.conn().await?;
        c.command(&format!("repeat {repeat}")).await?;
        c.command(&format!("single {single}")).await.map(|_| ())
    }
    async fn volume_step(&self, delta: f64) -> Result<(), MediaError> {
        let cur = self.current_volume().await as f64;
        let next = (cur + delta * 100.0).clamp(0.0, 100.0).round() as u8;
        self.conn()
            .await?
            .command(&format!("setvol {next}"))
            .await
            .map(|_| ())
    }

    async fn playlists(&self) -> Result<Vec<crate::model::Playlist>, MediaError> {
        Ok(Vec::new()) // stored playlists deferred
    }
    async fn activate_playlist(&self, _id: &str) -> Result<(), MediaError> {
        Ok(())
    }

    async fn seek(&self, offset: Duration, forward: bool) -> Result<(), MediaError> {
        let sign = if forward { '+' } else { '-' };
        self.conn()
            .await?
            .command(&format!("seekcur {sign}{}", offset.as_secs()))
            .await
            .map(|_| ())
    }
    async fn set_position(&self, pos: Duration, _track_id: Option<&str>) -> Result<(), MediaError> {
        self.conn()
            .await?
            .command(&format!("seekcur {}", pos.as_secs()))
            .await
            .map(|_| ())
    }
    async fn set_volume(&self, level: u8) -> Result<(), MediaError> {
        self.conn()
            .await?
            .command(&format!("setvol {}", level.min(100)))
            .await
            .map(|_| ())
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
            match conn.command("idle player mixer options").await {
                Ok(_) => true,
                Err(_) => {
                    // Connection lost — end the stream so the host reconnects.
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
        // Env-free default. (MPD_HOST may exist in a dev shell; guard the assert.)
        if std::env::var_os("MPD_HOST").is_none() {
            let ep = MpdEndpoint::resolve("127.0.0.1:6600", None);
            match ep.kind {
                EndpointKind::Tcp { host, port } => {
                    assert_eq!(host, "127.0.0.1");
                    assert_eq!(port, 6600);
                }
                _ => panic!("expected tcp"),
            }
        }
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
}
