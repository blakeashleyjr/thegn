//! Decide whether startup should probe container runtimes for orphan cleanup.

use thegn_core::config::Config;

/// Whether any supported configuration layer can produce a thegn container.
///
/// A named env or profile can enable sandboxing even when the global default
/// is disabled, so either kind of explicit `true` keeps startup GC useful.
pub(crate) fn startup_gc_wanted(cfg: &Config) -> bool {
    cfg.sandbox.enabled
        || cfg
            .env
            .values()
            .any(|env| env.sandbox.enabled == Some(true))
        || cfg
            .profiles
            .values()
            .any(|profile| profile.sandbox.enabled == Some(true))
}

#[cfg(test)]
mod tests {
    use super::startup_gc_wanted;
    use thegn_core::config::Config;

    fn parsed(source: &str) -> Config {
        toml::from_str(source).expect("config TOML parses")
    }

    #[test]
    fn startup_gc_tracks_sandbox_configuration_layers() {
        assert!(!startup_gc_wanted(&parsed("[sandbox]\nenabled = false\n")));
        assert!(startup_gc_wanted(&parsed("[sandbox]\nenabled = true\n")));
        assert!(startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[env.x.sandbox]\nenabled = true\n"
        )));
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[env.x.sandbox]\nenabled = false\n"
        )));
        assert!(startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[profiles.x.sandbox]\nenabled = true\n"
        )));
    }
}
