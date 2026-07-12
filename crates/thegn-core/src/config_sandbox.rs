//! `[sandbox.limits]` config — CPU/memory ceilings for worktree panes.
//!
//! Extracted from the pinned-oversized `config.rs` (file-size ratchet). Re-exported
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
