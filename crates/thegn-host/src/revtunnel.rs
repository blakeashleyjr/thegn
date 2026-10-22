//! Host-side reverse-tunnel supervisor (P0b/P1).
//!
//! For a provider worktree, run an in-sandbox `thegn bridge-revtunnel <port>` (the
//! resident musl bridge) and pump [`thegn_svc::revtunnel::run_host`] over the
//! provider exec stream, so a process *inside* the sprite reaching
//! `127.0.0.1:<port>` transparently hits a real **host** service — e.g. the
//! embedded host nix cache, a host `localhost` DB/API, or a host-bound MCP
//! server.
//!
//! The tunnel mechanics ([`run_host`]/`run_sandbox`/`exec_stream`) are mock-tested
//! in `thegn-svc`; this is the thin lifecycle glue (start per worktree, stop on
//! close), modeled on the forward/VPN supervisors. `Clone` (Arc inside) so it can
//! be captured into the off-loop `spawn_blocking` bridge-setup task.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use thegn_core::config::TunnelDeadlineSecs;
use thegn_svc::provider::{ExecSpec, Provider};
use thegn_svc::revtunnel::{
    HostLimits, TcpDialer, TunnelDeadlines, clamp_permits, exec_stream, run_host_with_limits,
};
use tokio::runtime::Handle;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

/// Emit the process-wide reverse-tunnel rejection deltas for one perf interval.
pub(crate) fn report_rejection_counts(interval_seconds: f64) {
    static PREVIOUS: OnceLock<Mutex<thegn_svc::revtunnel::RejectionCounts>> = OnceLock::new();
    let current = thegn_svc::revtunnel::rejection_counts();
    let previous = PREVIOUS.get_or_init(|| Mutex::new(Default::default()));
    let mut previous = match previous.lock() {
        Ok(previous) => previous,
        Err(_) => return,
    };
    let delta = thegn_svc::revtunnel::RejectionCounts {
        connection_limit: current
            .connection_limit
            .saturating_sub(previous.connection_limit),
        global_connection_limit: current
            .global_connection_limit
            .saturating_sub(previous.global_connection_limit),
        duplicate_id: current.duplicate_id.saturating_sub(previous.duplicate_id),
        dial_timeout: current.dial_timeout.saturating_sub(previous.dial_timeout),
        accept_transient: current
            .accept_transient
            .saturating_sub(previous.accept_transient),
        write_stall: current.write_stall.saturating_sub(previous.write_stall),
        idle: current.idle.saturating_sub(previous.idle),
        lifetime: current.lifetime.saturating_sub(previous.lifetime),
    };
    *previous = current;
    if delta == Default::default() {
        return;
    }
    tracing::info!(
        target: "thegn::perf::revtunnel",
        interval_seconds,
        connection_limit = delta.connection_limit,
        global_connection_limit = delta.global_connection_limit,
        duplicate_id = delta.duplicate_id,
        dial_timeout = delta.dial_timeout,
        accept_transient = delta.accept_transient,
        write_stall = delta.write_stall,
        idle = delta.idle,
        lifetime = delta.lifetime,
        "reverse-tunnel rejection counts"
    );
}

/// `(worktree, sandbox_port)` → its supervisor task (self-healing pump loop).
type Tasks = Arc<Mutex<HashMap<(String, u16), JoinHandle<()>>>>;

/// Latches the "no resident bridge → tunnels disabled" warning to one emission
/// (`start_all` runs on every worktree switch; the operator only needs telling once).
static BRIDGE_MISSING_WARNED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

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

pub(crate) struct TunnelRunLimits {
    max_connections: usize,
    global_permits: Arc<Semaphore>,
    deadline_secs: TunnelDeadlineSecs,
}

fn host_deadlines(deadlines: TunnelDeadlineSecs) -> TunnelDeadlines {
    TunnelDeadlines {
        write_stall: Duration::from_secs(deadlines.write_stall),
        idle: deadlines.idle.map(Duration::from_secs),
        lifetime: deadlines.lifetime.map(Duration::from_secs),
    }
}

