//! `[sandbox.limits]` config — CPU/memory ceilings for worktree panes — and
//! `[sandbox.git]`, the git write policy for sandboxed panes.
//!
//! Extracted from the oversized `config.rs` (kept flat). Re-exported
//! from `config` so the public path stays `crate::config::SandboxLimits`.

pub mod git_policy;

use crate::config::{config_enum, config_warn};

config_enum! {
    /// What a sandboxed pane may write in the git common dir. See
    /// [`git_policy::plan_git_mounts`] for the exact mount plan of each mode.
    pub enum SandboxGitMode : "sandbox git mode" {
        ReadOnly   = "read-only" | "read_only" | "readonly",
        CommitOnly = "commit-only" | "commit_only",
        PushLane   = "push-lane" | "push_lane",
        Full       = "full",
    } default = CommitOnly;
}

config_enum! {
    /// How sandbox commits travel back to the host repository.
    pub enum GitTransport : "sandbox git transport" {
        Bundle = "bundle",
        Clone  = "clone",
    } default = Bundle;
}

config_enum! {
    /// Which git credential a `mode = "full"` sandbox receives. `none` (the
    /// default) withholds the host GitHub token; `repo-token`/`user-token`
    /// pass it through. The others parse so configs can be written ahead.
    pub enum GitCredential : "sandbox git credential" {
        None      = "none" | "off",
        DeployKey = "deploy-key" | "deploy_key",
        RepoToken = "repo-token" | "repo_token",
        GithubApp = "github-app" | "github_app",
        UserToken = "user-token" | "user_token",
    } default = None;
}

fn default_push_refs() -> Vec<String> {
    vec!["refs/heads/tg/*".to_string()]
}

fn default_deny_refs() -> Vec<String> {
    vec!["refs/heads/main".to_string(), "refs/tags/*".to_string()]
}

/// `[sandbox.git]` — the git write policy for sandboxed panes.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(default)]
pub struct SandboxGitConfig {
    /// `read-only`, `commit-only` (default), `push-lane` or `full`.
    pub mode: SandboxGitMode,
    /// `bundle` (default) or `clone`: how commits come back to the host.
    pub transport: GitTransport,
    /// Credential handed to a `full` sandbox. `none` (default) withholds
    /// GH_TOKEN/GITHUB_TOKEN; `repo-token`/`user-token` pass them through.
    pub credential: GitCredential,
    /// Ref globs a `push-lane` sandbox may push (`refs/heads/tg/*`).
    pub push_refs: Vec<String>,
    /// Ref globs never accepted back from a sandbox. The merge target branch
    /// is always denied in addition to this list.
    pub deny_refs: Vec<String>,
    /// Private ssh key file used by `credential = "deploy-key"` (`~` is
    /// expanded). Local placements only: bound read-only into the sandbox and
    /// selected through `GIT_SSH_COMMAND`. Empty (the default) means unset.
    pub deploy_key: String,
}

impl Default for SandboxGitConfig {
    fn default() -> Self {
        SandboxGitConfig {
            mode: SandboxGitMode::default(),
            transport: GitTransport::default(),
            credential: GitCredential::default(),
            push_refs: default_push_refs(),
            deny_refs: default_deny_refs(),
            deploy_key: String::new(),
        }
    }
}

/// All-Option mirror of [`SandboxGitConfig`] for `[env.<n>.sandbox.git]` and
/// `[profiles.<n>.sandbox.git]`. Present keys win; absent keys inherit.
#[derive(
    Debug, Clone, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(default)]
pub struct SandboxGitOverlay {
    pub mode: Option<SandboxGitMode>,
    pub transport: Option<GitTransport>,
    pub credential: Option<GitCredential>,
    pub push_refs: Option<Vec<String>>,
    pub deny_refs: Option<Vec<String>>,
    pub deploy_key: Option<String>,
}

impl SandboxGitOverlay {
    /// True when no key is set (the overlay changes nothing).
    pub fn is_empty(&self) -> bool {
        self.mode.is_none()
            && self.transport.is_none()
            && self.credential.is_none()
            && self.push_refs.is_none()
            && self.deny_refs.is_none()
            && self.deploy_key.is_none()
    }
}

/// Host env vars carrying a forge (GitHub / GitLab) token. Withheld from
/// sandboxes and provider provisioning unless
/// [`SandboxGitConfig::admits_git_token`]. A token stored in a `gh`/`glab`
/// config that arrives via `[sandbox.home]` dotfiles is not filtered.
pub const GIT_TOKEN_ENV: &[&str] = &[
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GITLAB_TOKEN",
    "GL_TOKEN",
];

impl SandboxGitConfig {
    /// Does this policy hand the host GitHub token to the sandbox? Only
    /// `mode = "full"` with `credential = "repo-token" | "user-token"`; the
    /// default (`credential = "none"`) keeps credentials with thegn.
    pub fn admits_git_token(&self) -> bool {
        self.mode == SandboxGitMode::Full
            && matches!(
                self.credential,
                GitCredential::RepoToken | GitCredential::UserToken
            )
    }

    /// May the env var `key` be passed into a sandbox under this policy?
    /// False only for a [`GIT_TOKEN_ENV`] name when the token is withheld.
    pub fn admits_env(&self, key: &str) -> bool {
        self.admits_git_token() || !GIT_TOKEN_ENV.contains(&key)
    }

