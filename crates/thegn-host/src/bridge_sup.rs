//! `BridgeSupervisor` — makes the resident bridge agent *live* in the running
//! app. For a remote/provider worktree it spawns the agent (over the worktree's
//! transport), registers it so `thegn-svc::git` routes that worktree's git /
//! ops through the persistent connection, and forwards the agent's `fs.watch`
//! stream into the event loop as model refreshes. Tears the bridge down on
//! worktree close. Modeled on [`crate::lsp::LspSupervisor`].
//!
//! Notification is injected as a closure (`on_event`) rather than holding the
//! termwiz `TerminalWaker` / `RefreshKind` directly, so the supervisor is
//! unit-testable without a terminal and stays decoupled from the loop's channels.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use sha2::{Digest, Sha256};

use thegn_core::placement::Placement;
use thegn_core::remote::GitLoc;
use thegn_svc::bridge::{self, BridgeClient};
use thegn_svc::provider::{ExecControl, ExecFrame, ExecSession, ExecSpec, Provider};

/// Fired on every `fs.watch` event from any connected bridge — wired to
/// `refresh_tx.send(RefreshKind::Model)` + `waker.wake()` by `run.rs`.
type OnEvent = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct BridgeSupervisor {
    inner: Arc<Inner>,
}

struct Inner {
    on_event: OnEvent,
    /// bridge_key → live client.
    conns: Mutex<HashMap<String, Arc<BridgeClient>>>,
    /// host worktree path → bridge_key, so the close thread (which only has the
    /// path, and runs *after* the DB row is deleted) can find what to drop.
    paths: Mutex<HashMap<String, String>>,
}

