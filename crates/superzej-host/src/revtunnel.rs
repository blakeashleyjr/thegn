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
use std::time::{Duration, Instant};

use superzej_svc::provider::{ExecSpec, Provider};
use superzej_svc::revtunnel::{TcpDialer, exec_stream, run_host};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// `(worktree, sandbox_port)` → its supervisor task (self-healing pump loop).
type Tasks = Arc<Mutex<HashMap<(String, u16), JoinHandle<()>>>>;

/// Reconnect backoff floor: the delay after the first failed/short-lived tunnel
/// attempt before the supervisor dials again.
const BACKOFF_BASE: Duration = Duration::from_millis(500);
/// Reconnect backoff ceiling: repeated failures cap here so a persistently-down
/// provider is retried at a steady, cheap cadence rather than hammered.
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A tunnel session that stayed up at least this long counts as "healthy" — the
/// next reconnect resets to [`BACKOFF_BASE`] instead of continuing to grow, so a
/// transient WSS blip on a long-lived tunnel doesn't push us toward the ceiling.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(10);

/// Next reconnect delay given how long the last attempt's session lasted and the
/// current backoff. A session that ran at least [`BACKOFF_RESET_AFTER`] resets to
/// the floor; anything shorter (a fast failure) doubles up to [`BACKOFF_MAX`].
/// Pure so the policy is unit-tested without driving the async loop.
fn next_backoff(session_lasted: Duration, current: Duration) -> Duration {
    if session_lasted >= BACKOFF_RESET_AFTER {
        BACKOFF_BASE
    } else {
        (current * 2).min(BACKOFF_MAX)
    }
}

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
            // Self-heal: keep the tunnel up for the life of the worktree. A single
            // WSS hiccup (exec open failure, or the pump ending) previously killed
            // the tunnel permanently — the entry stayed in the map so `start`
            // never re-ran it — which surfaces in-sandbox as `ConnectionRefused`
            // on the injected proxy URL. Now we reconnect with bounded backoff;
            // the task only ends when `stop_worktree` aborts it.
            let mut backoff = BACKOFF_BASE;
            loop {
                let started = Instant::now();
                match provider.open_exec(&sandbox_id, &spec).await {
                    Ok(session) => {
                        let stream = exec_stream(session);
                        if let Err(e) = run_host(
                            stream,
                            TcpDialer {
                                addr: host_target.clone(),
                            },
                        )
                        .await
                        {
                            superzej_core::msg::warn(&format!(
                                "reverse tunnel ended: {e} (reconnecting)"
                            ));
                        }
                    }
                    Err(e) => {
                        superzej_core::msg::warn(&format!(
                            "reverse tunnel exec failed: {e} (reconnecting)"
                        ));
                    }
                }
                backoff = next_backoff(started.elapsed(), backoff);
                tokio::time::sleep(backoff).await;
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
    fn backoff_grows_on_fast_failure_and_resets_when_healthy() {
        // A fast failure (session barely lasted) doubles the backoff …
        let b1 = next_backoff(Duration::from_millis(50), BACKOFF_BASE);
        assert_eq!(b1, BACKOFF_BASE * 2);
        let b2 = next_backoff(Duration::from_millis(50), b1);
        assert_eq!(b2, BACKOFF_BASE * 4);
        // … but never past the ceiling.
        let capped = next_backoff(Duration::from_millis(50), BACKOFF_MAX);
        assert_eq!(capped, BACKOFF_MAX);
        // A session that stayed up long enough resets to the floor.
        let reset = next_backoff(BACKOFF_RESET_AFTER, BACKOFF_MAX);
        assert_eq!(reset, BACKOFF_BASE);
        let reset_longer = next_backoff(Duration::from_secs(300), b2);
        assert_eq!(reset_longer, BACKOFF_BASE);
    }

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
