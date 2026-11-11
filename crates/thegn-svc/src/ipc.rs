//! Local daemon IPC: unix-domain sockets on unix, named pipes on Windows —
//! one seam so the daemon, control client, and `axum::serve` are
//! platform-free.
//!
//! **The endpoint is the lock.** On unix, whoever binds the socket is the
//! daemon (a socket that accepts or has a full backlog ⇒ a live daemon; only
//! a refused socket is stale and unlinked). Other probe outcomes fail startup.
//! On Windows, `first_pipe_instance(true)` gives the same semantics — the
//! first creator owns the pipe name and a second daemon gets `ACCESS_DENIED`
//! (⇒ [`BindOutcome::AlreadyRunning`]). Pipes die with their handles, so
//! there is no stale *file* to clean up — but the name stays reserved for a
//! few milliseconds after the last handle of an instance that carried a
//! connection closes, so `bind_exclusive` retries briefly before believing
//! ACCESS_DENIED (see the comment there).
//!
//! Pipe names are derived from the same per-state-dir socket *path* the unix
//! side uses (`\\.\pipe\thegn-v2-<hex(sha256(identity))[..32]>`). The
//! `thegn/daemon-pipe/v2\0` domain and 128-bit digest prefix bind the name to a
//! normalized Windows path identity (Unix callers use the socket path itself);
//! display strings are diagnostic only and are never identity keys. The
//! derivation and endpoint classification are pure and unit-tested on every
//! platform; only the syscalls are `#[cfg]`-gated.
//! A pre-upgrade Windows daemon keeps its old pipe name and is reached through
//! its registry row; after it stops, new daemons use the v2 name.
//!
//! Unix endpoint authenticity is bound to the daemon's effective uid and the
//! filesystem namespace containing its owner-only socket parent. Other
//! principals cannot replace the endpoint or lock there. A same-uid process
//! in that namespace is the same filesystem principal, so token-only
//! deployments must keep this private parent outside sandbox mounts. Migrate
//! custom token-only sockets into a private per-user runtime/state directory.
//! Lock contention and socket probes fail closed after the same bounded wait.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// OS-authenticated identity attached to an accepted local-control stream.
/// This is created by the listener, never from request headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerIdentity {
    /// Effective uid returned by the Unix peer-credential syscall.
    UnixEuid(u32),
    /// A local-only Windows named pipe (`reject_remote_clients(true)`).
    LocalPipe,
    /// A daemon-internal dispatch whose caller already authenticated it.
    TrustedInternal,
    /// The platform could not authenticate this peer. Such a peer must use a
    /// normal scoped token even when implicit local admin is enabled.
    Unavailable(String),
}

/// Connection metadata propagated to HTTP/WebSocket and gRPC request
/// extensions by `into_make_service_with_connect_info`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IpcConnectInfo {
    pub endpoint: String,
    pub peer: PeerIdentity,
}

impl IpcConnectInfo {
    /// Identity for daemon-internal calls routed through the HTTP adapters.
    pub fn trusted_internal() -> Self {
        Self {
            endpoint: "in-process".into(),
            peer: PeerIdentity::TrustedInternal,
        }
    }
}

/// The daemon's effective uid on Unix. `None` on platforms without Unix uid
/// credentials; kept here so platform-specific identity knowledge has one
/// home.
pub fn effective_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(unsafe { libc::geteuid() })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// The Windows named-pipe namespace prefix.
pub const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Deterministic pipe name for a daemon socket path. Its SHA-256 digest is
/// domain-separated with `thegn/daemon-pipe/v2\0` and truncated to 128 bits.
/// Valid UTF-8 paths use normalized Windows spelling and Unicode lowercase;
/// non-UTF-8 paths retain their raw encoded bytes. Display strings are
/// diagnostic only and are never identity keys.
pub fn pipe_name_for_path(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"thegn/daemon-pipe/v2\0");
    hasher.update(pipe_identity_bytes(path));
    let digest = hasher.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{PIPE_PREFIX}thegn-v2-{hex}")
}

/// Lossless identity bytes with Windows path spelling normalized where the
/// encoded path is valid UTF-8. Raw encoded bytes preserve non-UTF-8 identities.
fn pipe_identity_bytes(path: &Path) -> Vec<u8> {
    let encoded = path.as_os_str().as_encoded_bytes();
    let Ok(path) = std::str::from_utf8(encoded) else {
        return encoded.to_vec();
    };

    let path = path.replace('/', "\\");
    let path = if let Some(verbatim_path) = path.strip_prefix(r"\\?\") {
        let bytes = verbatim_path.as_bytes();
        if bytes.len() >= 4 && bytes[..3].eq_ignore_ascii_case(b"UNC") && bytes[3] == b'\\' {
            format!(r"\\{}", &verbatim_path[4..])
        } else {
            verbatim_path.to_owned()
        }
    } else {
        path
    };
    path.to_lowercase().into_bytes()
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
                connect_unix_as(
                    path,
                    effective_uid().ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "cannot determine effective uid",
                        )
                    })?,
                )
                .await
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
                let _ = name; // best-effort: non-Result discard — param unused in this stub
                Err(unsupported("named-pipe IPC on a non-Windows host"))
            }
        }
    }
}

#[cfg(unix)]
async fn connect_unix_as(path: &Path, expected_euid: u32) -> io::Result<IpcStream> {
    verify_unix_client_parent(path)?;
    connect_unix_after_parent_check(path, expected_euid).await
}

#[cfg(unix)]
fn verify_unix_client_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::PermissionDenied, "unix socket has no parent")
    })?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symlink", parent.display()),
        ));
    }
    verify_unix_bind_parent(parent, &metadata)
}

#[cfg(unix)]
async fn connect_unix_after_parent_check(path: &Path, expected_euid: u32) -> io::Result<IpcStream> {
    let stream = tokio::net::UnixStream::connect(path).await?;
    let peer_uid = stream.peer_cred()?.uid();
    server_peer_allowed(Some(peer_uid), expected_euid)?;
    Ok(IpcStream::Unix(stream))
}

#[cfg(unix)]
fn server_peer_allowed(peer_uid: Option<u32>, euid: u32) -> io::Result<()> {
    if peer_uid == Some(euid) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "unix control server uid {:?} does not match client uid {euid}",
                peer_uid
            ),
        ))
    }
}

fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what.to_string())
}

#[derive(Debug)]
#[cfg_attr(not(unix), allow(dead_code))] // Unix-only production call sites.
struct IoPathContext {
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
}

impl std::fmt::Display for IoPathContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for IoPathContext {
    // `Display` already renders the cause; exposing it again as `source()`
    // makes anyhow's chain print it twice.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

#[cfg_attr(not(unix), allow(dead_code))] // Unix-only production call sites.
fn io_error_with_path_context(
    source: io::Error,
    operation: &'static str,
    path: &Path,
) -> io::Error {
    let kind = source.kind();
    io::Error::new(
        kind,
        IoPathContext {
            operation,
            path: path.to_path_buf(),
            source,
        },
    )
}

/// Take the advisory lock serializing [`IpcListener::bind_exclusive`]'s
/// probe→unlink→bind critical section for `sock`. The sidecar `<sock>.lock`
/// is only ever unlinked by the control-dir prune, which holds the lock while
/// unlinking; every binder re-checks after acquiring the flock that the path
/// still names the inode it locked and reopens otherwise, so two binders can
/// never hold different inodes for one endpoint. The flock (std's
/// `File::lock`) dies with the process, so it can't go stale. Acquisition is bounded by [`BIND_LOCK_WAIT`]; a held
/// lock that exceeds the bound is a startup error, never a wait without end.
/// The caller is already on a blocking thread. Failure is fatal: probing or
/// unlinking without this lock can race another daemon endpoint.
#[cfg(unix)]
fn bind_lock(sock: &Path) -> io::Result<std::fs::File> {
    bind_lock_with_wait(sock, BIND_LOCK_WAIT)
}

/// Verify the control socket's parent directory, then take the bind lock. An
/// idle-prune pass can remove an empty control directory between the parent
/// check and the lock open (or a second pruner can race the first), which
/// surfaces as `NotFound`; recreate the directory once and retry. A second
/// failure is returned as is (fail closed).
#[cfg(unix)]
fn bind_parent_and_lock(sock: &Path) -> io::Result<std::fs::File> {
    bind_parent_and_lock_with(
        sock,
        &mut |parent| prepare_control_directory(parent, false),
        &mut || {},
    )
}

#[cfg(unix)]
fn bind_parent_and_lock_with(
    sock: &Path,
    ensure_dir: &mut dyn FnMut(&Path) -> io::Result<()>,
    after_parent_check: &mut dyn FnMut(),
) -> io::Result<std::fs::File> {
    let parent = sock.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket has no parent directory",
        )
    })?;
    match bind_parent_attempt(sock, parent, after_parent_check) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tracing::debug!(
                target: "thegn::daemon",
                path = %parent.display(),
                "control directory vanished before the bind lock; recreating once"
            );
            ensure_dir(parent)?;
            bind_parent_attempt(sock, parent, after_parent_check)
        }
        other => other,
    }
}