impl BridgeSupervisor {
    pub fn new(on_event: OnEvent) -> BridgeSupervisor {
        BridgeSupervisor {
            inner: Arc::new(Inner {
                on_event,
                conns: Mutex::new(HashMap::new()),
                paths: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Already connected for this loc?
    pub fn is_connected(&self, loc: &GitLoc) -> bool {
        bridge::bridge_key(loc)
            .map(|k| self.inner.conns.lock().unwrap().contains_key(&k))
            .unwrap_or(false)
    }

    /// Spawn the agent via `cmd`, then register it + start the fs.watch forwarder.
    /// `host_path` is the local worktree path (the close thread's disconnect key).
    /// Best-effort: a spawn failure is logged and leaves the per-op git path as
    /// the fallback (no panic, never blocks — call off the event loop).
    pub fn connect(&self, loc: &GitLoc, workdir: &str, host_path: &str, cmd: Command) {
        if bridge::bridge_key(loc).is_none() || self.is_connected(loc) {
            return;
        }
        match BridgeClient::spawn(cmd) {
            Ok(client) => self.connect_client(loc, workdir, host_path, Arc::new(client)),
            Err(e) => thegn_core::msg::warn(&format!("bridge connect failed: {e}")),
        }
    }

    /// Like [`connect`](Self::connect) but over a provider's **native exec API** (no vendor CLI):
    /// run `thegn bridge` via `provider.open_exec(tty=false)` and talk to it over
    /// the resulting [`ExecSession`] channels. `rt` is the host runtime handle the
    /// session's driver task lives on. Best-effort: a failure leaves the per-op
    /// (CLI) git path as the fallback, same as [`connect`](Self::connect).
    pub(crate) fn connect_native(&self, args: NativeBridgeArgs<'_>) {
        let NativeBridgeArgs {
            loc,
            workdir,
            host_path,
            provider,
            sandbox_id,
            bridge,
            rt,
        } = args;
        if bridge::bridge_key(loc).is_none() || self.is_connected(loc) {
            return;
        }
        match open_bridge_native(&provider, &sandbox_id, bridge, &rt) {
            Ok(client) => self.connect_client(loc, workdir, host_path, Arc::new(client)),
            Err(e) => thegn_core::msg::warn(&format!("native bridge connect failed: {e}")),
        }
    }

    /// The no-spawn core (also the test seam): register an already-connected
    /// client and forward its `fs.watch(workdir)` events through `on_event`.
    pub(crate) fn connect_client(
        &self,
        loc: &GitLoc,
        workdir: &str,
        host_path: &str,
        client: Arc<BridgeClient>,
    ) {
        let Some(key) = bridge::bridge_key(loc) else {
            return;
        };
        {
            let mut conns = self.inner.conns.lock().unwrap();
            if conns.contains_key(&key) {
                return;
            }
            conns.insert(key.clone(), client.clone());
        }
        self.inner
            .paths
            .lock()
            .unwrap()
            .insert(host_path.to_string(), key.clone());
        bridge::register(&key, client.clone());
        if let Ok(rx) = client.watch(workdir) {
            let on_event = self.inner.on_event.clone();
            std::thread::Builder::new()
                .name("bridge-fswatch".into())
                .spawn(move || {
                    // Housekeeping: an fs-watch event pump that lives as long as
                    // the client; nobody blocks on the notification it relays.
                    crate::platform::qos::set_self(crate::platform::qos::Qos::Background);
                    // Ends when the client drops (Sender gone → recv errs).
                    while rx.recv().is_ok() {
                        (on_event)();
                    }
                })
                .inspect_err(|e| {
                    tracing::warn!(target: "thegn::bridge", error = %e, "bridge fs-watch pump failed to spawn");
                })
                .ok(); // best-effort: spawn failure is now logged above
        }
    }

    /// Disconnect a worktree's bridge (on close) by host worktree path — the
    /// close thread only has the path, and runs after the DB row (and thus the
    /// provider key) is gone. Unregisters + drops the client, which kills the
    /// agent and ends the forwarder thread (channel closes).
    pub fn disconnect_path(&self, host_path: &str) {
        let key = self.inner.paths.lock().unwrap().remove(host_path);
        if let Some(key) = key {
            self.drop_by_key(&key);
        }
    }

    fn drop_by_key(&self, key: &str) {
        bridge::drop_key(key);
        self.inner.conns.lock().unwrap().remove(key);
        self.inner.paths.lock().unwrap().retain(|_, k| k != key);
    }
}

/// Process-global supervisor handle, set once by the event loop, so the
/// fire-and-forget worktree-close thread (a free function with only the path)
/// can tear the bridge down. `None` until `set_global` runs.
static GLOBAL: OnceLock<BridgeSupervisor> = OnceLock::new();

pub fn set_global(sup: BridgeSupervisor) {
    let _ = GLOBAL.set(sup); // best-effort: first-set-wins: the first supervisor serves for the process
}

/// Disconnect a worktree's bridge by path (called from the close thread).
pub fn disconnect_path(host_path: &str) {
    if let Some(g) = GLOBAL.get() {
        g.disconnect_path(host_path);
    }
}

/// Read-only automatic-collection guard; never reconnect or drop a bridge.
pub(crate) fn automatic_cleanup_resources_absent(path: &str) -> Result<(), String> {
    if let Some(global) = GLOBAL.get() {
        let paths = global
            .inner
            .paths
            .lock()
            .map_err(|_| "bridge registry unavailable")?;
        if paths.contains_key(path) {
            return Err("attached bridge requires explicit owned cleanup".into());
        }
    }
    Ok(())
}

/// Provider-private root for content-addressed resident bridge installs.
pub const BRIDGE_ROOT: &str = "/var/tmp/thegn-bridge";

/// Verified resident bridge executable installed for exact local bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentBridge {
    path: String,
    sha256: String,
}

pub(crate) struct NativeBridgeArgs<'a> {
    pub(crate) loc: &'a GitLoc,
    pub(crate) workdir: &'a str,
    pub(crate) host_path: &'a str,
    pub(crate) provider: Provider,
    pub(crate) sandbox_id: String,
    pub(crate) bridge: &'a ResidentBridge,
    pub(crate) rt: tokio::runtime::Handle,
}

impl ResidentBridge {
    /// Construct a resident bridge reference from a lowercase SHA-256 digest.
    pub fn from_digest(hex: &str) -> Option<Self> {
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        Some(Self {
            path: format!("{BRIDGE_ROOT}/{hex}/thegn"),
            sha256: hex.to_string(),
        })
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// Bind the remote path to the digest of the exact local executable bytes.
pub fn resident_bridge_for(data: &[u8]) -> ResidentBridge {
    let sha256 = format!("{:x}", Sha256::digest(data));
    ResidentBridge::from_digest(&sha256).expect("SHA-256 digest is lowercase hex")
}

/// Return the content-addressed parent directory for a resident bridge.
pub fn resident_bridge_dir(bridge: &ResidentBridge) -> String {
    format!("{BRIDGE_ROOT}/{}", bridge.sha256())
}

const VERIFY_EXEC: &str = r#"p=$1; want=$2; shift 2; got=$(sha256sum -- "$p" 2>/dev/null) || { echo 'thegn: cannot hash the resident bridge' >&2; exit 126; }; [ "${got%% *}" = "$want" ] || { echo 'thegn: resident bridge digest mismatch' >&2; exit 126; }; exec "$p" "$@""#;

/// Run a native executable only after its current bytes match the install digest.
pub fn verified_exec_argv(bridge: &ResidentBridge, args: &[String]) -> Vec<String> {
    let mut argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        VERIFY_EXEC.to_string(),
        "thegn-bridge".to_string(),
        bridge.path().to_string(),
        bridge.sha256().to_string(),
    ];
    argv.extend_from_slice(args);
    argv
}

/// The local static-musl `thegn` to push into a remote env, or `None` to skip
/// the push (the bridge then falls back to the per-op git path). Resolution:
/// `THEGN_BRIDGE_BINARY` env (an explicit artifact path, e.g. set by the nix
/// wrapper) → else a `thegn-musl` sitting next to the running executable → else
/// none. We never push the running exe itself: it's likely a glibc/host-arch
/// build, wrong for the env's musl/Firecracker target.
pub fn bridge_binary_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("THEGN_BRIDGE_BINARY") {
        let p = std::path::PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // `self_exe_path`: a rebuilt-in-place binary reads back as `<path>
    // (deleted)`, whose `.parent()` is the right directory only by accident of
    // the marker containing no separator — and whose sibling probe below then
    // still works, but on a path we never validated. Resolve it properly.
    if let Some(exe) = thegn_core::util::self_exe_path()
        && let Some(dir) = exe.parent()
    {
        let cand = dir.join("thegn-musl");
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// The program of an ssh transport prefix: the last `/`- or `\`-separated
/// component is exactly `ssh`, or `ssh.exe` in any ASCII case (Windows
/// OpenSSH; Windows file names are case-insensitive).
fn is_ssh_program(program: &str) -> bool {
    let name = program.rsplit(['/', '\\']).next().unwrap_or("");
    name == "ssh"
        || (name.len() == 7
            && name.as_bytes()[..3].eq_ignore_ascii_case(b"ssh")
            && name.as_bytes()[3..].eq_ignore_ascii_case(b".exe"))
}

fn ssh_self_bridge(control_prefix: &[String]) -> bool {
    let thegn_self_bridge = control_prefix.len() == 4
        && matches!(control_prefix[1].as_str(), "vps-ssh" | "machine0-ssh")
        && control_prefix[3] == "--";
    let ssh_program_prefix = control_prefix.len() >= 2 && is_ssh_program(&control_prefix[0]);

    thegn_self_bridge || ssh_program_prefix
}

fn provider_bridge_argv(control_prefix: &[String], bridge: &ResidentBridge) -> Option<Vec<String>> {
    if control_prefix.is_empty() {
        return None;
    }
    let verified = verified_exec_argv(bridge, &["bridge".to_string()]);
    let mut argv = control_prefix.to_vec();
    if ssh_self_bridge(control_prefix) {
        argv.push(thegn_core::util::sh_join(&verified));
    } else {
        argv.extend(verified);
    }
    Some(argv)
}

/// Build the transport command that runs `thegn bridge` *inside* the env for a
/// placement, or `None` when no bridge applies (local / k8s-for-now). Provider
/// launches re-hash the resident bridge before exec; ssh transports (thegn's
/// self-bridges and a configured `ssh`/`ssh.exe` `exec_command`) receive that
/// wrapper as one shell-quoted word. The agent reads framed requests on stdin
/// and writes responses/notifications on stdout.
pub fn bridge_command(placement: &Placement, bridge: Option<&ResidentBridge>) -> Option<Command> {
    match placement {
        Placement::Ssh(t) => {
            // ssh <ctrlmaster opts> <host> -- thegn bridge (remote has thegn).
            let mut argv = t.ssh_base(true);
            argv.push(t.host.clone());
            argv.push("thegn bridge".to_string());
            Some(argv_command(&argv))
        }
        Placement::Provider(p) => {
            let bridge = bridge?;
            provider_bridge_argv(&p.control_prefix, bridge).map(|argv| argv_command(&argv))
        }
        Placement::Local | Placement::K8s(_) => None,
    }
}

fn argv_command(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

/// Open the resident bridge over a provider's native exec API and build a
/// [`BridgeClient`] on its [`ExecSession`] channels. The session's driver task
/// runs on `rt` (the host runtime) and outlives this call, so the bridge stays
/// live; it ends when the client (and thus the `ControlWriter`) drops.
fn open_bridge_native(
    provider: &Provider,
    sandbox_id: &str,
    bridge: &ResidentBridge,
    rt: &tokio::runtime::Handle,
) -> anyhow::Result<BridgeClient> {
    let spec = ExecSpec {
        argv: verified_exec_argv(bridge, &["bridge".to_string()]),
        tty: false,
        cols: 0,
        rows: 0,
        env: Vec::new(),
        cwd: None,
    };
    // block_on is valid here: callers run on a `spawn_blocking` thread (not a
    // runtime worker), and `open_exec`'s driver `tokio::spawn` lands on `rt`.
    let session = rt.block_on(provider.open_exec(sandbox_id, &spec))?;
    let ExecSession {
        frames, control, ..
    } = session;
    Ok(BridgeClient::new(
        FramesReader::new(frames),
        ControlWriter { tx: control },
    )?)
}

/// Adapts an [`ExecSession`]'s stdout frames into a blocking [`Read`] for the
/// `BridgeClient` reader thread. `Exit(0)` is EOF; nonzero exits and a closed
/// channel without an exit status are read errors.
/// Uses `blocking_recv` — only sound off a runtime worker (the bridge reader is
/// a dedicated std::thread, which satisfies that).
struct FramesReader {
    rx: tokio::sync::mpsc::Receiver<ExecFrame>,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl FramesReader {
    fn new(rx: tokio::sync::mpsc::Receiver<ExecFrame>) -> Self {
        FramesReader {
            rx,
            buf: Vec::new(),
            pos: 0,
            done: false,
        }
    }
}

impl Read for FramesReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        // `blocking_recv` panics inside the tokio runtime; this adapter is only
        // ever driven by the dedicated `bridge-reader` std thread (no runtime
        // context). Lock that invariant in so a future rewiring can't regress it.
        debug_assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "FramesReader::read must run off the tokio runtime",
        );
        loop {
            if self.pos < self.buf.len() {
                let n = (self.buf.len() - self.pos).min(out.len());
                out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.done {
                return Ok(0);
            }
            match self.rx.blocking_recv() {
                Some(ExecFrame::Stdout(d)) => {
                    self.buf = d;
                    self.pos = 0;
                }
                Some(ExecFrame::Exit(0)) => {
                    self.done = true;
                    return Ok(0);
                }
                Some(ExecFrame::Exit(code)) => {
                    self.done = true;
                    return Err(io::Error::other(format!(
                        "bridge exited with status {code}"
                    )));
                }
                None => {
                    self.done = true;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "bridge stream ended without an exit status",
                    ));
                }
            }
        }
    }
}

/// Adapts blocking [`Write`] into an [`ExecSession`]'s stdin control channel.
/// Uses the **non-blocking** `try_send`, so it is sound on ANY thread — the main
/// event loop, a runtime worker, `spawn_blocking`, or a plain std thread. That
/// matters because `BridgeClient::call` drives this `write` on whatever thread
/// issued the git/`exec` op, and several of those are inside the tokio runtime
/// (where `blocking_send` would panic with "Cannot block ... within a runtime").
/// Each `BridgeClient::call` writes one framed request in a single `write_all`,
/// so one `write` == one whole frame == one channel item (never a partial frame).
struct ControlWriter {
    tx: tokio::sync::mpsc::Sender<ExecControl>,
}

impl Write for ControlWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        use tokio::sync::mpsc::error::TrySendError;
        match self.tx.try_send(ExecControl::Stdin(data.to_vec())) {
            Ok(()) => Ok(data.len()),
            // Full ⇒ fast-fail: `BridgeClient::call` bails on a write error, so the op
            // errors out and retries next refresh rather than eating the 120s response
            // timeout. The control channel is bounded (256); with the id-correlated,
            // ~synchronous-per-connection protocol and small git payloads, Full is
            // unreachable in practice, but degrade cleanly if it ever isn't.
            Err(TrySendError::Full(_)) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "bridge control channel full",
            )),
            Err(TrySendError::Closed(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "exec session closed",
            )),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use thegn_core::placement::{ProviderPlacement, SshPlacement, TransportKind};

    fn loopback_client() -> Arc<BridgeClient> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                bridge::serve(sock.try_clone().unwrap(), sock);
            }
        });
        let sock = TcpStream::connect(addr).unwrap();
        Arc::new(BridgeClient::new(sock.try_clone().unwrap(), sock).unwrap())
    }

    #[test]
    fn exec_session_adapter_roundtrips_and_eofs() {
        // The ExecSession→Read/Write adapter the native bridge transport uses.
        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(8);
        let (control_tx, mut control_rx) = tokio::sync::mpsc::channel::<ExecControl>(8);
        let mut reader = FramesReader::new(frames_rx);
        let mut writer = ControlWriter { tx: control_tx };

        // Write side: bytes become an ExecControl::Stdin.
        writer.write_all(b"ping").unwrap();
        assert_eq!(
            control_rx.blocking_recv(),
            Some(ExecControl::Stdin(b"ping".to_vec()))
        );

        // Read side: stdout frames stream out across buffer boundaries, Exit ⇒ EOF.
        frames_tx
            .blocking_send(ExecFrame::Stdout(b"ab".to_vec()))
            .unwrap();
        frames_tx
            .blocking_send(ExecFrame::Stdout(b"cd".to_vec()))
            .unwrap();
        frames_tx.blocking_send(ExecFrame::Exit(0)).unwrap();
        let mut out = Vec::new();
        let mut tmp = [0u8; 3];
        loop {
            let n = reader.read(&mut tmp).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&tmp[..n]);
        }
        assert_eq!(out, b"abcd");
    }

    #[test]
    fn frames_reader_reports_a_nonzero_exit_as_an_error() {
        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(8);
        let mut reader = FramesReader::new(frames_rx);

        frames_tx
            .blocking_send(ExecFrame::Stdout(b"ab".to_vec()))
            .unwrap();
        frames_tx.blocking_send(ExecFrame::Exit(3)).unwrap();
        let mut out = [0u8; 8];
        assert_eq!(reader.read(&mut out).unwrap(), 2);
        assert_eq!(&out[..2], b"ab");
        let error = reader.read(&mut out).unwrap_err();
        assert!(error.to_string().contains("status 3"));
        assert_eq!(reader.read(&mut out).unwrap(), 0);
    }

    #[test]
    fn frames_reader_reports_a_dropped_stream_as_unexpected_eof() {
        let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<ExecFrame>(8);
        let mut reader = FramesReader::new(frames_rx);

        frames_tx
            .blocking_send(ExecFrame::Stdout(b"x".to_vec()))
            .unwrap();
        drop(frames_tx);
        let mut out = [0u8; 8];
        assert_eq!(reader.read(&mut out).unwrap(), 1);
        assert_eq!(&out[..1], b"x");
        let error = reader.read(&mut out).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn control_writer_write_is_panic_free_inside_runtime() {
        // Regression: git-over-bridge routes through `ControlWriter::write` on
        // whatever thread issued the op — often a runtime worker or the main
        // thread. `blocking_send` panicked there ("Cannot block ... within a
        // runtime"); `try_send` must not. Write from INSIDE a runtime worker and
        // assert both no-panic and that the bytes still reach the control channel.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ExecControl>(8);
        let got = rt.block_on(async move {
            tokio::spawn(async move {
                let mut w = ControlWriter { tx };
                w.write_all(b"hi").unwrap(); // pre-fix: PANIC; post-fix: Ok
            })
            .await
            .unwrap();
            rx.recv().await
        });
        assert_eq!(got, Some(ExecControl::Stdin(b"hi".to_vec())));
    }

    #[test]
    fn bridge_command_per_placement() {
        // ssh: ssh … host -- "thegn bridge"
        let ssh = Placement::Ssh(SshPlacement::plain(
            "user@box".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        let c = bridge_command(&ssh, None).expect("ssh cmd");
        assert_eq!(c.get_program(), "ssh");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "user@box"));
        assert!(args.iter().any(|a| a == "thegn bridge"));

        // provider: control_prefix + remote thegn + bridge
        let prov = Placement::Provider(ProviderPlacement {
            provider: "sprites".into(),
            id: "s1".into(),
            interactive_prefix: vec![],
            control_prefix: vec![
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "s1".into(),
                "--".into(),
            ],
            up_command: vec![],
            down_command: vec![],
        });
        let bridge = resident_bridge_for(b"provider bridge");
        let c = bridge_command(&prov, Some(&bridge)).expect("provider cmd");
        assert_eq!(c.get_program(), "sprite");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().unwrap(), "bridge");
        assert!(args.iter().any(|arg| arg == bridge.path()));

        // local: no bridge
        assert!(bridge_command(&Placement::Local, None).is_none());
    }

    #[test]
    fn resident_bridge_path_is_content_addressed_outside_the_worktree() {
        let bridge = resident_bridge_for(b"abc");
        assert_eq!(
            bridge.sha256(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            bridge.path(),
            "/var/tmp/thegn-bridge/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad/thegn"
        );
        assert!(!bridge.path().starts_with("/workspace"));
        assert_ne!(bridge.path(), resident_bridge_for(b"different").path());
        assert_eq!(
            resident_bridge_dir(&bridge),
            format!("{BRIDGE_ROOT}/{}", bridge.sha256())
        );
    }

    #[test]
    fn resident_bridge_from_digest_rejects_invalid_hex() {
        let invalid_char = "g".repeat(64);
        let uppercase = "A".repeat(64);
        let short = "a".repeat(63);
        for invalid in ["", "abc", &invalid_char, &uppercase, &short] {
            assert!(
                ResidentBridge::from_digest(invalid).is_none(),
                "{invalid:?}"
            );
        }
        let valid = "0123456789abcdef".repeat(4);
        let bridge = ResidentBridge::from_digest(&valid).expect("valid lowercase SHA-256");
        assert_eq!(bridge.sha256(), valid);
        assert_eq!(bridge.path(), format!("{BRIDGE_ROOT}/{valid}/thegn"));
    }

    #[test]
    fn verified_exec_argv_checks_the_digest_before_exec() {
        let bridge = resident_bridge_for(b"abc");
        let args = vec!["bridge-revtunnel".to_string(), "8383".to_string()];
        let argv = verified_exec_argv(&bridge, &args);
        assert_eq!(&argv[..2], &["/bin/sh", "-c"]);
        assert_eq!(
            &argv[3..6],
            &["thegn-bridge", bridge.path(), bridge.sha256()]
        );
        assert_eq!(&argv[6..], args.as_slice());
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "exercise the local shell verification wrapper"
    )]
    fn verified_exec_wrapper_refuses_mismatched_bytes() {
        if !std::path::Path::new("/bin/sh").exists() {
            // This host has no local POSIX shell to exercise the wrapper.
            return;
        }
        let probe = std::process::Command::new("sha256sum")
            .arg("--version")
            .output();
        if !probe.is_ok_and(|result| result.status.success()) {
            // This host has no sha256sum implementation for the wrapper.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("bridge.sh");
        let bytes = b"#!/bin/sh\nprintf '%s\\n' \"$1\"\n";
        std::fs::write(&script, bytes).unwrap();
        let chmod = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "chmod 0700 -- \"$1\"",
                "chmod-bridge",
                script.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(
            chmod.success(),
            "local shell could not mark the bridge executable"
        );
        let bridge = resident_bridge_for(bytes);
        let mut argv = verified_exec_argv(&bridge, &["ok".to_string()]);
        argv[4] = script.to_string_lossy().into_owned();
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        let mut success = command.output().unwrap();
        if success.status.code() == Some(126)
            && !String::from_utf8_lossy(&success.stderr).contains("digest mismatch")
        {
            // A sibling test process can briefly hold the executable open across fork/exec.
            success = command.output().unwrap();
        }
        assert!(success.status.success());
        assert_eq!(success.stdout, b"ok\n");

        let altered = b"#!/bin/sh\nprintf 'evil\\n'\n";
        std::fs::write(&script, altered).unwrap();
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]);
        let refused = command.output().unwrap();
        assert_eq!(refused.status.code(), Some(126));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("digest mismatch"));
    }

    #[test]
    fn bridge_command_provider_requires_a_verified_bridge() {
        let provider = Placement::Provider(ProviderPlacement {
            provider: "sprites".into(),
            id: "s1".into(),
            interactive_prefix: vec![],
            control_prefix: vec!["sprite".into(), "exec".into(), "--".into()],
            up_command: vec![],
            down_command: vec![],
        });
        assert!(bridge_command(&provider, None).is_none());
        let bridge = resident_bridge_for(b"verified");
        let command = bridge_command(&provider, Some(&bridge)).expect("verified provider command");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg == bridge.path()));
        assert_eq!(args.last().map(String::as_str), Some("bridge"));
    }

    #[test]
    fn provider_bridge_argv_wraps_the_helper_in_the_digest_check() {
        let prefix = vec![
            "sprite".to_string(),
            "exec".to_string(),
            "-s".to_string(),
            "s1".to_string(),
            "--".to_string(),
        ];
        let bridge = resident_bridge_for(b"provider bridge");
        let mut expected = prefix.clone();
        expected.extend(verified_exec_argv(&bridge, &["bridge".to_string()]));

        assert_eq!(provider_bridge_argv(&prefix, &bridge), Some(expected));
    }

    #[test]
    fn provider_bridge_argv_quotes_one_word_for_ssh_self_bridges() {
        let bridge = resident_bridge_for(b"provider bridge");
        let verified = verified_exec_argv(&bridge, &["bridge".to_string()]);
        for kind in ["vps-ssh", "machine0-ssh"] {
            let prefix = vec![
                "/opt/thegn".to_string(),
                kind.to_string(),
                "tg-a-1".to_string(),
                "--".to_string(),
            ];
            let argv = provider_bridge_argv(&prefix, &bridge).expect("ssh bridge argv");

            assert_eq!(argv.len(), 5);
            assert_eq!(&argv[..4], prefix.as_slice());
            assert_eq!(argv.last(), Some(&thegn_core::util::sh_join(&verified)));
        }
    }

    #[test]
    fn provider_bridge_ssh_exec_command_gets_one_quoted_word() {
        let bridge = resident_bridge_for(b"provider bridge");
        let verified = verified_exec_argv(&bridge, &["bridge".to_string()]);
        let prefixes: [Vec<String>; 3] = [
            vec!["ssh".into(), "box".into(), "--".into()],
            vec![
                "/usr/bin/ssh".into(),
                "-p".into(),
                "2222".into(),
                "u@h".into(),
                "--".into(),
            ],
            vec![
                "ssh".into(),
                "-o".into(),
                "BatchMode=yes".into(),
                "box".into(),
            ],
        ];

        for prefix in prefixes {
            let argv = provider_bridge_argv(&prefix, &bridge).expect("ssh provider argv");

            assert_eq!(argv.len(), prefix.len() + 1);
            assert_eq!(&argv[..prefix.len()], prefix.as_slice());
            assert_eq!(argv.last(), Some(&thegn_core::util::sh_join(&verified)));
        }
    }

    #[test]
    fn provider_bridge_ssh_exec_windows_program_gets_one_quoted_word() {
        let bridge = resident_bridge_for(b"provider bridge");
        let verified = verified_exec_argv(&bridge, &["bridge".to_string()]);
        let prefixes: [Vec<String>; 5] = [
            vec!["ssh.exe".into(), "box".into(), "--".into()],
            vec!["SSH.EXE".into(), "box".into(), "--".into()],
            vec![
                "Ssh.Exe".into(),
                "-p".into(),
                "2222".into(),
                "u@h".into(),
                "--".into(),
            ],
            vec![
                r"C:\Windows\System32\OpenSSH\ssh.exe".into(),
                "box".into(),
                "--".into(),
            ],
            vec!["C:/Program Files/OpenSSH/ssh.exe".into(), "box".into()],
        ];

        for prefix in prefixes {
            let argv = provider_bridge_argv(&prefix, &bridge).expect("Windows ssh provider argv");

            assert_eq!(argv.len(), prefix.len() + 1, "prefix: {prefix:?}");
            assert_eq!(
                &argv[..prefix.len()],
                prefix.as_slice(),
                "prefix: {prefix:?}"
            );
            assert_eq!(
                argv.last(),
                Some(&thegn_core::util::sh_join(&verified)),
                "prefix: {prefix:?}"
            );
        }
    }

    #[test]
    fn provider_bridge_ssh_exec_windows_program_rule() {
        let cases = [
            ("ssh", true),
            ("/usr/bin/ssh", true),
            ("ssh.exe", true),
            ("SSH.EXE", true),
            ("sSh.ExE", true),
            (r"C:\x\ssh.exe", true),
            ("C:/x/ssh.exe", true),
            (r"relative\ssh", true),
            ("SSH", false),
            ("ssh.exe.bak", false),
            ("sshd.exe", false),
            ("ssh.cmd", false),
            ("ssh.bat", false),
            ("autossh.exe", false),
            ("plink.exe", false),
            ("ssh.exe ", false),
            (r"C:\x\ssh.exe\", false),
            ("", false),
            ("ssh-wrapper", false),
            ("sshé.exe", false),
        ];

        for (program, expected) in cases {
            assert_eq!(is_ssh_program(program), expected, "program: {program:?}");
        }
    }

    #[test]
    fn provider_bridge_ssh_exec_windows_lone_program_keeps_separate_args() {
        let prefix = vec!["ssh.exe".to_string()];
        let bridge = resident_bridge_for(b"provider bridge");
        let mut expected = prefix.clone();
        expected.extend(verified_exec_argv(&bridge, &["bridge".to_string()]));

        assert_eq!(provider_bridge_argv(&prefix, &bridge), Some(expected));
    }

    #[test]
    fn provider_bridge_ssh_exec_command_builds_an_ssh_command() {
        let bridge = resident_bridge_for(b"provider bridge");
        let verified = verified_exec_argv(&bridge, &["bridge".to_string()]);
        let provider = Placement::Provider(ProviderPlacement {
            provider: "custom".into(),
            id: "box".into(),
            interactive_prefix: vec![],
            control_prefix: vec!["ssh".into(), "box".into(), "--".into()],
            up_command: vec![],
            down_command: vec![],
        });

        let command = bridge_command(&provider, Some(&bridge)).expect("provider command");
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(command.get_program(), std::ffi::OsStr::new("ssh"));
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], "box");
        assert_eq!(args[1], "--");
        assert_eq!(args[2], thegn_core::util::sh_join(&verified));
    }

    #[test]
    // positive control: only a program exactly named ssh is shell-joined.
    fn provider_bridge_ssh_exec_lookalikes_keep_separate_args() {
        let bridge = resident_bridge_for(b"provider bridge");
        let verified = verified_exec_argv(&bridge, &["bridge".to_string()]);
        let prefixes: [Vec<String>; 7] = [
            vec!["daytona".into(), "ssh".into(), "box".into(), "--".into()],
            vec!["autossh".into(), "box".into(), "--".into()],
            vec!["ssh-wrapper".into(), "box".into(), "--".into()],
            vec!["sshpass".into(), "box".into(), "--".into()],
            vec!["/opt/sshd".into(), "box".into()],
            vec![
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "s1".into(),
                "--".into(),
            ],
            vec!["ssh".into()],
        ];

        for prefix in prefixes {
            let mut expected = prefix.clone();
            expected.extend(verified.clone());

            assert_eq!(provider_bridge_argv(&prefix, &bridge), Some(expected));
        }
    }

    #[test]
    fn provider_bridge_command_refuses_an_empty_control_prefix() {
        let provider = Placement::Provider(ProviderPlacement {
            provider: "sprites".into(),
            id: "s1".into(),
            interactive_prefix: vec![],
            control_prefix: vec![],
            up_command: vec![],
            down_command: vec![],
        });
        let bridge = resident_bridge_for(b"provider bridge");

        assert!(bridge_command(&provider, Some(&bridge)).is_none());
    }

    #[test]
    fn provider_bridge_command_runs_the_digest_wrapper() {
        let provider = Placement::Provider(ProviderPlacement {
            provider: "sprites".into(),
            id: "s1".into(),
            interactive_prefix: vec![],
            control_prefix: vec![
                "sprite".into(),
                "exec".into(),
                "-s".into(),
                "s1".into(),
                "--".into(),
            ],
            up_command: vec![],
            down_command: vec![],
        });
        let bridge = resident_bridge_for(b"provider bridge");
        let command = bridge_command(&provider, Some(&bridge)).expect("provider command");
        let program = command.get_program().to_string_lossy();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();

        assert_eq!(program, "sprite");
        assert!(args.iter().any(|arg| arg == "/bin/sh"));
        assert!(args.iter().any(|arg| arg == "-c"));
        assert!(args.iter().any(|arg| arg == bridge.sha256()));
        assert_eq!(args.last().map(String::as_str), Some("bridge"));
    }

    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "exercise the shell-quoted provider verification wrapper locally"
    )]
    fn provider_bridge_ssh_word_runs_only_matching_bytes() {
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        let probe = std::process::Command::new("sha256sum")
            .arg("--version")
            .output();
        if !probe.is_ok_and(|result| result.status.success()) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("bridge.sh");
        let bytes = b"#!/bin/sh\nprintf '%s\\n' \"$1\"\n";
        std::fs::write(&script, bytes).unwrap();
        let chmod = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                "chmod 0700 -- \"$1\"",
                "chmod-bridge",
                script.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(
            chmod.success(),
            "local shell could not mark the bridge executable"
        );
        let bridge = resident_bridge_for(bytes);
        let mut verified = verified_exec_argv(&bridge, &["ok".to_string()]);
        verified[4] = script.to_string_lossy().into_owned();
        // Positive control for sh_join: the remote login shell re-parses this one word.
        let word = thegn_core::util::sh_join(&verified);
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", word.as_str()]);
        let mut success = command.output().unwrap();
        if success.status.code() == Some(126)
            && !String::from_utf8_lossy(&success.stderr).contains("digest mismatch")
        {
            success = command.output().unwrap();
        }
        assert!(success.status.success());
        assert_eq!(success.stdout, b"ok\n");

        let altered = b"#!/bin/sh\nprintf 'evil\\n'\n";
        std::fs::write(&script, altered).unwrap();
        let refused = command.output().unwrap();
        assert_eq!(refused.status.code(), Some(126));
        assert!(String::from_utf8_lossy(&refused.stderr).contains("digest mismatch"));
    }

    #[test]
    fn connect_registers_routes_and_forwards_then_disconnect() {
        let dir = std::env::temp_dir().join(format!("tg-sup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
        std::fs::create_dir_all(&dir).unwrap();
        let d = dir.to_string_lossy().into_owned();

        let fired = Arc::new(AtomicUsize::new(0));
        let f2 = fired.clone();
        let sup = BridgeSupervisor::new(Arc::new(move || {
            f2.fetch_add(1, Ordering::SeqCst);
        }));

        // A unique provider loc keyed to this test; client over loopback serve.
        let loc = GitLoc::provider(vec!["sup-test".into(), d.clone()], d.clone());
        sup.connect_client(&loc, &d, &d, loopback_client());

        // Registered → git routes through it.
        assert!(bridge::for_loc(&loc).is_some());
        assert!(sup.is_connected(&loc));

        // A file change fires the fs.watch forwarder → on_event.
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let start = Instant::now();
        while fired.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            fired.load(Ordering::SeqCst) > 0,
            "fs.watch never fired on_event"
        );

        sup.disconnect_path(&d);
        assert!(bridge::for_loc(&loc).is_none());
        assert!(!sup.is_connected(&loc));
        let _ = std::fs::remove_dir_all(&dir); // best-effort: cleanup: the target may already be gone; a failed removal never fails the caller
    }
}