fn bridge_argv(bridge_path: String, port: u16, deadlines: &TunnelDeadlineSecs) -> Vec<String> {
    let defaults = thegn_core::config::ForwardConfig::default().tunnel_deadlines();
    let mut argv = vec![
        bridge_path,
        "bridge-revtunnel".to_string(),
        port.to_string(),
    ];
    if deadlines.write_stall != defaults.write_stall {
        argv.extend([
            "--write-stall-secs".to_string(),
            deadlines.write_stall.to_string(),
        ]);
    }
    if deadlines.idle != defaults.idle {
        argv.extend([
            "--idle-secs".to_string(),
            deadlines.idle.unwrap_or(0).to_string(),
        ]);
    }
    if deadlines.lifetime != defaults.lifetime {
        argv.extend([
            "--lifetime-secs".to_string(),
            deadlines.lifetime.unwrap_or(0).to_string(),
        ]);
    }
    argv
}

fn tunnel_exec_argv(
    bridge: &crate::bridge_sup::ResidentBridge,
    port: u16,
    deadlines: &TunnelDeadlineSecs,
) -> Vec<String> {
    let argv = bridge_argv(bridge.path().to_string(), port, deadlines);
    crate::bridge_sup::verified_exec_argv(bridge, &argv[1..])
}

#[derive(Clone, Default)]
pub struct ReverseTunnelSupervisor {
    tasks: Tasks,
    global_permits: Arc<OnceLock<Arc<Semaphore>>>,
}

