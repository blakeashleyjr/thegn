//! `[sandbox.limits]` config — CPU/memory ceilings for worktree panes.
//!
//! Extracted from the oversized `config.rs` (kept flat). Re-exported
//! from `config` so the public path stays `crate::config::SandboxLimits`.

/// Per-pane and aggregate resource ceilings. All optional strings so junk config
/// degrades to "no cap" rather than a hard error; the values are handed to the
/// backend (OCI `--cpus`/`--memory`, or a systemd `CPUQuota`/`MemoryMax`).
#[derive(
    Debug, Clone, Default, serde::Deserialize, PartialEq, Eq, serde::Serialize, schemars::JsonSchema,
)]
#[serde(default)]
pub struct SandboxLimits {
    /// Per-pane CPU ceiling, in cores (fractional OK: `"0.5"`, `"2"`). Honored on
    /// every backend: OCI `--cpus`, and a `CPUQuota` on bwrap/systemd/none.
    pub cpu: Option<String>,
    /// Per-pane memory ceiling (`"512m"`, `"4g"`).
    pub memory: Option<String>,
    /// Aggregate CPU ceiling for *all* thegn worktree panes combined, in cores,
    /// enforced via a shared user slice on host-toolchain backends. `None` (the
    /// default) means `"auto"` — leave 2 cores free so the machine stays
    /// responsive; `"off"`/`""` disables it; a number is an explicit core count.
    pub cpu_total: Option<String>,
}

// `impl SandboxOverlay` — extracted from the pinned-oversized `config.rs`
// (kept flat). The struct + its fields stay in `config`; only the
// merge/emptiness logic lives here. `is_empty` is `pub(crate)` so the
// `#[serde(skip_serializing_if = "SandboxOverlay::is_empty")]` paths in
// `config` still resolve it across modules.
use crate::config::{SandboxConfig, SandboxOverlay};

impl SandboxOverlay {
    pub(crate) fn apply(self, base: &mut SandboxConfig) {
        if let Some(v) = self.enabled {
            base.enabled = v;
        }
        if let Some(v) = self.backend {
            base.backend = v;
        }
        if let Some(v) = self.on_dormant {
            base.on_dormant = v;
        }
        if let Some(v) = self.default_backend {
            base.default_backend = v;
        }
        if let Some(v) = self.default_env {
            base.default_env = v;
        }
        if let Some(v) = self.main_env {
            base.main_env = v;
        }
        if let Some(v) = self.backend_chain {
            base.backend_chain = v;
        }
        if let Some(v) = self.image {
            base.image = v;
        }
        if let Some(v) = self.profile {
            base.profile = v;
        }
        if let Some(v) = self.network {
            base.network = v;
        }
        if let Some(v) = self.file_access {
            base.file_access = v;
        }
        if let Some(v) = self.ports {
            base.ports = v;
        }
        if let Some(v) = self.gpu {
            base.gpu = Some(v);
        }
        if let Some(v) = self.limits {
            base.limits = v;
        }
        if let Some(v) = self.volumes {
            base.volumes = v;
        }
        if let Some(v) = self.compose {
            base.compose = Some(v);
        }
        if let Some(v) = self.env_passthrough {
            base.env_passthrough = v;
        }
        if let Some(v) = self.auto_caches {
            base.auto_caches = v;
        }
        if let Some(v) = self.mounts {
            base.mounts = v;
        }
        if let Some(v) = self.init_script {
            base.init_script = v;
        }
        if let Some(v) = self.prepare {
            base.prepare = v;
        }
        if let Some(v) = self.warm_direnv {
            base.warm_direnv = v;
        }
        if let Some(v) = self.devenv {
            base.devenv = v;
        }
        if let Some(v) = self.inject_devshell {
            base.inject_devshell = v;
        }
        if let Some(v) = self.devshell {
            base.devshell = v;
        }
        if let Some(v) = self.nix_daemon {
            base.nix_daemon = v;
        }
        if let Some(v) = self.shell {
            base.shell = v;
        }
        if let Some(v) = self.on_missing {
            base.on_missing = v;
        }
        if let Some(r) = self.remote {
            r.apply(&mut base.remote);
        }
        if let Some(v) = self.network_allow {
            base.network_allow = v;
        }
        if let Some(v) = self.network_block {
            base.network_block = v;
        }
        if let Some(v) = self.network_audit {
            base.network_audit = v;
        }
        if let Some(v) = self.vpn {
            base.vpn = v;
        }
        if let Some(h) = self.home {
            h.apply(&mut base.home);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        // `skip_serializing_if` for env/profile `[…sandbox]` overlays: EVERY field
        // must be checked or it is silently dropped through the serde round-trip
        // in apply_toml_overlay / apply_override_str. Test:
        // sandbox_overlay_is_empty_covers_every_field.
        self.enabled.is_none()
            && self.backend.is_none()
            && self.default_backend.is_none()
            && self.default_env.is_none()
            && self.main_env.is_none()
            && self.backend_chain.is_none()
            && self.image.is_none()
            && self.profile.is_none()
            && self.network.is_none()
            && self.file_access.is_none()
            && self.ports.is_none()
            && self.gpu.is_none()
            && self.limits.is_none()
            && self.volumes.is_none()
            && self.compose.is_none()
            && self.env_passthrough.is_none()
            && self.auto_caches.is_none()
            && self.mounts.is_none()
            && self.init_script.is_none()
            && self.prepare.is_none()
            && self.warm_direnv.is_none()
            && self.devenv.is_none()
            && self.inject_devshell.is_none()
            && self.devshell.is_none()
            && self.nix_daemon.is_none()
            && self.shell.is_none()
            && self.on_missing.is_none()
            && self.remote.is_none()
            && self.network_allow.is_none()
            && self.network_block.is_none()
            && self.network_audit.is_none()
            && self.vpn.is_none()
            && self.home.as_ref().is_none_or(|h| h.is_empty())
    }
}
