//! Where this process's config came from, so a long-lived process can re-read
//! it per request.
//!
//! The daemon used to snapshot `Config` at start and serve every `sessions.open`
//! from that snapshot: a new `[[agents]]` entry (or a changed `model`/`env`)
//! was invisible until the daemon — and every pane it owns — was restarted.
//! `main` records the CLI's config source here once; the daemon's agent-launch
//! path then loads a fresh layered config for each request (off the runtime
//! threads — it is file + DB I/O) and copies only the agent/tool/pipeline
//! registries over the boot snapshot. Everything else remains boot-fixed.

use std::path::PathBuf;
use std::sync::OnceLock;

use thegn_core::config::Config;

struct Source {
    overrides: Vec<String>,
    path: Option<PathBuf>,
}

static SOURCE: OnceLock<Source> = OnceLock::new();

/// Record the CLI's `--set` overrides and `--config` path. First call wins;
/// later calls are ignored (the process has one config source).
pub fn install(overrides: Vec<String>, path: Option<PathBuf>) {
    // best-effort: first call wins by contract; a second install is a no-op
    // (same pattern as the issue-token keyring resolver's `get_or_init`).
    let _ = SOURCE.set(Source { overrides, path }); // best-effort: first-set-wins: later calls are ignored by design
}

/// `boot` with only its agent/tool/pipeline registries refreshed from a
/// freshly **admitted** generation.
///
/// When this process performed startup admission (every configured verb,
/// including the daemon, does), the refresh re-admits from the frozen process
/// capture through `config_startup::reload` and uses only a generation that
/// is current and healthy. A failed or unauthorizable reload is an `Err`: an
/// agent launch must refuse rather than fall back to a stale boot snapshot
/// (which stays available for display/status only).
///
/// `Ok(None)` only for a process with neither an admitted store nor a legacy
/// source (unit tests), where the caller keeps its snapshot.
/// Blocking I/O: never call on the event loop or a runtime worker.
pub fn fresh(boot: &Config) -> Result<Option<Config>, String> {
    if crate::config_startup::store().is_some() {
        use crate::config_startup::ReloadOutcome;
        let admitted = match crate::config_startup::reload() {
            ReloadOutcome::Published(admitted) => admitted,
            // Another reload published first: use whatever is current now.
            ReloadOutcome::Superseded | ReloadOutcome::Unavailable => {
                crate::config_startup::store()
                    .ok_or("configuration store is unavailable")?
                    .current()
                    .map_err(|error| error.to_string())?
            }
            ReloadOutcome::Failed(error) => return Err(error.to_string()),
            ReloadOutcome::FailedCoalesced => {
                return Err(
                    "configuration reload failed; the last admitted generation is display-only"
                        .into(),
                );
            }
        };
        let mut cfg = admitted.config().clone();
        // best-effort: the clamped-feature report is for `main`'s startup
        // status note; a daemon re-load deliberately discards it.
        let _ = cfg.clamp_to_channel(crate::channel_state::current());
        return Ok(Some(thegn_core::pipeline_run::with_fresh_registry(
            boot, &cfg,
        )));
    }
    let Some(src) = SOURCE.get() else {
        return Ok(None);
    };
    // Legacy tolerant path: reachable only in a process that never ran
    // startup admission (no production verb), kept for existing tests.
    let Ok(mut cfg) = Config::try_load_layered(
        &thegn_core::config::ProcessEnv,
        &src.overrides,
        src.path.clone(),
    ) else {
        return Ok(None);
    };
    thegn_core::host_config::merge_db_hosts(&mut cfg);
    // best-effort: see above.
    let _ = cfg.clamp_to_channel(crate::channel_state::current());
    Ok(Some(thegn_core::pipeline_run::with_fresh_registry(
        boot, &cfg,
    )))
}

#[cfg(test)]
mod tests {
    #[test]
    fn fresh_without_a_source_is_none() {
        // The test binary never installs a source; a process that did not
        // record one keeps its snapshot rather than guessing a path.
        if super::SOURCE.get().is_none() && crate::config_startup::store().is_none() {
            assert!(matches!(
                super::fresh(&thegn_core::config::Config::default()),
                Ok(None)
            ));
        }
    }
}