impl ReverseTunnelSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    fn global_permits(&self, total: usize) -> Arc<Semaphore> {
        self.global_permits
            .get_or_init(|| Arc::new(Semaphore::new(clamp_permits(total))))
            .clone()
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
    /// sandbox (via the verified resident bridge) and forward every
    /// connection to the host `host_target` (`host:port`, e.g. the host nix
    /// cache). Idempotent per `(worktree, sandbox_port)`. `handle` is the
    /// tokio runtime to spawn the pump on (callers run inside `spawn_blocking`,
    /// which has no ambient runtime).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        handle: &Handle,
        worktree: &str,
        provider: Provider,
        sandbox_id: String,
        bridge: crate::bridge_sup::ResidentBridge,
        sandbox_port: u16,
        host_target: String,
        limits: TunnelRunLimits,
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
                argv: tunnel_exec_argv(&bridge, sandbox_port, &limits.deadline_secs),
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
                        if let Err(e) = run_host_with_limits(
                            stream,
                            TcpDialer {
                                addr: host_target.clone(),
                            },
                            HostLimits {
                                max_connections: limits.max_connections,
                                global_permits: limits.global_permits.clone(),
                                deadlines: host_deadlines(limits.deadline_secs),
                            },
                        )
                        .await
                        {
                            thegn_core::msg::warn(&format!(
                                "reverse tunnel ended: {e} (reconnecting)"
                            ));
                        }
                    }
                    Err(e) => {
                        thegn_core::msg::warn(&format!(
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

    /// Arm every `(sandbox_port, host_target)` reverse tunnel for a provider
    /// `worktree`, resolving the exec-API provider once.
    ///
    /// Unlike [`crate::agent::native_bridge_provider`], this deliberately ignores
    /// the provider's *transient* native-exec health cooldown: that gate is right
    /// for a one-shot control-plane call, but a reverse tunnel is a persistent
    /// supervisor that already reconnects with bounded backoff. Gating it on the
    /// instantaneous health at worktree-switch would leave the port unbound (e.g.
    /// the nix-cache `:8484` "could not connect") until the next switch. Only the
    /// durable gates apply: `exec != cli` and the provider has a native exec API.
    pub fn start_all(
        &self,
        handle: &Handle,
        cfg: &thegn_core::config::Config,
        env: &thegn_core::env::Environment,
        sandbox_id: &str,
        bridge: &crate::bridge_sup::ResidentBridge,
        worktree: &str,
        tunnels: Vec<(u16, String)>,
    ) {
        if tunnels.is_empty() {
            return;
        }
        // Reverse tunnels ride the verified resident musl bridge installed by
        // `agent::ensure_remote_bridge`. With no local bridge binary configured
        // (`THEGN_BRIDGE_BINARY` / a `thegn-musl` next to the exe) that push is skipped,
        // so `bridge-revtunnel` would exec a missing binary and the port never binds —
        // surfacing in-sandbox as `:8484 could not connect`. Warn once and skip rather
        // than spawn dead reconnect loops.
        if crate::bridge_sup::bridge_binary_path().is_none() {
            if !BRIDGE_MISSING_WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                thegn_core::msg::warn(
                    "reverse tunnels (nix cache :8484) disabled: no resident bridge binary — \
                     build it (`just bridge` / `nix build .#thegn-musl`) and set \
                     THEGN_BRIDGE_BINARY",
                );
            }
            return;
        }
        // Only the durable gates apply (exec != cli, provider has a native exec
        // API); `Provider` isn't `Clone`, so build a fresh one per tunnel.
        let pc = match cfg.env.get(&env.name) {
            Some(e)
                if e.provider.exec != thegn_core::config::ProviderExecMode::Cli
                    && thegn_svc::provider::exec_api_by_name(&e.provider.provider) =>
            {
                &e.provider
            }
            _ => return,
        };
        let max_connections = cfg.forward.reverse_max_connections;
        let deadline_secs = cfg.forward.tunnel_deadlines();
        for (sandbox_port, host_target) in tunnels {
            let Some(provider) = crate::provider_factory::provider_for(pc) else {
                return;
            };
            let global_permits = self.global_permits(cfg.forward.reverse_max_connections_total);
            self.start(
                handle,
                worktree,
                provider,
                sandbox_id.to_string(),
                bridge.clone(),
                sandbox_port,
                host_target,
                TunnelRunLimits {
                    max_connections,
                    global_permits: global_permits.clone(),
                    deadline_secs,
                },
            );
        }
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
    fn tunnel_exec_argv_runs_through_the_verify_wrapper() {
        let bridge = crate::bridge_sup::resident_bridge_for(b"tunnel bridge");
        let deadlines = thegn_core::config::ForwardConfig::default().tunnel_deadlines();
        let argv = tunnel_exec_argv(&bridge, 8383, &deadlines);
        assert_eq!(&argv[..2], &["/bin/sh", "-c"]);
        assert_eq!(
            &argv[3..6],
            &["thegn-bridge", bridge.path(), bridge.sha256()]
        );
        assert_eq!(&argv[6..], &["bridge-revtunnel", "8383"]);
    }

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
    fn bridge_argv_only_adds_nondefault_deadline_flags() {
        let defaults = thegn_core::config::ForwardConfig::default().tunnel_deadlines();
        assert_eq!(
            bridge_argv("/bridge".to_string(), 8383, &defaults),
            vec![
                "/bridge".to_string(),
                "bridge-revtunnel".to_string(),
                "8383".to_string(),
            ]
        );

        let cases = [
            (
                TunnelDeadlineSecs {
                    write_stall: 5,
                    ..defaults
                },
                vec![
                    "/bridge",
                    "bridge-revtunnel",
                    "8383",
                    "--write-stall-secs",
                    "5",
                ],
            ),
            (
                TunnelDeadlineSecs {
                    idle: None,
                    ..defaults
                },
                vec!["/bridge", "bridge-revtunnel", "8383", "--idle-secs", "0"],
            ),
            (
                TunnelDeadlineSecs {
                    lifetime: None,
                    ..defaults
                },
                vec![
                    "/bridge",
                    "bridge-revtunnel",
                    "8383",
                    "--lifetime-secs",
                    "0",
                ],
            ),
            (
                TunnelDeadlineSecs {
                    write_stall: 5,
                    idle: None,
                    lifetime: Some(9),
                },
                vec![
                    "/bridge",
                    "bridge-revtunnel",
                    "8383",
                    "--write-stall-secs",
                    "5",
                    "--idle-secs",
                    "0",
                    "--lifetime-secs",
                    "9",
                ],
            ),
        ];
        for (deadlines, expected) in cases {
            assert_eq!(
                bridge_argv("/bridge".to_string(), 8383, &deadlines),
                expected.into_iter().map(String::from).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn host_deadline_conversion_preserves_config_zero_rules() {
        assert_eq!(
            host_deadlines(TunnelDeadlineSecs {
                write_stall: 60,
                idle: None,
                lifetime: Some(9),
            }),
            TunnelDeadlines {
                write_stall: Duration::from_secs(60),
                idle: None,
                lifetime: Some(Duration::from_secs(9)),
            }
        );
    }

    #[test]
    fn global_permits_are_latched_and_shared_by_supervisor_clones() {
        let supervisor = ReverseTunnelSupervisor::new();
        let first = supervisor.global_permits(3);
        assert_eq!(first.available_permits(), 3);

        let second = supervisor.global_permits(7);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.available_permits(), 3);

        let cloned = supervisor.clone();
        let from_clone = cloned.global_permits(11);
        assert!(Arc::ptr_eq(&first, &from_clone));
        assert_eq!(from_clone.available_permits(), 3);
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
