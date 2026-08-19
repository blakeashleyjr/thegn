//! Local daemon IPC: unix-domain sockets on unix, named pipes on Windows —
//! one seam so the daemon, control client, and `axum::serve` are
//! platform-free.
//!
//! **The endpoint is the lock.** On unix, whoever binds the socket is the
//! daemon (a connectable socket ⇒ a live daemon; a stale file is unlinked).
//! On Windows, `first_pipe_instance(true)` gives the same semantics — the
//! first creator owns the pipe name, a second daemon gets `ACCESS_DENIED`
//! (⇒ [`BindOutcome::AlreadyRunning`]), and pipes die with the process, so
//! there is no stale-file case at all.
//!
//! Pipe names are derived from the same per-state-dir socket *path* the unix
//! side uses (`\\.\pipe\thegn-<hex(sha256(path))[..16]>`), so the
//! one-daemon-per-`$XDG_STATE_HOME` isolation (`just start`, smoke tests, the
//! "this shell runs inside a live thegn" gotcha) carries over unchanged. The
//! derivation and endpoint classification are pure and unit-tested on every
//! platform; only the syscalls are `#[cfg]`-gated.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// The Windows named-pipe namespace prefix.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Deterministic pipe name for a daemon socket path. Hashed (not sanitized)
/// so arbitrarily long/exotic state-dir paths always yield a valid, collision-
/// resistant pipe name.
pub fn pipe_name_for_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("{PIPE_PREFIX}thegn-{hex}")
}

/// Where local daemon IPC lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcEndpoint {
    /// Unix-domain socket path.
    Unix(PathBuf),
    /// Windows named pipe (`\\.\pipe\…`).
    Pipe(String),
}

impl IpcEndpoint {
    /// Classify a stored/configured "socket path" into an endpoint for this
    /// platform. A `\\.\pipe\…` string (e.g. a `DaemonRow.endpoint` written by
    /// a Windows daemon) is already a pipe name; any other path is a unix
    /// socket on unix and is *derived into* a pipe name on Windows.
    pub fn for_socket_path(path: &Path) -> Self {
        Self::classify(path, cfg!(windows))
    }

    /// [`Self::for_socket_path`] with the platform explicit — pure, so both
    /// arms are unit-tested on Linux CI.
    fn classify(path: &Path, windows: bool) -> Self {
        let s = path.to_string_lossy();
        if s.starts_with(PIPE_PREFIX) {
            IpcEndpoint::Pipe(s.into_owned())
        } else if windows {
            IpcEndpoint::Pipe(pipe_name_for_path(path))
        } else {
            IpcEndpoint::Unix(path.to_path_buf())
        }
    }

    /// The stable string form — what the daemon registry row stores and log
    /// lines print.
    pub fn display(&self) -> String {
        match self {
            IpcEndpoint::Unix(p) => p.to_string_lossy().into_owned(),
            IpcEndpoint::Pipe(name) => name.clone(),
        }
    }
}

/// One connected IPC stream (either side, either platform).
pub enum IpcStream {
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    PipeClient(tokio::net::windows::named_pipe::NamedPipeClient),
    #[cfg(windows)]
    PipeServer(tokio::net::windows::named_pipe::NamedPipeServer),
}

macro_rules! delegate {
    ($self:ident, $inner:ident => $e:expr) => {
        match $self.get_mut() {
            #[cfg(unix)]
            IpcStream::Unix($inner) => $e,
            #[cfg(windows)]
            IpcStream::PipeClient($inner) => $e,
            #[cfg(windows)]
            IpcStream::PipeServer($inner) => $e,
        }
    };
}

impl AsyncRead for IpcStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_read(cx, buf))
    }
}

impl AsyncWrite for IpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        delegate!(self, s => Pin::new(s).poll_write(cx, buf))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_flush(cx))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        delegate!(self, s => Pin::new(s).poll_shutdown(cx))
    }
}

