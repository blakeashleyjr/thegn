//! Decide whether startup should probe container runtimes for orphan cleanup.

use std::path::Path;
use thegn_core::config::Config;

/// Whether the effective global/profile/env configuration can produce a
/// thegn container. Repo-root overlays are checked against DB worktree roots
/// by the startup worker because they can enable sandboxing independently.
pub(crate) fn startup_gc_wanted(cfg: &Config) -> bool {
    let mut base = cfg.sandbox.enabled;
    if let Some(profile) = cfg.active_profile()
        && let Some(enabled) = profile.sandbox.enabled
    {
        base = enabled;
    }
    base || cfg
        .env
        .values()
        .any(|env| env.sandbox.enabled == Some(true))
}

/// Whether a repo overlay enables sandboxing for any DB worktree row.
pub(crate) fn repo_overlay_wants_startup_gc<'a>(
    cfg: &Config,
    repo_roots: impl IntoIterator<Item = &'a Path>,
) -> bool {
    repo_roots
        .into_iter()
        .any(|root| cfg.repo_sandbox(root).enabled)
}

/// Whether a persisted OCI backend should make container refresh demand live.
/// The DB observation can outlive the configuration that launched its pane.
pub(crate) fn active_oci_wanted(cfg: &Config, observed_backend: &str) -> bool {
    startup_gc_wanted(cfg)
        && thegn_core::sandbox::Backend::parse(observed_backend)
            .is_some_and(|backend| backend.is_oci())
}

/// Resolve refresh demand from the effective base config. With the base config
/// off, keep explicitly visible surfaces because a repo overlay can enable
/// their containers; suppress passive refresh from the panel or other tabs.
pub(crate) fn container_demand(
    cfg: &Config,
    panel_open: bool,
    monitor_open: bool,
    monitor_containers_visible: bool,
    sandbox_section_open: bool,
    observed_backend: &str,
) -> crate::hydrate::ContainerDemand {
    if !startup_gc_wanted(cfg) {
        return if monitor_containers_visible || sandbox_section_open {
            crate::hydrate::ContainerDemand::Detail
        } else {
            crate::hydrate::ContainerDemand::None
        };
    }
    crate::hydrate::ContainerDemand::resolve(
        panel_open,
        monitor_open,
        monitor_containers_visible,
        sandbox_section_open,
        active_oci_wanted(cfg, observed_backend),
    )
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
        assert!(startup_gc_wanted(&parsed("")));
        assert!(!startup_gc_wanted(&parsed("[sandbox]\nenabled = false\n")));
        assert!(startup_gc_wanted(&parsed("[sandbox]\nenabled = true\n")));
        assert!(startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[env.x.sandbox]\nenabled = true\n"
        )));
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[env.x.sandbox]\nenabled = false\n"
        )));
        // An inactive profile never applies.
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[profiles.x.sandbox]\nenabled = true\n"
        )));
        assert!(!startup_gc_wanted(&parsed(
            "profile = \"p\"\n[profiles.p.sandbox]\nenabled = false\n"
        )));
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[profiles.q.sandbox]\nenabled = true\n"
        )));
        assert!(startup_gc_wanted(&parsed(
            "profile = \"p\"\n[sandbox]\nenabled = false\n[profiles.p.sandbox]\nenabled = true\n"
        )));
    }

    #[test]
    fn stale_oci_observation_does_not_enable_container_demand_when_sandbox_is_off() {
        let cfg = parsed("[sandbox]\nenabled = false\n");
        let profile_disabled = parsed("profile = \"p\"\n[profiles.p.sandbox]\nenabled = false\n");

        assert!(!super::active_oci_wanted(&cfg, "podman"));
        assert!(!super::active_oci_wanted(&profile_disabled, "podman"));
        assert!(super::active_oci_wanted(&parsed(""), "podman"));
        assert_eq!(
            super::container_demand(&cfg, true, false, false, false, "podman"),
            crate::hydrate::ContainerDemand::None
        );
        assert_eq!(
            super::container_demand(&parsed(""), true, false, false, false, "podman"),
            crate::hydrate::ContainerDemand::Summary
        );
        assert_eq!(
            super::container_demand(&parsed(""), true, false, false, false, ""),
            crate::hydrate::ContainerDemand::None
        );
    }

    #[test]
    fn sandbox_off_keeps_only_explicit_container_surface_demand() {
        let cfg = parsed("[sandbox]\nenabled = false\n");

        assert_eq!(
            super::container_demand(&cfg, true, false, false, false, "podman"),
            crate::hydrate::ContainerDemand::None
        );
        assert_eq!(
            super::container_demand(&cfg, false, true, false, false, ""),
            crate::hydrate::ContainerDemand::None
        );
        assert_eq!(
            super::container_demand(&cfg, false, false, false, true, ""),
            crate::hydrate::ContainerDemand::Detail
        );
        assert_eq!(
            super::container_demand(&cfg, false, true, true, false, ""),
            crate::hydrate::ContainerDemand::Detail
        );
    }

    #[test]
    fn repo_overlay_keeps_startup_gc_enabled_when_global_sandbox_is_off() {
        let repo = tempfile::tempdir().expect("temporary repo directory");
        std::fs::write(
            repo.path().join(".thegn.toml"),
            "[sandbox]\nenabled = true\n",
        )
        .expect("repo overlay writes");
        let cfg = parsed("[sandbox]\nenabled = false\n");

        assert!(super::repo_overlay_wants_startup_gc(&cfg, [repo.path()]));
    }
}
