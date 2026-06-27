//! `BridgeSupervisor` — makes the resident bridge agent *live* in the running
//! app. For a remote/provider worktree it spawns the agent (over the worktree's
//! transport), registers it so `superzej-svc::git` routes that worktree's git /
//! ops through the persistent connection, and forwards the agent's `fs.watch`
//! stream into the event loop as model refreshes. Tears the bridge down on
//! worktree close. Modeled on [`crate::lsp::LspSupervisor`].
//!
//! Notification is injected as a closure (`on_event`) rather than holding the
//! termwiz `TerminalWaker` / `RefreshKind` directly, so the supervisor is
//! unit-testable without a terminal and stays decoupled from the loop's channels.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use superzej_core::placement::Placement;
use superzej_core::remote::{GitLoc, ssh_base};
use superzej_svc::bridge::{self, BridgeClient};

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
            Err(e) => superzej_core::msg::warn(&format!("bridge connect failed: {e}")),
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
                    // Ends when the client drops (Sender gone → recv errs).
                    while rx.recv().is_ok() {
                        (on_event)();
                    }
                })
                .ok();
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
    let _ = GLOBAL.set(sup);
}

/// Disconnect a worktree's bridge by path (called from the close thread).
pub fn disconnect_path(host_path: &str) {
    if let Some(g) = GLOBAL.get() {
        g.disconnect_path(host_path);
    }
}

/// Default in-env path the static-musl `szhost` is pushed to (8-B.3). Until the
/// push lands, a provider connect using this will spawn-fail gracefully.
fn remote_szhost() -> String {
    "/workspace/.sz/szhost".to_string()
}

/// Build the transport command that runs `szhost bridge` *inside* the env for a
/// placement, or `None` when no bridge applies (local / k8s-for-now). The agent
/// reads framed requests on stdin and writes responses/notifications on stdout.
pub fn bridge_command(placement: &Placement) -> Option<Command> {
    match placement {
        Placement::Ssh(t) => {
            // ssh <ctrlmaster opts> <host> -- szhost bridge (remote has szhost).
            let mut argv = ssh_base(t.port, t.forward_agent, true);
            argv.push(t.host.clone());
            argv.push("szhost bridge".to_string());
            Some(argv_command(&argv))
        }
        Placement::Provider(p) => {
            // <control_prefix> <remote szhost> bridge.
            let mut argv = p.control_prefix.clone();
            argv.push(remote_szhost());
            argv.push("bridge".to_string());
            Some(argv_command(&argv))
        }
        Placement::Local | Placement::K8s(_) => None,
    }
}

fn argv_command(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use superzej_core::placement::{ProviderPlacement, SshPlacement, TransportKind};

    fn loopback_client() -> Arc<BridgeClient> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                bridge::serve(sock.try_clone().unwrap(), sock);
            }
        });
        let sock = TcpStream::connect(addr).unwrap();
        Arc::new(BridgeClient::new(sock.try_clone().unwrap(), sock))
    }

    #[test]
    fn bridge_command_per_placement() {
        // ssh: ssh … host -- "szhost bridge"
        let ssh = Placement::Ssh(SshPlacement::plain(
            "user@box".into(),
            22,
            false,
            TransportKind::Ssh,
        ));
        let c = bridge_command(&ssh).expect("ssh cmd");
        assert_eq!(c.get_program(), "ssh");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|a| a == "user@box"));
        assert!(args.iter().any(|a| a == "szhost bridge"));

        // provider: control_prefix + remote szhost + bridge
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
        let c = bridge_command(&prov).expect("provider cmd");
        assert_eq!(c.get_program(), "sprite");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().unwrap(), "bridge");

        // local: no bridge
        assert!(bridge_command(&Placement::Local).is_none());
    }

    #[test]
    fn connect_registers_routes_and_forwards_then_disconnect() {
        let dir = std::env::temp_dir().join(format!("sz-sup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