/// Connect to a (presumed live) daemon endpoint.
pub async fn connect(ep: &IpcEndpoint) -> io::Result<IpcStream> {
    match ep {
        IpcEndpoint::Unix(path) => {
            #[cfg(unix)]
            {
                Ok(IpcStream::Unix(
                    tokio::net::UnixStream::connect(path).await?,
                ))
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                Err(unsupported("unix-socket IPC on a non-unix host"))
            }
        }
        IpcEndpoint::Pipe(name) => {
            #[cfg(windows)]
            {
                use tokio::net::windows::named_pipe::ClientOptions;
                // ERROR_PIPE_BUSY (231): instances exist but none is free —
                // the server is alive and about to create the next instance,
                // so a short bounded backoff (~127ms worst case) is correct.
                // Unknown-name (daemon gone) errors surface immediately.
                const ERROR_PIPE_BUSY: i32 = 231;
                let mut delay_ms = 1u64;
                loop {
                    match ClientOptions::new().open(name) {
                        Ok(c) => return Ok(IpcStream::PipeClient(c)),
                        Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && delay_ms <= 64 => {
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                            delay_ms *= 2;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let _ = name;
                Err(unsupported("named-pipe IPC on a non-Windows host"))
            }
        }
    }
}

fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

/// Take the advisory lock serializing [`IpcListener::bind_exclusive`]'s
/// probe→unlink→bind critical section for `sock`. The sidecar `<sock>.lock`
/// is created once and NEVER unlinked (unlinking it would resurrect the very
/// race it closes); the flock (std's `File::lock`) dies with the process, so
/// it can't go stale. Contention lasts a probe + a bind (milliseconds), and
/// the caller is already on a blocking thread. Best-effort: `None` (exotic
/// fs, permissions) degrades to the old raced path rather than refusing to
/// serve.
#[cfg(unix)]
fn bind_lock(sock: &Path) -> Option<std::fs::File> {
    let mut path = sock.as_os_str().to_owned();
    path.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(std::path::Path::new(&path))
        .ok()?;
    file.lock().ok()?;
    Some(file)
}

/// Outcome of [`IpcListener::bind_exclusive`]: the caller either *is* the
/// daemon or found a live one.
pub enum BindOutcome {
    Bound(IpcListener),
    AlreadyRunning,
}

/// The daemon's listening endpoint (and single-instance lock).
pub enum IpcListener {
    #[cfg(unix)]
    Unix(tokio::net::UnixListener),
    #[cfg(windows)]
    Pipe {
        name: String,
        /// The pre-created next server instance. Always `Some` between
        /// accepts so a client connecting concurrently never sees
        /// file-not-found; recreated after each hand-off.
        next: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    },
}

impl IpcListener {
    /// Bind the endpoint, treating it as the single-daemon lock (see the
    /// module docs). `AlreadyRunning` is the spawn-race loser's clean exit.
    pub async fn bind_exclusive(ep: &IpcEndpoint) -> io::Result<BindOutcome> {
        match ep {
            IpcEndpoint::Unix(sock) => {
                #[cfg(unix)]
                {
                    // A connectable socket ⇒ a live daemon; a stale file
                    // (bind would fail with AddrInUse) is unlinked. The whole
                    // probe→unlink→bind sequence runs under the advisory bind
                    // lock (see [`bind_lock`]) on a blocking thread — two
                    // racing binders could otherwise BOTH probe-fail the same
                    // stale socket, and the loser's `remove_file` would unlink
                    // the winner's freshly-bound live socket (unlink succeeds
                    // on bound sockets), stranding every future client.
                    // `ensure_daemon`'s lazy spawn makes concurrent binders
                    // over one stale socket the expected case, not a freak.
                    enum UnixBind {
                        Bound(std::os::unix::net::UnixListener),
                        AlreadyRunning,
                    }
                    let sock = sock.clone();
                    let decision = tokio::task::spawn_blocking(move || -> io::Result<UnixBind> {
                        let _lock = bind_lock(&sock);
                        if sock.exists() {
                            match std::os::unix::net::UnixStream::connect(&sock) {
                                Ok(_) => return Ok(UnixBind::AlreadyRunning),
                                Err(_) => {
                                    let _ = std::fs::remove_file(&sock);
                                }
                            }
                        }
                        match std::os::unix::net::UnixListener::bind(&sock) {
                            Ok(l) => {
                                // Owner-only (0600) on the socket: the local control
                                // plane grants admin to any connector, so the umask
                                // must not leave it cross-user-connectable. On Linux a
                                // 0600 socket inode denies connect(2) to other uids —
                                // defense in depth for the state-dir fallback path
                                // (no XDG_RUNTIME_DIR). Best-effort: a chmod failure
                                // must not down the daemon.
                                let _ = thegn_core::fsperm::restrict_to_owner(&sock);
                                l.set_nonblocking(true)?;
                                Ok(UnixBind::Bound(l))
                            }
                            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                                Ok(UnixBind::AlreadyRunning)
                            }
                            Err(e) => Err(e),
                        }
                    })
                    .await
                    .map_err(io::Error::other)??;
                    match decision {
                        UnixBind::Bound(l) => Ok(BindOutcome::Bound(IpcListener::Unix(
                            tokio::net::UnixListener::from_std(l)?,
                        ))),
                        UnixBind::AlreadyRunning => Ok(BindOutcome::AlreadyRunning),
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = sock;
                    Err(unsupported("unix-socket IPC on a non-unix host"))
                }
            }
            IpcEndpoint::Pipe(name) => {
                #[cfg(windows)]
                {
                    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
                    // ERROR_ACCESS_DENIED (5): an instance of the name already
                    // exists. That is NOT the same as "a daemon is running" —
                    // it is also what you get while the previous daemon's last
                    // handle is still being reclaimed, so treating it as
                    // AlreadyRunning made a restarting daemon exit instead of
                    // taking over. Probe the way the unix arm does (connect and
                    // see) rather than inferring liveness from the create error.
                    const ERROR_FILE_NOT_FOUND: i32 = 2;
                    const ERROR_ACCESS_DENIED: i32 = 5;
                    // ~250ms total. Only ever paid on daemon startup, and only
                    // when an instance exists but no server answers; a live
                    // daemon always keeps a free instance pre-created (see
                    // `accept_stream`), so it is detected on the first probe.
                    const BACKOFF_MS: [u64; 6] = [1, 2, 8, 30, 80, 128];
                    for (attempt, delay) in BACKOFF_MS.iter().enumerate() {
                        match ServerOptions::new()
                            .first_pipe_instance(true)
                            .reject_remote_clients(true)
                            .create(name)
                        {
                            Ok(server) => {
                                return Ok(BindOutcome::Bound(IpcListener::Pipe {
                                    name: name.clone(),
                                    next: Some(server),
                                }));
                            }
                            Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                                match ClientOptions::new().open(name) {
                                    // Someone is listening ⇒ a real daemon owns
                                    // the name. (This costs the daemon one
                                    // accept of an immediately-closed
                                    // connection — exactly what the unix probe
                                    // costs, and it already tolerates that.)
                                    Ok(_probe) => return Ok(BindOutcome::AlreadyRunning),
                                    // The name vanished between our create and
                                    // the probe: the old owner finished dying.
                                    // Retry the create at once.
                                    Err(pe) if pe.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => {
                                        continue;
                                    }
                                    // Instances exist but none is free, or the
                                    // probe failed some other way — ambiguous
                                    // (mid-handoff daemon, or teardown). Back
                                    // off and re-decide rather than guessing.
                                    Err(pe) => {
                                        if attempt + 1 == BACKOFF_MS.len() {
                                            let _ = pe;
                                            // Exhausted: assume a live daemon.
                                            // Refusing to start is recoverable;
                                            // stealing a live daemon's pipe is
                                            // not.
                                            return Ok(BindOutcome::AlreadyRunning);
                                        }
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            *delay,
                                        ))
                                        .await;
                                    }
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    }
                    // Every create returned ACCESS_DENIED and every probe said
                    // the name had vanished — pathological; be conservative.
                    Ok(BindOutcome::AlreadyRunning)
                }
                #[cfg(not(windows))]
                {
                    let _ = name;
                    Err(unsupported("named-pipe IPC on a non-Windows host"))
                }
            }
        }
    }

    /// Accept one connection. (Named `accept_stream` so the inherent method
    /// doesn't shadow `axum::serve::Listener::accept`.)
    pub async fn accept_stream(&mut self) -> io::Result<IpcStream> {
        match self {
            #[cfg(unix)]
            IpcListener::Unix(l) => Ok(IpcStream::Unix(l.accept().await?.0)),
            #[cfg(windows)]
            IpcListener::Pipe { name, next } => {
                use tokio::net::windows::named_pipe::ServerOptions;
                let server = match next.take() {
                    Some(s) => s,
                    None => ServerOptions::new()
                        .reject_remote_clients(true)
                        .create(&*name)?,
                };
                server.connect().await?;
                // Pre-create the successor before handing this one out.
                *next = ServerOptions::new()
                    .reject_remote_clients(true)
                    .create(&*name)
                    .ok();
                Ok(IpcStream::PipeServer(server))
            }
        }
    }

    /// The endpoint's stable string form (registry row / log lines).
    pub fn endpoint_display(&self) -> String {
        match self {
            #[cfg(unix)]
            IpcListener::Unix(l) => l
                .local_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.to_string_lossy().into_owned()))
                .unwrap_or_default(),
            #[cfg(windows)]
            IpcListener::Pipe { name, .. } => name.clone(),
        }
    }
}

/// `axum::serve` integration: the trait's `accept` is infallible, so transient
/// accept errors are logged and retried (matching axum's own built-in
/// listener impls).
impl axum::serve::Listener for IpcListener {
    type Io = IpcStream;
    type Addr = String;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.accept_stream().await {
                Ok(stream) => return (stream, self.endpoint_display()),
                Err(e) => {
                    tracing::warn!(target: "thegn::daemon", "ipc accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.endpoint_display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_name_is_deterministic_prefixed_and_short() {
        let a = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let b = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let c = pipe_name_for_path(Path::new("/tmp/other/thegn/daemon.sock"));
        assert_eq!(a, b, "same path ⇒ same pipe name");
        assert_ne!(a, c, "different state dirs ⇒ different pipes (isolation)");
        assert!(a.starts_with(r"\\.\pipe\thegn-"), "{a}");
        // prefix + "thegn-" + 16 hex chars — comfortably inside the 256-char
        // pipe-name limit regardless of the input path length.
        assert_eq!(a.len(), PIPE_PREFIX.len() + "thegn-".len() + 16);
    }

    #[test]
    fn classify_routes_by_prefix_then_platform() {
        let sock = Path::new("/run/user/1000/thegn/daemon.sock");
        assert_eq!(
            IpcEndpoint::classify(sock, false),
            IpcEndpoint::Unix(sock.to_path_buf())
        );
        assert_eq!(
            IpcEndpoint::classify(sock, true),
            IpcEndpoint::Pipe(pipe_name_for_path(sock))
        );
        // A stored pipe name (DaemonRow.endpoint from a Windows daemon) is
        // recognized as-is on either platform — discovery round-trips.
        let pipe = Path::new(r"\\.\pipe\thegn-0011223344556677");
        for windows in [false, true] {
            assert_eq!(
                IpcEndpoint::classify(pipe, windows),
                IpcEndpoint::Pipe(pipe.to_string_lossy().into_owned())
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_bind_is_the_lock_and_round_trips() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let dir = std::env::temp_dir().join(format!("thegn-ipc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ep = IpcEndpoint::for_socket_path(&dir.join("d.sock"));

        // First bind wins…
        let mut listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(l) => l,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        // …the second sees a live daemon.
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::AlreadyRunning
        ));
        // That liveness probe connected once; drain it from the backlog so the
        // round-trip below accepts the real client.
        drop(listener.accept_stream().await.unwrap());

        // Round-trip a byte each way through connect/accept.
        let client = tokio::spawn({
            let ep = ep.clone();
            async move {
                let mut c = connect(&ep).await.unwrap();
                c.write_all(b"hi").await.unwrap();
                let mut buf = [0u8; 2];
                c.read_exact(&mut buf).await.unwrap();
                buf
            }
        });
        let mut server_side = listener.accept_stream().await.unwrap();
        let mut buf = [0u8; 2];
        server_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
        server_side.write_all(b"ok").await.unwrap();
        assert_eq!(&client.await.unwrap(), b"ok");

        // A stale file (dead daemon) is unlinked and re-bound.
        drop(server_side);
        drop(listener);
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::Bound(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The stale-socket TOCTOU: N binders racing one stale socket file must
    /// elect exactly one daemon, and the losers' probe/unlink path must not
    /// strip the winner's freshly-bound socket out of the filesystem. Without
    /// the `<sock>.lock` serialization this is a race (unlink succeeds on
    /// bound sockets); with it the outcome is deterministic.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn racing_binders_on_a_stale_socket_elect_exactly_one_daemon() {
        let dir = std::env::temp_dir().join(format!("thegn-ipc-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("d.sock");
        // A dead daemon's leftover: a socket file nothing is listening on.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let ep = IpcEndpoint::for_socket_path(&path);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let ep = ep.clone();
            tasks.push(tokio::spawn(async move {
                IpcListener::bind_exclusive(&ep).await
            }));
        }
        let mut bound = Vec::new();
        let mut already = 0usize;
        for t in tasks {
            match t.await.unwrap().unwrap() {
                BindOutcome::Bound(l) => bound.push(l),
                BindOutcome::AlreadyRunning => already += 1,
            }
        }
        assert_eq!(bound.len(), 1, "exactly one binder may win the socket");
        assert_eq!(already, 7);
        // The winner's socket survived the losers' stale-file handling.
        connect(&ep)
            .await
            .expect("the surviving socket accepts connections");
        drop(bound);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// TEMPORARY DIAGNOSTIC — delete once the pipe-rebind fix is confirmed.
    /// Windows cannot be run locally, so this reports the raw OS behaviour of
    /// the rebind-after-drop path by panicking with the findings; the panic
    /// message is the only channel that reaches the CI log.
    #[cfg(windows)]
    #[tokio::test]
    async fn diag_pipe_rebind_after_drop() {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};
        let name = format!(r"\\.\pipe\thegn-diag-{}", std::process::id());
        let mut r = String::new();
        let probe = |tag: &str, out: &mut String| {
            let e = match ClientOptions::new().open(&name) {
                Ok(_c) => "client-open=OK(connected)".to_string(),
                Err(e) => format!("client-open=Err({:?})", e.raw_os_error()),
            };
            out.push_str(&format!("\n  [{tag}] {e}"));
        };
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name);
        r.push_str(&format!("create#1 first_instance -> {:?}", first.is_ok()));
        let first = first.unwrap();
        // Second create must be ACCESS_DENIED (5) while #1 lives.
        let second = ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name);
        r.push_str(&format!(
            "\ncreate#2 while#1-alive -> {:?}",
            second.as_ref().err().map(|e| e.raw_os_error())
        ));
        drop(second);
        probe("instance alive", &mut r);
        // Now drop every handle and immediately retry the first-instance create,
        // then retry with escalating sleeps to measure any teardown lag.
        drop(first);
        for attempt in 0..6u32 {
            let again = ServerOptions::new()
                .first_pipe_instance(true)
                .reject_remote_clients(true)
                .create(&name);
            match again {
                Ok(s) => {
                    r.push_str(&format!("\nrebind OK after {attempt} retries"));
                    drop(s);
                    break;
                }
                Err(e) => {
                    r.push_str(&format!(
                        "\nrebind attempt {attempt} -> {:?}",
                        e.raw_os_error()
                    ));
                    probe("after drop", &mut r);
                    tokio::time::sleep(std::time::Duration::from_millis(10 << attempt)).await;
                }
            }
        }
        panic!("DIAG(rebind-after-drop): {r}");
    }

    /// TEMPORARY DIAGNOSTIC — as above, but through the real
    /// bind_exclusive/accept_stream path, which is what the failing test uses.
    #[cfg(windows)]
    #[tokio::test]
    async fn diag_rebind_after_accept_cycle() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let ep = IpcEndpoint::Pipe(format!(r"\\.\pipe\thegn-diag2-{}", std::process::id()));
        let mut r = String::new();
        let mut listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(l) => l,
            BindOutcome::AlreadyRunning => panic!("fresh pipe must bind"),
        };
        let client = tokio::spawn({
            let ep = ep.clone();
            async move {
                let mut c = connect(&ep).await.unwrap();
                c.write_all(b"hi").await.unwrap();
                let mut buf = [0u8; 2];
                c.read_exact(&mut buf).await.unwrap();
                buf
            }
        });
        let mut server_side = listener.accept_stream().await.unwrap();
        let mut buf = [0u8; 2];
        server_side.read_exact(&mut buf).await.unwrap();
        server_side.write_all(b"ok").await.unwrap();
        let _ = client.await.unwrap();
        r.push_str("round-trip ok");
        drop(server_side);
        r.push_str("; dropped server_side");
        drop(listener);
        r.push_str("; dropped listener");
        for attempt in 0..6u32 {
            match IpcListener::bind_exclusive(&ep).await.unwrap() {
                BindOutcome::Bound(l) => {
                    r.push_str(&format!("; REBOUND after {attempt} retries"));
                    drop(l);
                    break;
                }
                BindOutcome::AlreadyRunning => {
                    r.push_str(&format!("; attempt {attempt} = AlreadyRunning"));
                    tokio::time::sleep(std::time::Duration::from_millis(10 << attempt)).await;
                }
            }
        }
        panic!("DIAG(accept-cycle): {r}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn pipe_bind_is_the_lock_and_round_trips() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        // Unique per test process so parallel CI runs can't collide.
        let ep = IpcEndpoint::Pipe(format!(r"\\.\pipe\thegn-test-{}", std::process::id()));

        let mut listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(l) => l,
            BindOutcome::AlreadyRunning => panic!("fresh pipe must bind"),
        };
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::AlreadyRunning
        ));

        let client = tokio::spawn({
            let ep = ep.clone();
            async move {
                let mut c = connect(&ep).await.unwrap();
                c.write_all(b"hi").await.unwrap();
                let mut buf = [0u8; 2];
                c.read_exact(&mut buf).await.unwrap();
                buf
            }
        });
        let mut server_side = listener.accept_stream().await.unwrap();
        let mut buf = [0u8; 2];
        server_side.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hi");
        server_side.write_all(b"ok").await.unwrap();
        assert_eq!(&client.await.unwrap(), b"ok");

        // Pipes die with their handles: dropping the listener frees the name.
        drop(server_side);
        drop(listener);
        assert!(matches!(
            IpcListener::bind_exclusive(&ep).await.unwrap(),
            BindOutcome::Bound(_)
        ));
    }
}
