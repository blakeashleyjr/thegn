//! Where this process's config came from, so a long-lived process can re-read
//! it per request.
//!
//! The daemon used to snapshot `Config` at start and serve every `sessions.open`
//! from that snapshot: a new `[[agents]]` entry (or a changed `model`/`env`)
//! was invisible until the daemon — and every pane it owns — was restarted.
//! `main` records the CLI's config source here once; the daemon's agent-launch
//! path then loads a fresh layered config for each request (off the runtime
//! threads — it is file + DB I/O), falling back to its snapshot if the file no
//! longer parses.

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
    let _ = SOURCE.set(Source { overrides, path });
}

/// A freshly loaded config from the recorded source, layered exactly as `main`
/// layers it (env + overrides + DB hosts + channel clamp). `None` when nothing
/// was recorded or the file no longer loads — the caller keeps its snapshot.
/// Blocking I/O: never call on the event loop or a runtime worker.
pub fn fresh() -> Option<Config> {
    let src = SOURCE.get()?;
    let mut cfg = Config::try_load_layered(
        &thegn_core::config::ProcessEnv,
        &src.overrides,
        src.path.clone(),
    )
    .ok()?;
    thegn_core::host_config::merge_db_hosts(&mut cfg);
    // best-effort: the clamped-feature report is for `main`'s startup status
    // note; a daemon re-load deliberately discards it.
    let _ = cfg.clamp_to_channel(crate::channel_state::current());
    Some(cfg)
}

#[cfg(test)]
mod tests {
    #[test]
    fn fresh_without_a_source_is_none() {
        // The test binary never installs a source; a process that did not
        // record one keeps its snapshot rather than guessing a path.
        if super::SOURCE.get().is_none() {
            assert!(super::fresh().is_none());
        }
    }
}