    /// `self` with every key present in `overlay` replaced (lists replace
    /// wholesale, they do not append).
    #[must_use]
    pub fn merged(&self, overlay: &SandboxGitOverlay) -> SandboxGitConfig {
        SandboxGitConfig {
            mode: overlay.mode.unwrap_or(self.mode),
            transport: overlay.transport.unwrap_or(self.transport),
            credential: overlay.credential.unwrap_or(self.credential),
            push_refs: overlay
                .push_refs
                .clone()
                .unwrap_or_else(|| self.push_refs.clone()),
            deny_refs: overlay
                .deny_refs
                .clone()
                .unwrap_or_else(|| self.deny_refs.clone()),
            deploy_key: overlay
                .deploy_key
                .clone()
                .unwrap_or_else(|| self.deploy_key.clone()),
        }
    }

    /// Does this policy hand the sandbox a deploy key? `mode = "full"` with
    /// `credential = "deploy-key"` and a non-empty [`Self::deploy_key`].
    pub fn wants_deploy_key(&self) -> bool {
        self.mode == SandboxGitMode::Full
            && self.credential == GitCredential::DeployKey
            && !self.deploy_key.trim().is_empty()
    }

    /// [`Self::deploy_key`] with `~` expanded, or `None` when unset.
    pub fn deploy_key_path(&self) -> Option<std::path::PathBuf> {
        let raw = self.deploy_key.trim();
        (!raw.is_empty()).then(|| std::path::PathBuf::from(crate::util::expand_tilde(raw)))
    }
}

/// Per-pane and aggregate resource ceilings. All optional strings so junk config
/// degrades to "no cap" rather than a hard error; the values are handed to the
/// backend (OCI `--cpus`/`--memory`, or a systemd `CPUQuota`/`MemoryMax`).
#[derive(
    Debug, Clone, Default, serde::Deserialize, PartialEq, serde::Serialize, schemars::JsonSchema,
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
    /// Aggregate MEMORY ceiling for all panes combined (`"56g"`), applied to the
    /// same shared slice as [`Self::cpu_total`]. Set as systemd `MemoryHigh`,
    /// not `MemoryMax`: over the line the kernel throttles allocation and
    /// reclaims *inside* the cgroup instead of OOM-killing, which is what a
    /// build workload wants — a slow `cargo build` beats a dead one, and either
    /// beats the whole machine stalling in global direct reclaim. The per-pane
    /// [`Self::memory`] stays a hard `MemoryMax`. `None` (the default) means no
    /// aggregate memory cap; `"off"`/`""` also disable it.
    pub memory_total: Option<String>,
    /// Per-pane memory high-water mark, applied as systemd `MemoryHigh` on pane
    /// scopes. A size (`"20g"`) or a percentage of [`Self::memory`] (`"85%"`).
    /// Over this line the kernel throttles and reclaims inside the pane's cgroup
    /// *before* the hard `MemoryMax` OOM-kill. `None` (the default) means `"85%"`
    /// whenever `memory` is set; `"off"`/`""` disables it.
    pub memory_high: Option<String>,
    /// Per-pane swap ceiling, applied as systemd `MemorySwapMax` on pane scopes
    /// so one pane cannot fill the machine's swap. A size (`"4g"`), `"0"` for no
    /// swap, or `"off"`/`""` to leave swap unbounded. `None` (the default) means
    /// `"4G"` whenever `memory` is set.
    pub memory_swap: Option<String>,
    /// Aggregate swap ceiling for the shared slice (`MemorySwapMax` on
    /// `thegn.slice`). Opt-in: `None`/`"off"`/`""` means no aggregate swap cap.
    pub memory_swap_total: Option<String>,
    /// Maximum number of concurrent host-local heavyweight gates. Zero leaves
    /// gate concurrency unlimited.
    pub heavy_slots: usize,
    /// Absolute one-minute load-average ceiling for heavyweight gates. Zero
    /// disables the load gate.
    #[schemars(range(min = 0))]
    pub max_load: f64,
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
        if let Some(v) = self.devcontainer {
            base.devcontainer = v;
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
        if let Some(v) = self.compiler_cache {
            base.compiler_cache = v;
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
            base.nix_daemon = Some(v);
        }
        if let Some(v) = self.shell {
            base.shell = v;
        }
        if let Some(v) = self.on_missing {
            base.on_missing = v;
        }
        if let Some(v) = self.isolation_floor {
            base.isolation_floor = Some(v);
        }
        if let Some(v) = self.on_floor_miss {
            base.on_floor_miss = v;
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
        if let Some(g) = self.git {
            base.git = base.git.merged(&g);
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
            && self.devcontainer.is_none()
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
            && self.compiler_cache.is_none()
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
            && self.isolation_floor.is_none()
            && self.on_floor_miss.is_none()
            && self.remote.is_none()
            && self.network_allow.is_none()
            && self.network_block.is_none()
            && self.network_audit.is_none()
            && self.vpn.is_none()
            && self.home.as_ref().is_none_or(|h| h.is_empty())
            && self.git.as_ref().is_none_or(SandboxGitOverlay::is_empty)
    }
}