#[cfg(unix)]
fn bind_parent_attempt(
    sock: &Path,
    parent: &Path,
    after_parent_check: &mut dyn FnMut(),
) -> io::Result<std::fs::File> {
    let parent_metadata = std::fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() {
        return Err(io_error_with_path_context(
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control socket parent must not be a symlink",
            ),
            "verify control socket parent",
            parent,
        ));
    }
    verify_unix_bind_parent(parent, &parent_metadata)?;
    after_parent_check();
    bind_lock(sock)
}

#[cfg(unix)]
/// Upper bound for control socket lock contention. The host's
/// `SCOPE_PROBE_TIMEOUT` planner budget is 5 s; honest contention is measured
/// in milliseconds, so this bound leaves room for startup work without an
/// unbounded wait.
const BIND_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The listener probe shares the bind-lock budget; a healthy probe completes
/// in microseconds.
#[cfg(unix)]
const BIND_PROBE_WAIT: std::time::Duration = BIND_LOCK_WAIT;

#[cfg(unix)]
const BIND_LOCK_REOPEN_LIMIT: usize = 16;

#[cfg(unix)]
fn bind_lock_path(sock: &Path) -> PathBuf {
    let mut path = sock.as_os_str().to_owned();
    path.push(".lock");
    PathBuf::from(path)
}

#[cfg(unix)]
fn bind_lock_with_wait(sock: &Path, wait: std::time::Duration) -> io::Result<std::fs::File> {
    bind_lock_checked(sock, wait, &mut || {})
}

#[cfg(unix)]
fn open_bind_lock_file(sock: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

    let path = bind_lock_path(sock);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|error| io_error_with_path_context(error, "open control socket lock", sock))?;
    let mut metadata = file
        .metadata()
        .map_err(|error| io_error_with_path_context(error, "inspect control socket lock", sock))?;
    if !metadata.is_file() || metadata.uid() != effective_uid().unwrap_or_default() {
        let mode = metadata.mode() & 0o777;
        return Err(io_error_with_path_context(
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "control socket lock must be an owner-owned regular file (found uid {}, mode {mode:04o})",
                    metadata.uid()
                ),
            ),
            "verify control socket lock",
            sock,
        ));
    }
    if metadata.mode() & 0o777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| {
                io_error_with_path_context(error, "restrict control socket lock", sock)
            })?;
        metadata = file.metadata().map_err(|error| {
            io_error_with_path_context(error, "recheck control socket lock", sock)
        })?;
        if !metadata.is_file()
            || metadata.uid() != effective_uid().unwrap_or_default()
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(io_error_with_path_context(
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "control socket lock could not be restricted to owner mode 0600",
                ),
                "verify control socket lock",
                sock,
            ));
        }
    }
    Ok(file)
}

/// True when `lock_path` still names the very inode `file` holds (and it is
/// still linked), so a flock on `file` is the endpoint's one lock domain.
#[cfg(unix)]
fn bind_lock_is_current(file: &std::fs::File, lock_path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let (Ok(held), Ok(named)) = (file.metadata(), std::fs::symlink_metadata(lock_path)) else {
        return false;
    };
    held.nlink() > 0
        && named.file_type().is_file()
        && held.dev() == named.dev()
        && held.ino() == named.ino()
}

/// Open and flock the sidecar, then verify the path still names the locked
/// inode; a lock unlinked or replaced in between is dropped and reopened.
/// `after_open` runs after each open, before the flock (a test seam).
#[cfg(unix)]
fn bind_lock_checked(
    sock: &Path,
    wait: std::time::Duration,
    after_open: &mut dyn FnMut(),
) -> io::Result<std::fs::File> {
    let lock_path = bind_lock_path(sock);
    let deadline = std::time::Instant::now() + wait;
    let mut interval = thegn_proc::POLL_INTERVAL;
    for _attempt in 0..BIND_LOCK_REOPEN_LIMIT {
        let file = open_bind_lock_file(sock)?;
        after_open();
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(io_error_with_path_context(
                        error,
                        "lock control socket",
                        sock,
                    ));
                }
            }
            let remaining = match deadline.checked_duration_since(std::time::Instant::now()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => {
                    return Err(io_error_with_path_context(
                        io::Error::new(
                            io::ErrorKind::WouldBlock,
                            format!(
                                "control socket lock is held by another process (waited {}ms)",
                                wait.as_millis()
                            ),
                        ),
                        "lock control socket",
                        sock,
                    ));
                }
            };
            std::thread::sleep(interval.min(remaining));
            interval = thegn_proc::next_backoff(interval, remaining);
        }
        if bind_lock_is_current(&file, &lock_path) {
            return Ok(file);
        }
        drop(file);
    }
    Err(io_error_with_path_context(
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "control socket lock kept being replaced",
        ),
        "lock control socket",
        sock,
    ))
}

#[cfg_attr(not(unix), allow(dead_code))] // Unix-only production call site.
fn require_live_endpoint(
    bind_error: io::Error,
    probe: io::Result<()>,
    sock: &Path,
) -> io::Result<()> {
    match probe {
        Ok(()) => Ok(()),
        Err(_) => Err(io_error_with_path_context(
            bind_error,
            "bind control socket",
            sock,
        )),
    }
}

/// Idle floor before another state scope's control directory may be pruned
/// (the same 24 h the host's /tmp-fallback reclaim uses).
#[cfg(unix)]
const CONTROL_DIR_PRUNE_MIN_IDLE: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);
/// Directories examined per bind: bounds how long a binder holds its own lock.
#[cfg(unix)]
const CONTROL_DIR_PRUNE_MAX_EXAMINED: usize = 64;

/// A state-endpoint key directory name: exactly 32 lowercase hex digits.
#[cfg(unix)]
fn is_control_key_dir_name(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Prune abandoned `<runtime_root>/<key>/` control directories: owner-only,
/// idle for `min_idle`, holding nothing but an unheld `daemon.sock.lock`, or
/// that lock plus a stale `daemon.sock` (a dead daemon's leftover). `probe` is
/// the liveness check of a leftover socket: only a refused connection proves a
/// socket stale. The socket (if any) then the lock are unlinked and the empty
/// directory is left for a later pass.
/// Best effort (every failure is logged and skipped), never follows a symlink,
/// and unlinks a lock only while holding its flock and after re-verifying the
/// path still names the locked inode (binders re-check the same way). Returns
/// how many entries (locks, or empty directories) were removed.
#[cfg(unix)]
fn prune_idle_control_dirs(
    runtime_root: &Path,
    own_dir: &Path,
    min_idle: std::time::Duration,
    max_examined: usize,
    probe: &dyn Fn(&Path) -> io::Result<()>,
) -> usize {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _};

    // Every per-candidate operation below is path-based, so the root itself
    // must be a real, owner-owned, non-group/world-writable directory: nobody
    // else can then swap a vetted key directory for a symlink under us.
    let Some(uid) = effective_uid() else {
        return 0;
    };
    match std::fs::symlink_metadata(runtime_root) {
        Ok(root_meta)
            if root_meta.file_type().is_dir()
                && root_meta.uid() == uid
                && root_meta.mode() & 0o022 == 0 => {}
        Ok(_) => {
            tracing::debug!(target: "thegn::daemon", root = %runtime_root.display(), "control prune: runtime root is not a private real directory");
            return 0;
        }
        Err(error) => {
            tracing::debug!(target: "thegn::daemon", %error, "control prune: cannot inspect runtime root");
            return 0;
        }
    }
    let entries = match std::fs::read_dir(runtime_root) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(target: "thegn::daemon", %error, "control prune: cannot list runtime root");
            return 0;
        }
    };
    let own_name = own_dir.file_name();
    let mut examined = 0_usize;
    let mut removed = 0_usize;
    for entry in entries {
        if examined >= max_examined {
            break;
        }
        let Ok(entry) = entry else { continue };
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !is_control_key_dir_name(name) || own_name.is_some_and(|own| own == file_name) {
            continue;
        }
        examined += 1;
        let dir = entry.path();
        let Ok(dir_meta) = std::fs::symlink_metadata(&dir) else {
            continue;
        };
        if !dir_meta.file_type().is_dir()
            || dir_meta.uid() != uid
            || dir_meta.mode() & 0o777 != 0o700
            || !dir_meta
                .modified()
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age >= min_idle)
        {
            continue;
        }
        let Ok(children) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names = Vec::new();
        for child in children {
            match child {
                Ok(child) => names.push(child.file_name()),
                Err(_) => {
                    names.clear();
                    names.push(std::ffi::OsString::from("?"));
                    break;
                }
            }
        }
        let mut sock_candidate = false;
        match names.as_slice() {
            [] => {
                match std::fs::remove_dir(&dir) {
                    Ok(()) => removed += 1,
                    Err(error) => {
                        tracing::debug!(target: "thegn::daemon", %error, dir = %dir.display(), "control prune: cannot remove empty directory");
                    }
                }
                continue;
            }
            [only] if only == "daemon.sock.lock" => {}
            names
                if names.len() == 2
                    && names.iter().any(|n| n == "daemon.sock")
                    && names.iter().any(|n| n == "daemon.sock.lock") =>
            {
                sock_candidate = true;
            }
            _ => continue,
        }
        let lock = dir.join("daemon.sock.lock");
        let Ok(named) = std::fs::symlink_metadata(&lock) else {
            continue;
        };
        if !named.file_type().is_file() || named.nlink() != 1 || named.uid() != uid {
            continue;
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&lock)
        {
            Ok(file) => file,
            Err(error) => {
                tracing::debug!(target: "thegn::daemon", %error, lock = %lock.display(), "control prune: cannot open lock");
                continue;
            }
        };
        let Ok(held) = file.metadata() else { continue };
        if held.dev() != named.dev() || held.ino() != named.ino() {
            continue;
        }
        // A holder (live daemon or binder) or any lock error means: leave it.
        if file.try_lock().is_err() {
            continue;
        }
        let still_named = std::fs::symlink_metadata(&lock)
            .is_ok_and(|now| now.dev() == held.dev() && now.ino() == held.ino());
        if !still_named {
            continue;
        }
        if sock_candidate {
            // Lock held: nobody can bind here meanwhile. Only a socket that is
            // ours, singly linked and refusing connections is a dead daemon's.
            let sock = dir.join("daemon.sock");
            let Ok(sock_named) = std::fs::symlink_metadata(&sock) else {
                continue;
            };
            if !sock_named.file_type().is_socket()
                || sock_named.uid() != uid
                || sock_named.nlink() != 1
            {
                continue;
            }
            match classify_unix_probe(probe(&sock), &sock) {
                Ok(UnixProbe::Stale) => {}
                Ok(UnixProbe::Live | UnixProbe::Gone) | Err(_) => continue,
            }
            let same_socket = std::fs::symlink_metadata(&sock)
                .is_ok_and(|now| now.dev() == sock_named.dev() && now.ino() == sock_named.ino());
            if !same_socket {
                continue;
            }
            match std::fs::remove_file(&sock) {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::debug!(target: "thegn::daemon", %error, sock = %sock.display(), "control prune: cannot remove stale socket");
                    continue;
                }
            }
        }
        match std::fs::remove_file(&lock) {
            // The now-empty directory is deliberately left in place: a binder
            // that opened this lock before we unlinked it re-opens it after its
            // inode recheck fails, and must not find its parent gone. The
            // unlink refreshes the directory mtime, so a later pass removes the
            // empty directory only after another full idle period.
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::debug!(target: "thegn::daemon", %error, lock = %lock.display(), "control prune: cannot remove lock");
            }
        }
    }
    removed
}

