//! Startup-time DB touch-ups extracted from `run.rs` (pinned by the file-size
//! ratchet): the default-terminal reseed and the newer-schema-DB status note.
//! Both are best-effort and log rather than swallow, so a failing read is
//! diagnosable instead of looking like "no data".

use superzej_core::db::Db;
use superzej_core::store::WorkspaceStore;

/// Install the per-pane service configs on the registry — `[replay]`
/// recording and the `[daemon]` control-plane route — in one call so the
/// startup and live-config-reload paths in `run.rs` can't drift apart.
pub(crate) fn install_pane_services(
    panes: &mut crate::panes::Panes,
    cfg: &superzej_core::config::Config,
) {
    panes.set_replay_config(cfg.replay.clone());
    panes.set_daemon_config(cfg.daemon.clone());
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
            tracing::warn!(target: "szhost::db", error = %e, "reseed: terminals() read failed; attempting seed anyway");
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
        superzej_core::db::SCHEMA_VERSION
    ))
}
