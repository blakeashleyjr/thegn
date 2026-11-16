//! Decide whether startup should probe container runtimes for orphan cleanup.
//!
//! A configuration layer counts as producing containers only when it is
//! enabled AND its effective backend is not `none` (alias `host`, or
//! `THEGN_SANDBOX_BACKEND=none`). Under an effective `backend = none` startup
//! never probes a container runtime: no orphan GC, no events subscriber, and no
//! passive OCI refresh demand unless the active pane is observed on an OCI
//! backend. Probing would otherwise run `podman version`,
//! which under a fresh `XDG_RUNTIME_DIR` leaves a rootless `pause` process
//! behind (THE-870). Orphans left by an earlier OCI configuration are still
//! removed by the on-demand `thegn sandbox gc`.

use std::path::Path;
use thegn_core::config::{Config, SandboxBackend};

/// Whether an enabled layer with this backend can produce a container.
fn layer_produces_containers(enabled: bool, backend: SandboxBackend) -> bool {
    enabled && backend != SandboxBackend::None
}

/// Whether the effective global/profile/env configuration can produce a
/// thegn container. Repo-root overlays are checked against DB worktree roots
/// by the startup worker because they can enable sandboxing independently.
/// An env overlay's `enabled` and `backend` each default to the base value;
/// any env overlay that is thereby enabled with a backend other than `none`
/// keeps the startup sweep wanted, even if no repo currently selects that env.
/// An effective `backend = none` never wants the sweep.
pub(crate) fn startup_gc_wanted(cfg: &Config) -> bool {
    let mut enabled = cfg.sandbox.enabled;
    let mut backend = cfg.sandbox.backend;
    if let Some(profile) = cfg.active_profile() {
        if let Some(value) = profile.sandbox.enabled {
            enabled = value;
        }
        if let Some(value) = profile.sandbox.backend {
            backend = value;
        }
    }
    layer_produces_containers(enabled, backend)
        || cfg.env.values().any(|env| {
            layer_produces_containers(
                env.sandbox.enabled.unwrap_or(enabled),
                env.sandbox.backend.unwrap_or(backend),
            )
        })
}

/// Start a startup sandbox subscriber only when the effective configuration
/// can produce sandbox containers.
pub(crate) fn with_startup_sandbox_events(cfg: &Config, start: impl FnOnce()) {
    if startup_gc_wanted(cfg) {
        start();
    }
}

/// Whether a repo overlay enables sandboxing with a container-producing
/// backend (not `none`) for any DB worktree row.
pub(crate) fn repo_overlay_wants_startup_gc<'a>(
    cfg: &Config,
    repo_roots: impl IntoIterator<Item = &'a Path>,
) -> bool {
    repo_roots.into_iter().any(|root| {
        let sandbox = cfg.repo_sandbox(root);
        layer_produces_containers(sandbox.enabled, sandbox.backend)
    })
}

/// Whether any global/profile/env layer enables sandboxing, whatever its backend.
fn sandbox_enabled_in_any_layer(cfg: &Config) -> bool {
    let enabled = cfg
        .active_profile()
        .and_then(|profile| profile.sandbox.enabled)
        .unwrap_or(cfg.sandbox.enabled);
    enabled
        || cfg
            .env
            .values()
            .any(|env| env.sandbox.enabled == Some(true))
}