/// Classification of a non-blocking probe of an existing Unix socket.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
enum UnixProbe {
    Live,
    Stale,
    Gone,
}

/// A probe is live when it connects or the accept backlog is full. Only
/// `ConnectionRefused` proves that the socket is stale; unclassified failures
/// carry path context and must never cause the endpoint to be unlinked.
#[cfg_attr(not(unix), allow(dead_code))]
fn classify_unix_probe(probe: io::Result<()>, sock: &Path) -> io::Result<UnixProbe> {
    match probe {
        Ok(()) => Ok(UnixProbe::Live),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(UnixProbe::Live),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => Ok(UnixProbe::Stale),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(UnixProbe::Gone),
        Err(error) => Err(io_error_with_path_context(
            error,
            "probe existing control socket",
            sock,
        )),
    }
}

/// Probe from the existing `spawn_blocking` worker. `Handle::block_on` is
/// legal there, and the runtime thread awaiting that worker drives the I/O
/// driver even when it uses a current-thread runtime.
#[cfg(unix)]
fn probe_unix_listener(sock: &Path) -> io::Result<()> {
    let handle = tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    match handle.block_on(tokio::time::timeout(
        BIND_PROBE_WAIT,
        tokio::net::UnixStream::connect(sock),
    )) {
        Ok(Ok(_stream)) => Ok(()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "existing control socket did not answer the probe within {}ms; refusing to replace it",
                BIND_PROBE_WAIT.as_millis()
            ),
        )),
    }
}

