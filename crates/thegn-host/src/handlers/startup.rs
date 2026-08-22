//! Startup-time DB touch-ups extracted from `run.rs` (pinned by the file-size
//! ratchet): the default-terminal reseed and the newer-schema-DB status note.
//! Both are best-effort and log rather than swallow, so a failing read is
//! diagnosable instead of looking like "no data".

use thegn_core::db::Db;
use thegn_core::store::WorkspaceStore;

/// Whether center-tab panes route through the pane daemon (surviving UI detach)
/// for this process. The single source of truth for that decision, shared by
/// [`install_pane_services`] (which transport the registry uses) and the
/// launch-spec builders (which drop the bwrap `--die-with-parent` guard on
/// daemon-persistent panes) so the two can't drift.
///
/// Harness kill-switch: the first-frame benchmark and the e2e snapshot suite
/// kill the compositor without a quit path — their panes would detach into
/// never-reaped daemon sessions (one leaked daemon + shell per iteration/case),
/// and the racy "persist" chip would flake the snapshots. Those harnesses opt
/// out via env, forcing plain in-process panes.
///
/// Also off when the control-socket path cannot fit `sun_path` — see
/// [`socket_path_fits`], which explains why that degrades instead of failing.
///
/// DESTRUCTIVE toward persisted daemon sessions: ANY launch with the route
/// disabled — `[daemon] enabled = false`, `THEGN_NO_DAEMON=1`, or
/// `THEGN_BENCH_FIRST_FRAME_EXIT` — claims each persisted daemon-backed pane
/// record at materialize and best-effort KILLS its daemon session (see
/// `handlers::provision::drain_specs`). The alternative was worse: the pane
/// respawned in-process while the daemon copy kept running forever under the
/// untimed default lease, invisible after the next persist pruned the record.
/// But it means a one-off `THEGN_NO_DAEMON=1` debugging run against a real
/// state dir stops the user's persisted daemon sessions — the harnesses only
/// stay side-effect-free because they isolate `XDG_STATE_HOME` (no daemon,
/// nothing persisted, so the connect-only kill is a no-op).
///
/// The over-long-socket case is side-effect-free for the same reason: a path
/// that can never be bound can never have carried a live daemon, so there is no
/// session for the drain to connect to and kill.
pub(crate) fn daemon_active(cfg: &thegn_core::config::Config) -> bool {
    cfg.daemon.enabled
        && std::env::var_os("THEGN_BENCH_FIRST_FRAME_EXIT").is_none()
        && std::env::var_os("THEGN_NO_DAEMON").is_none()
        && socket_path_fits(cfg)
}

/// Whether the resolved control socket fits the platform's `sun_path` bound.
///
/// A path over the limit can never be bound, and the failure is otherwise
/// invisible AND misleading: the daemon is spawned through `util::detached`
/// (all three stdio streams nulled), so its `bind` error goes to `/dev/null`,
/// and `ensure_daemon` reports the 3s health-poll timeout instead — which reads
/// like a slow machine. Every pane would then become an error husk, one 3s
/// stall at a time. Degrading to in-process panes keeps thegn fully usable and
/// costs only detach/reattach persistence, so decide it here, once, alongside
/// the other opt-outs.
///
/// Cheap enough for the per-pane callers: one env read and two path joins, no
/// I/O — and deliberately NOT cached, so a live `[daemon] socket` edit is
/// re-evaluated on reload.
fn socket_path_fits(cfg: &thegn_core::config::Config) -> bool {
    use thegn_core::config_daemon::{check_socket_path_len, max_socket_path_len};

    let sock = crate::daemon::socket_path(&cfg.daemon);
    let max = max_socket_path_len(cfg!(target_os = "linux"));
    let Err(too_long) = check_socket_path_len(&sock, max, cfg!(windows)) else {
        return true;
    };
    // Once per process: `install_pane_services` re-runs on every live config
    // reload, and the per-pane callers hit this on each spawn.
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        thegn_core::msg::warn(&format!(
            "pane daemon disabled — control socket path is {} bytes, over this \
             platform's {}-byte limit:\n  {}\nPanes will run in-process (no \
             detach/reattach).\nFix: set [daemon] socket to a shorter path.",
            too_long.len,
            too_long.max,
            sock.display()
        ));
    });
    false
}