/// Whether a persisted OCI backend should make container refresh demand live.
/// The DB observation can outlive the configuration that launched its pane, so
/// a disabled sandbox suppresses a stale observation. A global `backend = none`
/// does not, because a per-worktree override can still run an OCI pane.
pub(crate) fn active_oci_wanted(cfg: &Config, observed_backend: &str) -> bool {
    sandbox_enabled_in_any_layer(cfg)
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
    if !startup_gc_wanted(cfg) && !active_oci_wanted(cfg, observed_backend) {
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
        assert!(startup_gc_wanted(&parsed(
            "profile = \"p\"\n[sandbox]\nenabled = false\n[profiles.p.sandbox]\nenabled = true\n"
        )));
        // An effective `backend = none` (or its `host` alias) never produces
        // containers, so startup must not probe a runtime (THE-870).
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = true\nbackend = \"none\"\n"
        )));
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = true\nbackend = \"host\"\n"
        )));
        assert!(startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = true\nbackend = \"podman\"\n"
        )));
        assert!(!startup_gc_wanted(&parsed(
            "profile = \"p\"\n[sandbox]\nenabled = true\n[profiles.p.sandbox]\nbackend = \"none\"\n"
        )));
        assert!(startup_gc_wanted(&parsed(
            "profile = \"p\"\n[sandbox]\nbackend = \"none\"\n[profiles.p.sandbox]\nbackend = \"podman\"\n"
        )));
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nenabled = false\n[env.x.sandbox]\nenabled = true\nbackend = \"none\"\n"
        )));
        // An env without its own backend inherits the base `none`.
        assert!(!startup_gc_wanted(&parsed(
            "[sandbox]\nbackend = \"none\"\n[env.x.sandbox]\nenabled = true\n"
        )));
        assert!(startup_gc_wanted(&parsed(
            "[sandbox]\nbackend = \"none\"\n[env.x.sandbox]\nenabled = true\nbackend = \"podman\"\n"
        )));
        assert!(startup_gc_wanted(&parsed(
            "[sandbox]\nbackend = \"none\"\n[env.x.sandbox]\nbackend = \"podman\"\n"
        )));
    }

    struct OneVar(&'static str, &'static str);

    impl thegn_core::config::EnvSource for OneVar {
        fn get(&self, key: &str) -> Option<String> {
            (key == self.0).then(|| self.1.to_owned())
        }
    }

    #[test]
    fn sandbox_backend_env_none_disables_startup_gc() {
        let overlay = thegn_core::config::env_overlay(&OneVar("THEGN_SANDBOX_BACKEND", "none"));
        assert_eq!(
            overlay.sandbox.backend,
            Some(thegn_core::config::SandboxBackend::None)
        );
        let mut cfg = parsed("");
        assert!(startup_gc_wanted(&cfg));
        if let Some(backend) = overlay.sandbox.backend {
            cfg.sandbox.backend = backend;
        }
        assert!(!startup_gc_wanted(&cfg));
        assert!(!super::active_oci_wanted(&cfg, ""));
    }

    #[test]
    fn startup_events_are_gated_when_active_profile_disables_global_sandbox() {
        let cfg = parsed(
            "profile = \"p\"\n[sandbox]\nenabled = true\n[profiles.p.sandbox]\nenabled = false\n",
        );
        let started = std::cell::Cell::new(false);

        super::with_startup_sandbox_events(&cfg, || started.set(true));

        assert!(!started.get());
    }

    #[test]
    fn stale_oci_observation_does_not_enable_container_demand_when_sandbox_is_off() {
        let cfg = parsed("[sandbox]\nenabled = false\n");
        let profile_disabled = parsed("profile = \"p\"\n[profiles.p.sandbox]\nenabled = false\n");

        assert!(!super::active_oci_wanted(&cfg, "podman"));
        assert!(!super::active_oci_wanted(&profile_disabled, "podman"));
        assert!(super::active_oci_wanted(&parsed(""), "podman"));
        let backend_none = parsed("[sandbox]\nbackend = \"none\"\n");
        assert!(super::active_oci_wanted(&backend_none, "podman"));
        assert_eq!(
            super::container_demand(&backend_none, true, false, false, false, "podman"),
            crate::hydrate::ContainerDemand::Summary
        );
        assert_eq!(
            super::container_demand(&backend_none, true, false, false, false, ""),
            crate::hydrate::ContainerDemand::None
        );
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

    #[test]
    fn repo_overlay_with_backend_none_does_not_want_startup_gc() {
        let repo = tempfile::tempdir().expect("temporary repo directory");
        std::fs::write(
            repo.path().join(".thegn.toml"),
            "[sandbox]\nenabled = true\nbackend = \"none\"\n",
        )
        .expect("repo overlay writes");
        let cfg = parsed("[sandbox]\nenabled = false\nbackend = \"none\"\n");

        assert!(!super::repo_overlay_wants_startup_gc(&cfg, [repo.path()]));
    }
}
