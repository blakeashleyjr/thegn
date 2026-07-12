//! Startup-time DB touch-ups extracted from `run.rs` (pinned by the file-size
//! ratchet): the default-terminal reseed and the newer-schema-DB status note.
//! Both are best-effort and log rather than swallow, so a failing read is
//! diagnosable instead of looking like "no data".

use thegn_core::db::Db;
use thegn_core::store::WorkspaceStore;

/// Install the per-pane service configs on the registry — `[replay]`
/// recording and the `[daemon]` control-plane route — in one call so the
/// startup and live-config-reload paths in `run.rs` can't drift apart.
pub(crate) fn install_pane_services(
    panes: &mut crate::panes::Panes,
    cfg: &thegn_core::config::Config,
) {
    panes.set_replay_config(cfg.replay.clone());
    panes.set_daemon_config(cfg.daemon.clone());
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
