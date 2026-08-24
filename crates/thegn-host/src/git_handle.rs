//! The host's git read engine: the `[git] backend` selection, built once from
//! config and shared by every read site (sidebar glyph scan, branch lists,
//! remote polls). Writes and plumbing go through `CliGit` explicitly — the
//! git-backend spec's "writes go through the CLI".
//!
//! Same shape and reasons as `forge_handle`: a process-global because reads
//! run from `sched::spawn_bg` closures, `spawn_blocking` tasks and CLI verbs
//! alike, and `get()` never panics (default = the native engine).

use std::sync::{Arc, OnceLock};
use thegn_core::config::Config;
use thegn_svc::git::GitBackend;

static GIT: OnceLock<Arc<dyn GitBackend>> = OnceLock::new();

/// Install the read engine from config (first call wins).
pub fn install(cfg: &Config) {
    // best-effort: a second install is a no-op by design (first config wins).
    let _ = GIT.set(thegn_svc::git::backend_for(cfg.git.backend));
}

/// The read engine.
pub fn get() -> Arc<dyn GitBackend> {
    GIT.get_or_init(|| thegn_svc::git::backend_for(Default::default()))
        .clone()
}

#[cfg(test)]
mod tests {

    #[test]
    fn get_without_install_is_the_native_engine() {
        assert_eq!(super::get().probe().id, "gix");
    }
}
