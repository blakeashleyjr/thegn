//! Host-side reverse-tunnel supervisor (P0b/P1).
//!
//! For a provider worktree, run an in-sandbox `szhost bridge-revtunnel <port>` (the
//! resident musl bridge) and pump [`superzej_svc::revtunnel::run_host`] over the
//! provider exec stream, so a process *inside* the sprite reaching
//! `127.0.0.1:<port>` transparently hits a real **host** service — by default the
//! local `szproxy` (so any agent there routes through the proxy), and (P1) host
//! `localhost` DB/API or a host-bound MCP server.
//!
//! The tunnel mechanics ([`run_host`]/`run_sandbox`/`exec_stream`) are mock-tested
//! in `superzej-svc`; this is the thin lifecycle glue (start per worktree, stop on
//! close), modeled on the forward/VPN supervisors. `Clone` (Arc inside) so it can
//! be captured into the off-loop `spawn_blocking` bridge-setup task.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use superzej_svc::provider::{ExecSpec, Provider};
use superzej_svc::revtunnel::{TcpDialer, exec_stream, run_host};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// `(worktree, sandbox_port)` → its pump task.
type Tasks = Arc<Mutex<HashMap<(String, u16), JoinHandle<()>>>>;

#[derive(Clone, Default)]
pub struct ReverseTunnelSupervisor {
    tasks: Tasks,
}

impl ReverseTunnelSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a tunnel is already running for `(worktree, sandbox_port)`.
    #[allow(dead_code)] // public API; exercised by tests, not yet by non-test callers
    pub fn has(&self, worktree: &str, sandbox_port: u16) -> bool {
        self.tasks
            .lock()
            .map(|t| t.contains_key(&(worktree.to_string(), sandbox_port)))
            .unwrap_or(false)
    }

    /// Start a reverse tunnel for `worktree`: bind `sandbox_port` inside the
    /// sandbox (via the resident bridge at `bridge_path`) and forward every
    /// connection to the host `host_target` (`host:port`, e.g. `127.0.0.1:8383`
    /// for `szproxy`). Idempotent per `(worktree, sandbox_port)`. `handle` is the
    /// tokio runtime to spawn the pump on (callers run inside `spawn_blocking`,
    /// which has no ambient runtime).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        handle: &Handle,
        worktree: &str,
        provider: Provider,
        sandbox_id: String,
        bridge_path: String,
        sandbox_port: u16,
        host_target: String,
    ) {
        let key = (worktree.to_string(), sandbox_port);
        let mut tasks = match self.tasks.lock() {
            Ok(t) => t,
            Err(_) => return,
        };
        if tasks.contains_key(&key) {
            return;
        }
        let task = handle.spawn(async move {
            let spec = ExecSpec {
                argv: vec![
                    bridge_path,
                    "bridge-revtunnel".to_string(),
                    sandbox_port.to_string(),
                ],
                tty: false,
                cols: 0,
                rows: 0,
                env: Vec::new(),
                cwd: None,
            };
            match provider.open_exec(&sandbox_id, &spec).await {
                Ok(session) => {
                    let stream = exec_stream(session);
                    if let Err(e) = run_host(stream, TcpDialer { addr: host_target }).await {
                        superzej_core::msg::warn(&format!("reverse tunnel ended: {e}"));
                    }
                }
                Err(e) => {
                    superzej_core::msg::warn(&format!("reverse tunnel exec failed: {e}"));
                }
            }
        });
        tasks.insert(key, task);
    }

    /// Stop and forget all tunnels for `worktree` (called on worktree close).
    pub fn stop_worktree(&self, worktree: &str) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.retain(|(wt, _), h| {
                if wt == worktree {
                    h.abort();
                    false
                } else {
                    true
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_tracks_and_clears_by_worktree() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _g = rt.enter();
        let sup = ReverseTunnelSupervisor::new();
        {
            let mut t = sup.tasks.lock().unwrap();
            t.insert(("/wt/a".into(), 8383), rt.spawn(async {}));
            t.insert(("/wt/a".into(), 5432), rt.spawn(async {}));
            t.insert(("/wt/b".into(), 8383), rt.spawn(async {}));
        }
        assert!(sup.has("/wt/a", 8383));
        assert!(sup.has("/wt/b", 8383));
        sup.stop_worktree("/wt/a");
        assert!(!sup.has("/wt/a", 8383));
        assert!(!sup.has("/wt/a", 5432));
        assert!(sup.has("/wt/b", 8383), "other worktree's tunnel survives");
    }
}