/// Decide what to do with an existing control socket: `Ok(true)` when a live daemon owns it (the caller must not
/// replace it), `Ok(false)` when the path is free to bind (it never existed, or a stale socket was removed).
/// Only a refused probe proves staleness; `remove` runs only then, and any removal failure other than NotFound is
/// a startup error carrying the path.
#[cfg(unix)]
fn settle_existing_socket(
    probe: io::Result<()>,
    sock: &Path,
    remove: impl FnOnce() -> io::Result<()>,
) -> io::Result<bool> {
    match classify_unix_probe(probe, sock)? {
        UnixProbe::Live => Ok(true),
        UnixProbe::Gone => Ok(false),
        UnixProbe::Stale => match remove() {
            Ok(()) => Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error_with_path_context(
                error,
                "remove stale control socket",
                sock,
            )),
        },
    }
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
        Self::bind_exclusive_with_hardening(ep, false).await
    }

    /// Bind the endpoint with fail-closed Unix filesystem hardening. Bearer
    /// tokens authenticate clients, so every Unix server endpoint must also
    /// be protected from replacement by an untrusted filesystem principal.
    /// The flag is retained for API compatibility; Windows pipe policy does
    /// not use it.
    pub async fn bind_exclusive_with_hardening(
        ep: &IpcEndpoint,
        _require_hardening: bool,
    ) -> io::Result<BindOutcome> {
        match ep {
            IpcEndpoint::Unix(sock) => {
                #[cfg(unix)]
                {
                    // An accepting socket or one with a full backlog is a
                    // live daemon; only a refused socket is unlinked. Any
                    // other probe result is a startup error. The whole
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
                        let _lock = bind_parent_and_lock(&sock)?;
                        if let Some(own_dir) = sock.parent()
                            && own_dir
                                .file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(is_control_key_dir_name)
                            && let Some(root) = own_dir.parent()
                            && root.file_name().is_some_and(|n| n == "thegn")
                        {
                            let removed = prune_idle_control_dirs(
                                root,
                                own_dir,
                                CONTROL_DIR_PRUNE_MIN_IDLE,
                                CONTROL_DIR_PRUNE_MAX_EXAMINED,
                                &|sock| probe_unix_listener(sock),
                            );
                            if removed > 0 {
                                tracing::debug!(target: "thegn::daemon", removed, root = %root.display(), "pruned abandoned control directories");
                            }
                        }
                        if sock.exists() {
                            use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
                            let metadata = std::fs::symlink_metadata(&sock)?;
                            if !metadata.file_type().is_socket()
                                || metadata.uid() != effective_uid().unwrap_or_default()
                            {
                                return Err(io_error_with_path_context(
                                    io::Error::new(
                                        io::ErrorKind::PermissionDenied,
                                        "existing control endpoint is not an owner-owned socket",
                                    ),
                                    "verify existing control socket",
                                    &sock,
                                ));
                            }
                            if settle_existing_socket(probe_unix_listener(&sock), &sock, || {
                                std::fs::remove_file(&sock)
                            })? {
                                return Ok(UnixBind::AlreadyRunning);
                            }
                        }
                        match std::os::unix::net::UnixListener::bind(&sock) {
                            Ok(l) => {
                                // Filesystem isolation is defense in depth; runtime
                                // peer credentials are the authority. When this
                                // listener may grant implicit admin, however, a
                                // hardening failure is a startup error rather than a
                                // silently weakened deployment.
                                if let Err(e) = thegn_core::fsperm::restrict_to_owner(&sock)
                                    .and_then(|()| verify_unix_owner_mode(&sock, 0o600, false))
                                {
                                    if let Err(cleanup) = std::fs::remove_file(&sock) {
                                        tracing::error!(target: "thegn::daemon", path = %sock.display(), "failed to remove unhardened control socket: {cleanup}");
                                    }
                                    return Err(e);
                                }
                                l.set_nonblocking(true)?;
                                Ok(UnixBind::Bound(l))
                            }
                            Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                                let probe = match classify_unix_probe(
                                    probe_unix_listener(&sock),
                                    &sock,
                                ) {
                                    Ok(UnixProbe::Live) => Ok(()),
                                    Ok(UnixProbe::Stale | UnixProbe::Gone) => Err(io::Error::new(
                                        io::ErrorKind::ConnectionRefused,
                                        "no live listener",
                                    )),
                                    Err(error) => Err(error),
                                };
                                match require_live_endpoint(
                                    e,
                                    probe,
                                    &sock,
                                ) {
                                    Ok(()) => Ok(UnixBind::AlreadyRunning),
                                    Err(error) => Err(error),
                                }
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
                    use tokio::net::windows::named_pipe::ServerOptions;
                    // ERROR_ACCESS_DENIED (5): an instance of the name already
                    // exists. Usually that means a live daemon owns it — but
                    // not always, so retry briefly before concluding it.
                    //
                    // Windows keeps the name reserved for a moment after the
                    // last handle closes, if any of its instances ever carried
                    // a connection. Measured on the runner: a bind right after
                    // the previous owner drops its handles needs ONE ~5ms retry
                    // when there was a client, and zero when there wasn't. A
                    // daemon restarting into that window (`thegn daemon`
                    // straight after the old one exits) would otherwise decide
                    // a rival owned the pipe and exit, leaving no daemon at all.
                    //
                    // Deliberately NOT probed by connecting, unlike the unix
                    // arm. A unix probe connect is free — the listener's
                    // backlog absorbs it. A pipe probe is destructive: it
                    // consumes the one free instance the listener pre-created,
                    // and Windows then wants an explicit DisconnectNamedPipe
                    // before that instance can serve anyone else, so the next
                    // `accept_stream` hands out a corpse whose first read fails
                    // with "early eof". Also measured, the hard way.
                    const ERROR_ACCESS_DENIED: i32 = 5;
                    // ~122ms total. Only ever paid on daemon startup, and only
                    // when the name is taken — a losing racer is exiting anyway.
                    const REBIND_BACKOFF_MS: [u64; 5] = [2, 5, 15, 40, 60];
                    let mut backoff = REBIND_BACKOFF_MS.iter();
                    loop {
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
                                match backoff.next() {
                                    Some(ms) => {
                                        tokio::time::sleep(std::time::Duration::from_millis(*ms))
                                            .await;
                                    }
                                    // Still held after the whole budget: a real
                                    // daemon. Refusing to start is recoverable;
                                    // stealing a live daemon's pipe is not.
                                    None => return Ok(BindOutcome::AlreadyRunning),
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                #[cfg(not(windows))]
                {
                    let _ = name; // best-effort: non-Result discard — param unused in this stub
                    Err(unsupported("named-pipe IPC on a non-Windows host"))
                }
            }
        }
    }

    /// Accept one stream together with listener-authenticated peer metadata.
    pub async fn accept_stream_with_info(&mut self) -> io::Result<(IpcStream, IpcConnectInfo)> {
        match self {
            #[cfg(unix)]
            IpcListener::Unix(l) => {
                let (stream, _) = l.accept().await?;
                let peer = match stream.peer_cred() {
                    Ok(cred) => PeerIdentity::UnixEuid(cred.uid()),
                    Err(error) => PeerIdentity::Unavailable(error.to_string()),
                };
                let endpoint = l
                    .local_addr()
                    .ok()
                    .and_then(|addr| {
                        addr.as_pathname()
                            .map(|path| path.to_string_lossy().into_owned())
                    })
                    .unwrap_or_default();
                Ok((IpcStream::Unix(stream), IpcConnectInfo { endpoint, peer }))
            }
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
                Ok((
                    IpcStream::PipeServer(server),
                    IpcConnectInfo {
                        endpoint: name.clone(),
                        peer: PeerIdentity::LocalPipe,
                    },
                ))
            }
        }
    }

    /// Accept one connection. (Named `accept_stream` so the inherent method
    /// doesn't shadow `axum::serve::Listener::accept`.)
    pub async fn accept_stream(&mut self) -> io::Result<IpcStream> {
        self.accept_stream_with_info()
            .await
            .map(|(stream, _)| stream)
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
    type Addr = IpcConnectInfo;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.accept_stream_with_info().await {
                Ok(accepted) => return accepted,
                Err(e) => {
                    tracing::warn!(target: "thegn::daemon", "ipc accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(IpcConnectInfo {
            endpoint: self.endpoint_display(),
            peer: PeerIdentity::Unavailable("listener address has no peer".into()),
        })
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, IpcListener>>
    for IpcConnectInfo
{
    fn connect_info(stream: axum::serve::IncomingStream<'_, IpcListener>) -> Self {
        stream.remote_addr().clone()
    }
}

/// Create and owner-restrict a Unix socket's parent directory. Unix endpoint
/// parents are hardened for both local-admin and token-only listeners.
pub fn prepare_control_directory(path: &Path, _require_hardening: bool) -> io::Result<()> {
    // Windows control endpoints are named pipes, so their security comes from
    // the pipe DACL and there is no filesystem parent to harden.
    if cfg!(windows) {
        return Ok(());
    }
    std::fs::create_dir_all(path).map_err(|error| {
        io_error_with_path_context(
            error,
            "prepare control socket directory; move [daemon] socket into a private directory",
            path,
        )
    })?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "control socket parent {} must not be a symlink; move [daemon] socket into a private directory",
                path.display()
            ),
        ));
    }
    let result = thegn_core::fsperm::restrict_dir_to_owner(path);
    #[cfg(unix)]
    let result = result.and_then(|()| verify_unix_owner_mode(path, 0o700, true));
    result.map_err(|error| {
        io_error_with_path_context(
            error,
            "harden control socket directory; move [daemon] socket into a private directory",
            path,
        )
    })
}

/// Read-only hardening status used by `thegn doctor`.
pub fn control_endpoint_hardening(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket has no parent directory",
        )
    })?;
    #[cfg(unix)]
    {
        verify_unix_owner_mode(parent, 0o700, true)?;
        verify_unix_owner_mode(path, 0o600, false)
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

#[cfg(unix)]
fn verify_unix_bind_parent(path: &Path, metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let mode = metadata.mode() & 0o777;
    let expected_uid = effective_uid().expect("unix has an effective uid");
    if !metadata.is_dir() || metadata.uid() != expected_uid || mode & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be an owner-owned directory without group/other write permission (found uid {}, mode {mode:04o})",
                path.display(),
                metadata.uid(),
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_unix_owner_mode(path: &Path, expected_mode: u32, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};
    let metadata = std::fs::symlink_metadata(path)?;
    let correct_kind = if directory {
        metadata.is_dir()
    } else {
        metadata.file_type().is_socket()
    };
    let mode = metadata.permissions().mode() & 0o777;
    let uid = metadata.uid();
    let expected_uid = effective_uid().expect("unix has an effective uid");
    if !correct_kind || uid != expected_uid || mode != expected_mode {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} must be an owner-owned {} with mode {:04o} (found uid {uid}, mode {mode:04o}); set owner uid {expected_uid} and mode {:04o}",
                path.display(),
                if directory { "directory" } else { "socket" },
                expected_mode,
                expected_mode,
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_probe_classifies_every_probe_result() {
        let sock = Path::new("control.sock");
        assert_eq!(classify_unix_probe(Ok(()), sock).unwrap(), UnixProbe::Live);

        let cases = [
            (io::ErrorKind::WouldBlock, Some(UnixProbe::Live)),
            (io::ErrorKind::ConnectionRefused, Some(UnixProbe::Stale)),
            (io::ErrorKind::NotFound, Some(UnixProbe::Gone)),
            (io::ErrorKind::TimedOut, None),
            (io::ErrorKind::PermissionDenied, None),
            (io::ErrorKind::Other, None),
        ];
        for (kind, expected) in cases {
            let result = classify_unix_probe(Err(io::Error::new(kind, "probe failure")), sock);
            match expected {
                Some(expected) => assert_eq!(result.unwrap(), expected, "{kind:?}"),
                None => {
                    let error = result.expect_err("unclassified probe errors must fail closed");
                    assert!(error.to_string().contains("probe existing control socket"));
                    assert!(error.to_string().contains(&sock.display().to_string()));
                    let context = error
                        .get_ref()
                        .and_then(|source| source.downcast_ref::<IoPathContext>())
                        .expect("probe error keeps its path context");
                    assert_eq!(context.source.kind(), kind);
                }
            }
        }
    }

    /// The deterministic decision is covered by `settle_existing_socket_*`; this is the real-kernel confirmation.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bind_probe_treats_a_full_backlog_as_a_live_daemon() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::io::AsRawFd as _;

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("busy.sock");
        assert_socket_path_fits(&sock);
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        // SAFETY: listener owns a valid listening socket fd; listen updates its backlog.
        let result = unsafe { libc::listen(listener.as_raw_fd(), 0) };
        assert_eq!(result, 0, "listen must shrink the backlog");

        let mut connections = Vec::new();
        let mut backlog_full = false;
        for _ in 0..64 {
            match tokio::net::UnixStream::connect(&sock).await {
                Ok(stream) => connections.push(stream),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    backlog_full = true;
                    break;
                }
                Err(error) => panic!("unexpected backlog fill error: {error}"),
            }
        }
        if !backlog_full {
            assert!(!thegn_core::testenv::require_or_skip(
                "a unix listener whose accept backlog fills (EAGAIN)",
                false,
            ));
            return;
        }

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            IpcListener::bind_exclusive(&IpcEndpoint::for_socket_path(&sock)),
        )
        .await
        .expect("probe must not block on a full backlog")
        .unwrap();
        assert!(matches!(outcome, BindOutcome::AlreadyRunning));
        assert!(sock.exists(), "live listener socket must survive the probe");
        drop(connections);
        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_probe_still_replaces_a_refused_stale_socket() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("stale.sock");
        assert_socket_path_fits(&sock);
        let endpoint = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive(&endpoint).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        drop(listener);

        assert!(matches!(
            IpcListener::bind_exclusive(&endpoint).await.unwrap(),
            BindOutcome::Bound(_)
        ));
    }

    #[cfg(unix)]
    fn open_lock_holder(path: &Path) -> std::fs::File {
        use std::os::unix::fs::OpenOptionsExt as _;

        std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .unwrap()
    }

    #[cfg(unix)]
    fn private_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[cfg(unix)]
    fn assert_socket_path_fits(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;

        // Keep a NUL terminator available in sockaddr_un::sun_path on every
        // Unix target supported by libc.
        let sockaddr = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
        assert!(
            path.as_os_str().as_bytes().len() < sockaddr.sun_path.len(),
            "socket path exceeds sockaddr_un::sun_path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn server_peer_identity_requires_same_uid() {
        let uid = effective_uid().unwrap();
        assert!(server_peer_allowed(Some(uid), uid).is_ok());
        assert_eq!(
            server_peer_allowed(Some(uid.wrapping_add(1)), uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            server_peer_allowed(None, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connect_rejects_symlinked_and_shared_parents_before_connecting() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let td = tempfile::tempdir().unwrap();
        let real = td.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = td.path().join("alias");
        symlink(&real, &alias).unwrap();
        let symlink_error = match connect(&IpcEndpoint::Unix(alias.join("missing.sock"))).await {
            Err(error) => error,
            Ok(_) => panic!("symlinked socket parent connected"),
        };
        assert_eq!(symlink_error.kind(), io::ErrorKind::PermissionDenied);

        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o777)).unwrap();
        let broad_error = match connect(&IpcEndpoint::Unix(real.join("missing.sock"))).await {
            Err(error) => error,
            Ok(_) => panic!("world-writable socket parent connected"),
        };
        assert_eq!(broad_error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connect_rejects_a_server_before_the_server_can_read_anything() {
        use tokio::io::AsyncReadExt as _;

        let dir = private_tempdir();
        let sock = dir.path().join("fake.sock");
        assert_socket_path_fits(&sock);
        prepare_control_directory(dir.path(), false).unwrap();
        let ep = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };

        verify_unix_client_parent(&sock).unwrap();
        drop(listener);
        let mut replacement = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("dropped listener must be stale"),
        };
        let expected_euid = effective_uid().unwrap().wrapping_add(1);
        let error = match connect_unix_after_parent_check(&sock, expected_euid).await {
            Err(error) => error,
            Ok(_) => panic!("server with a different uid was accepted"),
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let (mut server_side, _) = replacement.accept_stream_with_info().await.unwrap();
        let mut received = Vec::new();
        server_side.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty(), "rejected server received client bytes");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_connect_handles_leaf_replacement_before_connect() {
        let dir = private_tempdir();
        let sock = dir.path().join("replace.sock");
        assert_socket_path_fits(&sock);
        prepare_control_directory(dir.path(), false).unwrap();
        let ep = IpcEndpoint::for_socket_path(&sock);

        let first = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        drop(first);
        let mut replacement = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("dropped listener must be stale"),
        };
        let client = connect(&ep)
            .await
            .expect("connect must authenticate the listener bound at the replaced leaf");
        drop(client);
        let (accepted, _) = replacement.accept_stream_with_info().await.unwrap();
        drop(accepted);

        std::fs::remove_file(&sock).unwrap();
        std::fs::write(&sock, b"replacement leaf").unwrap();
        assert!(connect(&ep).await.is_err());
    }

    #[test]
    fn pipe_name_is_deterministic_prefixed_and_short() {
        let a = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let b = pipe_name_for_path(Path::new("/home/u/.local/state/thegn/daemon.sock"));
        let c = pipe_name_for_path(Path::new("/tmp/other/thegn/daemon.sock"));
        assert_eq!(a, b, "same path ⇒ same pipe name");
        assert_ne!(a, c, "different state dirs ⇒ different pipes (isolation)");
        assert!(a.starts_with(r"\\.\pipe\thegn-v2-"), "{a}");
        // prefix + "thegn-v2-" + 32 hex chars — comfortably inside the 256-char
        // pipe-name limit regardless of the input path length.
        assert_eq!(a.len(), PIPE_PREFIX.len() + "thegn-v2-".len() + 32);
    }

    #[test]
    fn pipe_name_normalizes_case_separators_and_verbatim_prefix() {
        let paths = [
            r"C:\Users\U\AppData\Local\thegn\daemon.sock",
            r"\\?\C:\Users\U\AppData\Local\thegn\daemon.sock",
            "C:/Users/U/AppData/Local/thegn/daemon.sock",
            r"c:\users\u\appdata\local\thegn\daemon.sock",
        ];
        let names: Vec<_> = paths
            .iter()
            .map(|path| pipe_name_for_path(Path::new(path)))
            .collect();
        let expected = r"\\.\pipe\thegn-v2-416b942e44eb25df62f1699da9543710";
        for name in &names {
            assert_eq!(name, expected, "normalized pipe name differs: {name}");
        }
    }

    #[test]
    fn pipe_name_unc_verbatim_prefix_matches_plain_unc() {
        let verbatim = pipe_name_for_path(Path::new(r"\\?\UNC\server\share\thegn\daemon.sock"));
        let plain = pipe_name_for_path(Path::new(r"\\server\share\thegn\daemon.sock"));
        let other = pipe_name_for_path(Path::new(r"\\server\share\other\daemon.sock"));
        assert_eq!(verbatim, plain);
        assert_ne!(plain, other);
    }

    #[test]
    fn pipe_name_tolerates_multibyte_after_verbatim_prefix() {
        for path in [r"\\?\a界\x", r"\\?\abé\x"] {
            let name = pipe_name_for_path(Path::new(path));
            assert!(name.starts_with(r"\\.\pipe\thegn-v2-"), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn pipe_name_keeps_non_utf8_paths_distinct() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;

        let first = Path::new(OsStr::from_bytes(b"/s/\xff"));
        let second = Path::new(OsStr::from_bytes(b"/s/\xfe"));
        assert_ne!(pipe_name_for_path(first), pipe_name_for_path(second));
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
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let ep = IpcEndpoint::for_socket_path(&sock);

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
        let (probe, probe_info) = listener.accept_stream_with_info().await.unwrap();
        assert_eq!(
            probe_info.peer,
            PeerIdentity::UnixEuid(effective_uid().unwrap())
        );
        drop(probe);

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
        let (mut server_side, peer_info) = listener.accept_stream_with_info().await.unwrap();
        assert_eq!(
            peer_info.peer,
            PeerIdentity::UnixEuid(effective_uid().unwrap())
        );
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
    }

    /// The deterministic decision is covered by `settle_existing_socket_*`; this is the real-kernel confirmation.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(
        clippy::permissions_set_readonly_false,
        reason = "restores a private temp dir after exercising stale-socket removal"
    )]
    async fn failed_stale_socket_removal_is_a_contextual_startup_error() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let ep = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive(&ep).await.unwrap() {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        drop(listener); // leave a genuine stale socket at the endpoint

        let mut permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(dir.path(), permissions).unwrap();
        // Root and some unusual filesystems can still unlink from a read-only
        // directory. Report that missing non-root prerequisite explicitly.
        match std::fs::remove_file(&sock) {
            Ok(()) => {
                let mut permissions = std::fs::metadata(dir.path()).unwrap().permissions();
                permissions.set_readonly(false);
                std::fs::set_permissions(dir.path(), permissions).unwrap();
                assert!(!thegn_core::testenv::require_or_skip(
                    "a read-only directory that refuses unlink (non-root)",
                    false,
                ));
                return;
            }
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
        }

        let error = match IpcListener::bind_exclusive(&ep).await {
            Ok(BindOutcome::AlreadyRunning) => panic!("an unremovable path is not a live daemon"),
            Ok(BindOutcome::Bound(_)) => {
                panic!("a stale socket in a read-only directory must not bind")
            }
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("remove stale control socket"));
        assert!(error.to_string().contains(&sock.display().to_string()));
        let source = error
            .get_ref()
            .and_then(|context| context.downcast_ref::<IoPathContext>())
            .expect("context wrapper preserves the filesystem error as its source");
        assert_eq!(source.source.kind(), io::ErrorKind::PermissionDenied);

        let mut permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        permissions.set_readonly(false);
        std::fs::set_permissions(dir.path(), permissions).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn settle_existing_socket_full_backlog_is_live_and_never_removes() {
        use std::cell::Cell;

        let sock = Path::new("/run/thegn-test/control.sock");
        let calls = Cell::new(0_u32);
        for probe in [Err(io::Error::from(io::ErrorKind::WouldBlock)), Ok(())] {
            let result = settle_existing_socket(probe, sock, || {
                calls.set(calls.get() + 1);
                Ok(())
            });
            assert!(matches!(result, Ok(true)));
        }
        assert_eq!(calls.get(), 0, "a live endpoint must never be removed");
    }

    #[cfg(unix)]
    #[test]
    fn settle_existing_socket_stale_removal_denied_is_a_contextual_startup_error() {
        use std::cell::Cell;

        let sock = Path::new("/run/thegn-test/control.sock");
        let calls = Cell::new(0_u32);
        let result = settle_existing_socket(
            Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            sock,
            || {
                calls.set(calls.get() + 1);
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a denied stale-socket removal must fail startup"),
        };
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("remove stale control socket"));
        assert!(error.to_string().contains(&sock.display().to_string()));
        let context = error
            .get_ref()
            .and_then(|source| source.downcast_ref::<IoPathContext>())
            .expect("removal error keeps its path context");
        assert_eq!(context.source.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(calls.get(), 1, "stale sockets invoke the remover once");
    }

    #[cfg(unix)]
    #[test]
    fn settle_existing_socket_stale_or_gone_path_is_free_to_bind() {
        use std::cell::Cell;

        let sock = Path::new("/run/thegn-test/control.sock");
        let removed_calls = Cell::new(0_u32);
        let removed = settle_existing_socket(
            Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            sock,
            || {
                removed_calls.set(removed_calls.get() + 1);
                Ok(())
            },
        );
        assert!(matches!(removed, Ok(false)));
        assert_eq!(removed_calls.get(), 1);

        let not_found_calls = Cell::new(0_u32);
        let not_found = settle_existing_socket(
            Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            sock,
            || {
                not_found_calls.set(not_found_calls.get() + 1);
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
        );
        assert!(matches!(not_found, Ok(false)));
        assert_eq!(not_found_calls.get(), 1);

        let gone_calls = Cell::new(0_u32);
        let gone =
            settle_existing_socket(Err(io::Error::from(io::ErrorKind::NotFound)), sock, || {
                gone_calls.set(gone_calls.get() + 1);
                Ok(())
            });
        assert!(matches!(gone, Ok(false)));
        assert_eq!(gone_calls.get(), 0, "a gone path must not be removed");
    }

    #[cfg(unix)]
    #[test]
    fn settle_existing_socket_unclassified_probe_error_never_removes() {
        use std::cell::Cell;

        let sock = Path::new("/run/thegn-test/control.sock");
        for kind in [
            io::ErrorKind::TimedOut,
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::Other,
        ] {
            let calls = Cell::new(0_u32);
            let result =
                settle_existing_socket(Err(io::Error::new(kind, "probe failure")), sock, || {
                    calls.set(calls.get() + 1);
                    Ok(())
                });
            let error = match result {
                Err(error) => error,
                Ok(_) => panic!("unclassified probe failures must fail closed"),
            };
            assert!(error.to_string().contains("probe existing control socket"));
            assert!(error.to_string().contains(&sock.display().to_string()));
            assert_eq!(calls.get(), 0, "{kind:?} probe errors must not unlink");
        }
    }

    #[test]
    fn addr_in_use_without_a_connectable_listener_preserves_the_bind_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");

        assert!(
            require_live_endpoint(
                io::Error::new(io::ErrorKind::AddrInUse, "original bind failure"),
                Ok(()),
                &sock,
            )
            .is_ok()
        );

        let error = require_live_endpoint(
            io::Error::new(io::ErrorKind::AddrInUse, "original bind failure"),
            Err(io::Error::new(io::ErrorKind::NotFound, "no listener")),
            &sock,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(error.to_string().contains("bind control socket"));
        assert!(error.to_string().contains(&sock.display().to_string()));
        let source = error
            .get_ref()
            .and_then(|context| context.downcast_ref::<IoPathContext>())
            .expect("context wrapper preserves the bind error as its source");
        assert_eq!(source.source.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(source.source.to_string(), "original bind failure");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn strict_hardening_is_verified_and_doctor_detects_regression() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), true).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let ep = IpcEndpoint::for_socket_path(&sock);
        let listener = match IpcListener::bind_exclusive_with_hardening(&ep, true)
            .await
            .unwrap()
        {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh socket must bind"),
        };
        control_endpoint_hardening(&sock).expect("strict bind leaves a hardened endpoint");

        let link = dir.path().join("link.sock");
        std::os::unix::fs::symlink(&sock, &link).unwrap();
        let symlink_error = control_endpoint_hardening(&link).unwrap_err();
        assert_eq!(symlink_error.kind(), io::ErrorKind::PermissionDenied);

        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).unwrap();
        let error = control_endpoint_hardening(&sock).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("mode 0666"));

        // Token-only mode cannot use a shared parent or a replaceable lock
        // path as a source of server identity either.
        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        let shared_error = match IpcListener::bind_exclusive_with_hardening(
            &IpcEndpoint::for_socket_path(&shared.join("d.sock")),
            false,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("token-only bind must reject a shared parent"),
        };
        assert_eq!(shared_error.kind(), io::ErrorKind::PermissionDenied);

        let not_a_directory = dir.path().join("not-a-directory");
        std::fs::write(&not_a_directory, b"file").unwrap();
        let directory_error = prepare_control_directory(&not_a_directory, false).unwrap_err();
        assert!(
            directory_error
                .to_string()
                .contains("move [daemon] socket into a private directory")
        );

        let lock_sock = dir.path().join("replace.sock");
        let lock_link = dir.path().join("replace.sock.lock");
        let lock_target = dir.path().join("lock-target");
        std::fs::write(&lock_target, b"unrelated").unwrap();
        std::os::unix::fs::symlink(&lock_target, &lock_link).unwrap();
        let lock_ep = IpcEndpoint::for_socket_path(&lock_sock);
        let lock_error = match IpcListener::bind_exclusive_with_hardening(&lock_ep, false).await {
            Err(error) => error,
            Ok(_) => panic!("token-only bind must reject a replaceable lock path"),
        };
        let lock_context = lock_error
            .get_ref()
            .and_then(|error| error.downcast_ref::<IoPathContext>())
            .expect("lock open error retains path context");
        assert!(lock_error.to_string().contains("open control socket lock"));
        assert_eq!(lock_context.source.raw_os_error(), Some(libc::ELOOP));
        assert!(
            !lock_sock.exists(),
            "failed lock acquisition must not create the endpoint"
        );

        let regular_sock = dir.path().join("regular.sock");
        std::fs::write(&regular_sock, b"leave this file alone").unwrap();
        let regular_ep = IpcEndpoint::for_socket_path(&regular_sock);
        let regular_error = match IpcListener::bind_exclusive(&regular_ep).await {
            Err(error) => error,
            Ok(_) => panic!("bind must reject a regular file at the socket path"),
        };
        assert_eq!(regular_error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read(&regular_sock).unwrap(),
            b"leave this file alone"
        );

        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_restricts_an_existing_broad_control_lock() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let lock = dir.path().join("d.sock.lock");
        std::fs::write(&lock, b"").unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();

        let listener = match IpcListener::bind_exclusive(&IpcEndpoint::for_socket_path(&sock))
            .await
            .unwrap()
        {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        assert_eq!(
            std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o600,
            "bind must tighten the preexisting lock before using it"
        );

        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_fails_closed_while_another_holder_keeps_it() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("held.sock");
        assert_socket_path_fits(&sock);
        let holder = open_lock_holder(&sock.with_extension("sock.lock"));
        holder.lock().unwrap();

        let started = std::time::Instant::now();
        let error = bind_lock_with_wait(&sock, std::time::Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("held by another process"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(!sock.exists(), "failed lock acquisition created a socket");
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_acquires_once_the_holder_releases() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("held.sock");
        assert_socket_path_fits(&sock);
        let holder = open_lock_holder(&sock.with_extension("sock.lock"));
        holder.lock().unwrap();
        drop(holder);

        assert!(bind_lock_with_wait(&sock, std::time::Duration::from_secs(2)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_recheck_reopens_when_the_lock_is_unlinked_after_open() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("r.sock");
        assert_socket_path_fits(&sock);
        let lock = dir.path().join("r.sock.lock");
        let mut calls = 0_u32;
        let mut old_ino = 0_u64;
        let file = bind_lock_checked(&sock, std::time::Duration::from_secs(5), &mut || {
            calls += 1;
            if calls == 1 {
                old_ino = std::fs::symlink_metadata(&lock).unwrap().ino();
                std::fs::remove_file(&lock).unwrap();
            }
        })
        .unwrap();
        let current = std::fs::symlink_metadata(&lock).unwrap().ino();
        assert_eq!(file.metadata().unwrap().ino(), current);
        assert_ne!(current, old_ino);
        assert_eq!(calls, 2);
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_recheck_reopens_when_the_lock_is_replaced_after_open() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("r.sock");
        assert_socket_path_fits(&sock);
        let lock = dir.path().join("r.sock.lock");
        let other = dir.path().join("r.other");
        let mut calls = 0_u32;
        let mut old_ino = 0_u64;
        let file = bind_lock_checked(&sock, std::time::Duration::from_secs(5), &mut || {
            calls += 1;
            if calls == 1 {
                old_ino = std::fs::symlink_metadata(&lock).unwrap().ino();
                drop(open_lock_holder(&other));
                std::fs::rename(&other, &lock).unwrap();
            }
        })
        .unwrap();
        let current = std::fs::symlink_metadata(&lock).unwrap().ino();
        assert_eq!(file.metadata().unwrap().ino(), current);
        assert_ne!(current, old_ino);
        assert_eq!(calls, 2);
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_recheck_returns_the_same_inode_when_nothing_changes() {
        use std::os::unix::fs::MetadataExt as _;

        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("r.sock");
        assert_socket_path_fits(&sock);
        let lock = dir.path().join("r.sock.lock");
        let mut calls = 0_u32;
        let file = bind_lock_checked(&sock, std::time::Duration::from_secs(5), &mut || {
            calls += 1;
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            file.metadata().unwrap().ino(),
            std::fs::symlink_metadata(&lock).unwrap().ino()
        );
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_recheck_gives_up_when_the_lock_keeps_being_replaced() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("r.sock");
        assert_socket_path_fits(&sock);
        let lock = dir.path().join("r.sock.lock");
        let error = bind_lock_checked(&sock, std::time::Duration::from_secs(5), &mut || {
            if lock.exists() {
                std::fs::remove_file(&lock).unwrap();
            }
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("kept being replaced"));
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_recheck_two_binders_serialize_on_one_inode() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("r.sock");
        assert_socket_path_fits(&sock);
        let first = bind_lock_with_wait(&sock, std::time::Duration::from_secs(2)).unwrap();
        let error = bind_lock_with_wait(&sock, std::time::Duration::from_millis(200)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(first);
        assert!(bind_lock_with_wait(&sock, std::time::Duration::from_secs(2)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn bind_lock_waits_out_a_brief_holder() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("held.sock");
        assert_socket_path_fits(&sock);
        let lock_path = sock.with_extension("sock.lock");
        let (locked_sender, locked_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let holder_thread = std::thread::spawn(move || {
            let holder = open_lock_holder(&lock_path);
            holder.lock().unwrap();
            locked_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            drop(holder);
        });
        locked_receiver.recv().unwrap();

        let (binding_sender, binding_receiver) = std::sync::mpsc::channel();
        let binding_sock = sock.clone();
        let binder_thread = std::thread::spawn(move || {
            binding_sender.send(()).unwrap();
            bind_lock_with_wait(&binding_sock, std::time::Duration::from_secs(2))
        });
        binding_receiver.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let binder_waited_for_holder = !binder_thread.is_finished();
        release_sender.send(()).unwrap();
        assert!(binder_thread.join().unwrap().is_ok());
        holder_thread.join().unwrap();
        assert!(
            binder_waited_for_holder,
            "binder finished while the lock was held"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bind_retry_recreates_a_dir_removed_before_the_parent_check() {
        let root = private_tempdir();
        let dir = root.path().join("k");
        prepare_control_directory(&dir, false).unwrap();
        let sock = dir.join("d.sock");
        assert_socket_path_fits(&sock);
        std::fs::remove_dir(&dir).unwrap();
        let mut ensured = 0_u32;
        let result = bind_parent_and_lock_with(
            &sock,
            &mut |p| {
                ensured += 1;
                prepare_control_directory(p, false)
            },
            &mut || {},
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(dir.join("d.sock.lock").exists());
        assert_eq!(ensured, 1);
    }

    #[cfg(unix)]
    #[test]
    fn bind_retry_recovers_when_the_dir_is_pruned_between_check_and_lock() {
        let root = private_tempdir();
        let dir = root.path().join("k");
        prepare_control_directory(&dir, false).unwrap();
        let sock = dir.join("d.sock");
        assert_socket_path_fits(&sock);
        let mut ensured = 0_u32;
        let mut checked = 0_u32;
        let result = bind_parent_and_lock_with(
            &sock,
            &mut |p| {
                ensured += 1;
                prepare_control_directory(p, false)
            },
            &mut || {
                checked += 1;
                if checked == 1 {
                    std::fs::remove_dir(&dir).unwrap();
                }
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(ensured, 1);
        assert_eq!(checked, 2);
    }

    #[cfg(unix)]
    #[test]
    fn bind_retry_gives_up_after_one_retry() {
        let root = private_tempdir();
        let dir = root.path().join("k");
        prepare_control_directory(&dir, false).unwrap();
        let sock = dir.join("d.sock");
        assert_socket_path_fits(&sock);
        std::fs::remove_dir(&dir).unwrap();
        let mut ensured = 0_u32;
        let result = bind_parent_and_lock_with(
            &sock,
            &mut |_| {
                ensured += 1;
                Ok(())
            },
            &mut || {},
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(ensured, 1);
    }

    #[cfg(unix)]
    #[test]
    fn bind_retry_does_not_retry_other_errors() {
        let root = private_tempdir();
        let dir = root.path().join("k");
        prepare_control_directory(&dir, false).unwrap();
        let sock = dir.join("d.sock");
        assert_socket_path_fits(&sock);
        std::fs::create_dir(dir.join("d.sock.lock")).unwrap();
        let mut ensured = 0_u32;
        let result = bind_parent_and_lock_with(
            &sock,
            &mut |p| {
                ensured += 1;
                prepare_control_directory(p, false)
            },
            &mut || {},
        );
        let error = result.unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(ensured, 0);
    }

    #[cfg(unix)]
    #[test]
    fn bind_retry_happy_path_is_unchanged() {
        let root = private_tempdir();
        let dir = root.path().join("k");
        prepare_control_directory(&dir, false).unwrap();
        let sock = dir.join("d.sock");
        assert_socket_path_fits(&sock);
        let mut ensured = 0_u32;
        let mut checked = 0_u32;
        let result = bind_parent_and_lock_with(
            &sock,
            &mut |p| {
                ensured += 1;
                prepare_control_directory(p, false)
            },
            &mut || checked += 1,
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(ensured, 0);
        assert_eq!(checked, 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bind_fails_closed_when_control_lock_cannot_be_opened() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let sock = dir.path().join("blocked.sock");
        assert_socket_path_fits(&sock);
        std::fs::create_dir(sock.with_extension("sock.lock")).unwrap();

        let error = match IpcListener::bind_exclusive_with_hardening(
            &IpcEndpoint::for_socket_path(&sock),
            false,
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("bind must fail when lock acquisition fails"),
        };
        assert!(error.to_string().contains("open control socket lock"));
        assert!(!sock.exists(), "failed lock acquisition created a socket");
    }

    /// The stale-socket TOCTOU: N binders racing one stale socket file must
    /// elect exactly one daemon, and the losers' probe/unlink path must not
    /// strip the winner's freshly-bound socket out of the filesystem. Without
    /// the `<sock>.lock` serialization this is a race (unlink succeeds on
    /// bound sockets); with it the outcome is deterministic.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn racing_binders_on_a_stale_socket_elect_exactly_one_daemon() {
        let dir = private_tempdir();
        prepare_control_directory(dir.path(), false).unwrap();
        let path = dir.path().join("d.sock");
        assert_socket_path_fits(&path);
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

    #[cfg(unix)]
    fn prune_fixture() -> (tempfile::TempDir, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let td = tempfile::tempdir_in("/tmp").unwrap();
        let root = td.path().join("thegn");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        (td, root)
    }

    #[cfg(unix)]
    fn prune_key_dir(root: &Path, key: &str, lock: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = root.join(key);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if lock {
            drop(open_lock_holder(&dir.join("daemon.sock.lock")));
        }
        dir
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_removes_an_idle_abandoned_dir_with_only_its_lock() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let dir = prune_key_dir(&root, &"b".repeat(32), true);
        let removed =
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(()));
        assert_eq!(removed, 1);
        // The lock goes first; the empty directory stays for a later pass.
        assert!(dir.exists());
        assert!(!dir.join("daemon.sock.lock").exists());
        assert!(own.join("daemon.sock.lock").exists());
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            1
        );
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_removes_an_empty_key_dir() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let dir = prune_key_dir(&root, &"b".repeat(32), false);
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            1
        );
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_keeps_a_dir_whose_lock_is_held() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let dir = prune_key_dir(&root, &"b".repeat(32), false);
        let lock = dir.join("daemon.sock.lock");
        let holder = open_lock_holder(&lock);
        holder.lock().unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            0
        );
        assert!(lock.exists());
        drop(holder);
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            1
        );
        assert!(!lock.exists());
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_keeps_a_dir_with_any_other_entry() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let with_sock = prune_key_dir(&root, &"b".repeat(32), true);
        std::fs::write(with_sock.join("daemon.sock"), b"").unwrap();
        let with_locator = prune_key_dir(&root, &"c".repeat(32), true);
        std::fs::write(with_locator.join("tg-locator"), b"").unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            0
        );
        assert!(with_sock.join("daemon.sock.lock").exists());
        assert!(with_locator.join("daemon.sock.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_ignores_non_key_names_and_symlinks() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let not_key = prune_key_dir(&root, "not-a-key", true);
        let upper = prune_key_dir(&root, &format!("{}A", "d".repeat(31)), true);
        let target = prune_key_dir(&root, &"e".repeat(32), true);
        // The target is owned by `own`'s sibling set; only the symlink name is
        // under test, so make the target ineligible by holding its lock.
        let holder = open_lock_holder(&target.join("daemon.sock.lock"));
        holder.lock().unwrap();
        let link = root.join("f".repeat(32));
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            0
        );
        assert!(not_key.exists());
        assert!(upper.exists());
        assert!(target.join("daemon.sock.lock").exists());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        drop(holder);
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_requires_owner_only_mode_and_a_single_link_lock() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let broad = prune_key_dir(&root, &"b".repeat(32), true);
        std::fs::set_permissions(&broad, std::fs::Permissions::from_mode(0o755)).unwrap();
        let linked = prune_key_dir(&root, &"c".repeat(32), true);
        std::fs::hard_link(linked.join("daemon.sock.lock"), root.join("outside-link")).unwrap();
        let symlinked = prune_key_dir(&root, &"d".repeat(32), false);
        let victim = root.join("victim-file");
        std::fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, symlinked.join("daemon.sock.lock")).unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            0
        );
        assert!(broad.join("daemon.sock.lock").exists());
        assert!(linked.join("daemon.sock.lock").exists());
        assert!(
            std::fs::symlink_metadata(symlinked.join("daemon.sock.lock"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_respects_the_idle_floor_and_the_examined_cap() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let fresh = prune_key_dir(&root, &"b".repeat(32), true);
        assert_eq!(
            prune_idle_control_dirs(
                &root,
                &own,
                std::time::Duration::from_secs(3600),
                64,
                &|_| Ok(())
            ),
            0
        );
        assert!(fresh.exists());
        std::fs::remove_dir_all(&fresh).unwrap();

        let dirs = [
            prune_key_dir(&root, &"c".repeat(32), true),
            prune_key_dir(&root, &"d".repeat(32), true),
            prune_key_dir(&root, &"e".repeat(32), true),
        ];
        let count = prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 2, &|_| Ok(()));
        assert!(count <= 2);
        let remaining = dirs
            .iter()
            .filter(|dir| dir.join("daemon.sock.lock").exists())
            .count();
        assert_eq!(remaining, 3 - count);
        assert!(remaining >= 1);
        // Locks first, then the emptied directories.
        prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, usize::MAX, &|_| {
            Ok(())
        });
        prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, usize::MAX, &|_| {
            Ok(())
        });
        assert!(dirs.iter().all(|dir| !dir.exists()));
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_refuses_an_unverified_root() {
        use std::os::unix::fs::PermissionsExt as _;

        // (a) a group/world-writable root prunes nothing.
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let eligible = prune_key_dir(&root, &"b".repeat(32), true);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            0
        );
        assert!(eligible.join("daemon.sock.lock").exists());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();

        // (b) a symlinked root prunes nothing, and its target is untouched.
        let link = root.parent().unwrap().join("thegn-link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert_eq!(
            prune_idle_control_dirs(&link, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            0
        );
        assert!(eligible.join("daemon.sock.lock").exists());
        // Control: the verified root does prune.
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Ok(())),
            1
        );
    }

    #[cfg(unix)]
    fn std_probe(path: &Path) -> io::Result<()> {
        std::os::unix::net::UnixStream::connect(path).map(|_| ())
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_removes_a_stale_socket_then_its_lock() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let dir = prune_key_dir(&root, &"b".repeat(32), true);
        drop(std::os::unix::net::UnixListener::bind(dir.join("daemon.sock")).unwrap());
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &std_probe),
            2
        );
        assert!(!dir.join("daemon.sock").exists());
        assert!(!dir.join("daemon.sock.lock").exists());
        assert!(dir.exists());
        assert!(own.join("daemon.sock.lock").exists());
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &std_probe),
            1
        );
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_keeps_a_live_socket_and_its_lock() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let dir = prune_key_dir(&root, &"b".repeat(32), true);
        let listener = std::os::unix::net::UnixListener::bind(dir.join("daemon.sock")).unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &std_probe),
            0
        );
        assert!(dir.join("daemon.sock").exists());
        assert!(dir.join("daemon.sock.lock").exists());
        assert!(std::os::unix::net::UnixStream::connect(dir.join("daemon.sock")).is_ok());
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_keeps_a_stale_socket_when_the_lock_is_held() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let dir = prune_key_dir(&root, &"b".repeat(32), true);
        drop(std::os::unix::net::UnixListener::bind(dir.join("daemon.sock")).unwrap());
        let holder = open_lock_holder(&dir.join("daemon.sock.lock"));
        holder.lock().unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &std_probe),
            0
        );
        assert!(dir.join("daemon.sock").exists());
        assert!(dir.join("daemon.sock.lock").exists());
        drop(holder);
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &std_probe),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_keeps_a_socket_that_is_not_ours_to_remove() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        // (a) a symlink named daemon.sock pointing at a victim file.
        let linked = prune_key_dir(&root, &"b".repeat(32), true);
        let victim = root.join("victim");
        std::fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, linked.join("daemon.sock")).unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &std_probe),
            0
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
        assert!(
            std::fs::symlink_metadata(linked.join("daemon.sock")).is_ok()
                && linked.join("daemon.sock.lock").exists()
        );
        // (b) a hard-linked (nlink 2) stale socket.
        let hard = prune_key_dir(&root, &"c".repeat(32), true);
        let outside = root.join("outside-sock");
        drop(std::os::unix::net::UnixListener::bind(&outside).unwrap());
        std::fs::hard_link(&outside, hard.join("daemon.sock")).unwrap();
        assert_eq!(
            prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &std_probe),
            0
        );
        assert!(outside.exists());
        assert!(hard.join("daemon.sock").exists());
        assert!(hard.join("daemon.sock.lock").exists());
        assert!(linked.join("daemon.sock.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn control_dir_prune_keeps_a_socket_when_the_probe_is_inconclusive() {
        let (_td, root) = prune_fixture();
        let own = prune_key_dir(&root, &"a".repeat(32), true);
        let dir = prune_key_dir(&root, &"b".repeat(32), true);
        drop(std::os::unix::net::UnixListener::bind(dir.join("daemon.sock")).unwrap());
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::TimedOut] {
            assert_eq!(
                prune_idle_control_dirs(&root, &own, std::time::Duration::ZERO, 64, &|_| Err(
                    io::Error::new(kind, "x")
                )),
                0
            );
            assert!(dir.join("daemon.sock").exists());
            assert!(dir.join("daemon.sock.lock").exists());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn control_dir_prune_wiring_fires_in_a_real_thegn_key_layout() {
        let (_td, root) = prune_fixture();
        let own = root.join("a".repeat(32));
        std::fs::create_dir(&own).unwrap();
        prepare_control_directory(&own, false).unwrap();
        let sibling = prune_key_dir(&root, &"b".repeat(32), true);
        // Backdate the sibling past the 24 h floor (futimens on a directory
        // fd works for the owner with std alone).
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 60 * 60);
        std::fs::File::open(&sibling)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let sock = own.join("d.sock");
        assert_socket_path_fits(&sock);
        let listener = match IpcListener::bind_exclusive(&IpcEndpoint::for_socket_path(&sock))
            .await
            .unwrap()
        {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        assert!(!sibling.join("daemon.sock.lock").exists());
        drop(listener);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn control_dir_prune_wiring_leaves_foreign_layouts_alone() {
        let td = tempfile::tempdir_in("/tmp").unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(td.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        prepare_control_directory(td.path(), false).unwrap();
        let sibling = prune_key_dir(td.path(), &"a".repeat(32), true);
        let sock = td.path().join("d.sock");
        assert_socket_path_fits(&sock);
        let listener = match IpcListener::bind_exclusive(&IpcEndpoint::for_socket_path(&sock))
            .await
            .unwrap()
        {
            BindOutcome::Bound(listener) => listener,
            BindOutcome::AlreadyRunning => panic!("fresh path must bind"),
        };
        assert!(sibling.join("daemon.sock.lock").exists());
        drop(listener);
    }
}