/// Install the per-pane service configs on the registry — `[replay]`
/// recording and the `[daemon]` control-plane route — in one call so the
/// startup and live-config-reload paths in `run.rs` can't drift apart.
pub(crate) fn install_pane_services(
    panes: &mut crate::panes::Panes,
    cfg: &thegn_core::config::Config,
) {
    panes.set_replay_config(cfg.replay.clone());
    let mut daemon = cfg.daemon.clone();
    daemon.enabled = daemon_active(cfg);
    panes.set_daemon_config(daemon);
    set_aggregate_cpu_cap(cfg);
}

/// Establish the aggregate CPU ceiling for all worktree panes: set the shared
/// [`thegn_core::sandbox_cpucap::CPU_SLICE`] quota once, off-loop. Panes join it
/// in `sandbox::enter_argv`; this sets its bound. Best-effort and idempotent —
/// runs once per process (a `Once` guard, so the live-config-reload path can't
/// re-spawn it), and an older/missing systemd or no cgroup `cpu` delegation just
/// means the cap silently doesn't bite (surfaced by `thegn doctor`).
fn set_aggregate_cpu_cap(cfg: &thegn_core::config::Config) {
    use thegn_core::sandbox_cpucap as sandbox;
    static ONCE: std::sync::Once = std::sync::Once::new();
    // Only touch systemd when a real cgroup hard cap is available.
    if sandbox::detect_cpu_cap() != sandbox::CpuCap::ScopeHard {
        return;
    }
    let ncpu = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let raw = cfg.sandbox.limits.cpu_total.as_deref().unwrap_or("auto");
    let Some(quota) = sandbox::resolve_cpu_total(raw, ncpu) else {
        return;
    };
    ONCE.call_once(move || {
        tokio::task::spawn_blocking(move || {
            // off-loop: blocking child wait runs on the spawn_blocking pool.
            #[expect(clippy::disallowed_methods)]
            let status = std::process::Command::new("systemctl")
                .args([
                    "--user",
                    "set-property",
                    "--runtime",
                    sandbox::CPU_SLICE,
                    &format!("CPUQuota={quota}"),
                ])
                .status();
            match status {
                Ok(s) if s.success() => tracing::info!(
                    target: "thegn::startup", slice = sandbox::CPU_SLICE, %quota,
                    "aggregate CPU cap set"
                ),
                Ok(s) => tracing::warn!(
                    target: "thegn::startup", code = ?s.code(),
                    "systemctl set-property for aggregate CPU cap failed"
                ),
                Err(e) => tracing::warn!(
                    target: "thegn::startup", error = %e,
                    "systemctl set-property for aggregate CPU cap failed"
                ),
            }
        });
    });
}

/// Ensure a default `local` terminal exists so the sidebar's TERMINALS section
/// always has a live entry. Seeding only on an empty table keeps it a one-time
/// default the user can rename or delete; a deliberately-emptied list is
/// reseeded on the next launch ("there is always a local terminal"). On a read
/// error we log and still attempt the reseed rather than silently skipping it —
/// a swallowed error is exactly how the section stayed blank.
pub(crate) fn reseed_default_terminal(db: Option<&Db>) {
    let Some(db) = db else { return };
    let empty = match db.terminals() {
        Ok(t) => t.is_empty(),
        Err(e) => {
            tracing::warn!(target: "thegn::db", error = %e, "reseed: terminals() read failed; attempting seed anyway");
            true
        }
    };
    if empty {
        // best-effort: the DB is a cache; a failed seed just means the sidebar
        // shows its empty-state hint until the next successful launch.
        let _ = db.put_terminal("local", "local", "", None);
    }
}

/// A one-line status note when the on-disk DB was written by a newer-schema
/// build (a different branch sharing this file). `None` when schemas match.
pub(crate) fn schema_mismatch_status(db: Option<&Db>) -> Option<String> {
    let newer = db?.schema_mismatch()?;
    Some(format!(
        "⚠ database schema v{newer} is newer than this build (v{}); some data may be hidden",
        thegn_core::db::SCHEMA_VERSION
    ))
}
