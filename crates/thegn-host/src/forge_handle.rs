//! The host's forge handle: one [`ForgeSet`] for the process, built from the
//! loaded config and read from any blocking thread.
//!
//! A process-global rather than a field on the model: `FrameModel` is plain
//! `Clone + Default` data swapped wholesale by hydration, and the forge is
//! reached from CLI subcommands, `sched::spawn_bg` closures and
//! `spawn_blocking` tasks alike — the same reasons `sched::bg_sem()` and the
//! native layer's circuit breaker are globals. `get()` never panics: without
//! an `install`, it builds from `Config::default()` (a GitHub ladder), so
//! tests and one-shot verbs work unconfigured.

use std::sync::{Arc, OnceLock};
use thegn_core::config::Config;
use thegn_svc::forge::ForgeSet;

static FORGES: OnceLock<Arc<ForgeSet>> = OnceLock::new();

/// Install the process's forge set from config. Idempotent: a second call is
/// a no-op (the first config wins; `[[forges]]` is not hot-reloadable).
pub fn install(cfg: &Config) {
    // best-effort: a second install is a no-op by design (first config wins).
    let _ = FORGES.set(Arc::new(ForgeSet::from_config(cfg)));
}

/// The process's forge set.
pub fn get() -> Arc<ForgeSet> {
    FORGES.get_or_init(|| Arc::new(ForgeSet::default())).clone()
}

#[cfg(test)]
mod tests {
    #[test]
    fn get_without_install_is_the_github_default() {
        let f = super::get();
        assert_eq!(f.default_forge().id(), "github");
    }
}
