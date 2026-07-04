//! `[env.<name>.ssh]` / `[env.<name>.k8s]` / `[env.<name>.provider]` — the
//! per-placement connection sub-tables of an `[env.<name>]` entry — plus the
//! `[metrics]` table. Extracted from `config.rs` (pinned by the file-size
//! ratchet); re-exported from `crate::config` so external paths are unchanged.

use serde::{Deserialize, Serialize};

use crate::config::{RemoteTransport, config_enum, config_warn};

config_enum! {
    /// How a `provider`-placement env runs its interactive pane.
    /// - `auto` — native API exec when the provider supports it (its `exec_api`
    ///   capability), else the vendor CLI. The default: a provider that has a
    ///   native exec API (e.g. Sprites) is CLI-free out of the box.
    /// - `api`  — force native API exec; surface an error rather than silently
    ///   falling back to the CLI when the provider can't do it.
    /// - `cli`  — always wrap the vendor CLI (`interactive_command`).
    pub enum ProviderExecMode: "provider exec mode" {
        Auto = "auto", Api = "api", Cli = "cli",
    } default = Auto;
}

config_enum! {
    /// Which Nix installer the provisioner runs in a fresh sandbox:
    /// - `official` — the upstream `nixos.org/nix/install --no-daemon`
    ///   (single-user; the safe default + fallback).
    /// - `determinate` — Determinate Systems' faster Rust installer, with the
    ///   official installer as an automatic fallback.
    pub enum NixInstaller: "nix installer" {
        Official = "official" | "nodaemon", Determinate = "determinate" | "ds",
    } default = Official;
}

config_enum! {
    /// `[env.<name>.provider] connect` — how a provider sandbox's interactive
    /// pane attaches.
    ///
    /// - `exec` — the provider's native WSS PTY exec (the default; superzej's
    ///            vt100-over-WebSocket relay).
    /// - `ssh`  — run `sshd` inside the sandbox and attach the pane as a LOCAL
    ///            `ssh` client tunneled over the provider's TCP-over-WebSocket
    ///            proxy. Delegates PTY/resize/flow-control to ssh (no hand-rolled
    ///            relay) and unlocks scp/sshfs/agent-forwarding. (Sprites expose
    ///            no UDP, so mosh is not possible — use a real `placement = "ssh"`
    ///            env for that.)
    pub enum ProviderConnect: "provider connect" {
        Exec = "exec", Ssh = "ssh",
    } default = Exec;
}

/// `[env.<name>.ssh]` — connection settings for an `ssh`/`mosh` placement. An
/// empty `host` falls back to the worktree's own remote target (its `GitLoc`)
/// or the global `[sandbox.remote] host`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct EnvSshConfig {
    pub host: String,
    pub port: u16,
    pub transport: RemoteTransport,
    pub forward_agent: bool,
    /// `ssh -F <path>` — a dedicated ssh config file for this environment.
    pub ssh_config: String,
    /// `ssh -J <host>` — a ProxyJump bastion.
    pub jump_host: String,
    /// `ssh -i <path>` — an explicit identity file.
    pub identity: String,
    /// Extra raw ssh args appended verbatim.
    pub extra_args: Vec<String>,
}

impl Default for EnvSshConfig {
    fn default() -> Self {
        EnvSshConfig {
            host: String::new(),
            port: 22,
            transport: RemoteTransport::Mosh,
            forward_agent: true,
            ssh_config: String::new(),
            jump_host: String::new(),
            identity: String::new(),
            extra_args: Vec::new(),
        }
    }
}

impl EnvSshConfig {
    pub(crate) fn is_default(&self) -> bool {
        self.host.is_empty()
            && self.ssh_config.is_empty()
            && self.jump_host.is_empty()
            && self.identity.is_empty()
            && self.extra_args.is_empty()
            && self.port == 22
            && self.forward_agent
            && self.transport == RemoteTransport::Mosh
    }
}

/// `[env.<name>.k8s]` — a Kubernetes pod target for a `k8s` placement.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct EnvK8sConfig {
    /// kubectl binary (empty ⇒ `kubectl`).
    pub kubectl: String,
    /// `--context` (empty ⇒ current kubeconfig context).
    pub context: String,
    /// `--namespace` (empty ⇒ default namespace).
    pub namespace: String,
    /// Target pod name or selector (e.g. `dev-blake`). Required to exec; the
    /// pod-template spawn lifecycle resolves it when `pod_template` is set.
    pub pod: String,
    /// `-c <container>` within the pod (empty ⇒ default container).
    pub container: String,
    /// Path to a pod manifest applied to spawn a custom env (lifecycle phase).
    pub pod_template: String,
    /// Image for a template-less spawned pod.
    pub image: String,
    /// Enable remote-access features (e.g. `kubectl port-forward`).
    pub remote_access: bool,
}

impl EnvK8sConfig {
    pub(crate) fn is_default(&self) -> bool {
        self.kubectl.is_empty()
            && self.context.is_empty()
            && self.namespace.is_empty()
            && self.pod.is_empty()
            && self.container.is_empty()
            && self.pod_template.is_empty()
            && self.image.is_empty()
            && !self.remote_access
    }
}

/// `[env.<name>.provider]` — a managed-sandbox provider for a `provider`
/// placement (Daytona, Codespaces, …). `exec_command` is a static argv template
/// (`{id}` is substituted with `id`) that runs a command in the sandbox; the
/// API-driven discover/create lifecycle is layered on in the provider phase.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct EnvProviderConfig {
    /// Provider id, e.g. `"daytona"`.
    pub provider: String,
    /// Opaque sandbox/environment id (static config; or resolved by lifecycle).
    pub id: String,
    /// Argv prefix to run a command in the sandbox; `{id}` is substituted.
    /// e.g. `["daytona", "ssh", "{id}", "--"]`.
    pub exec_command: Vec<String>,
    /// Optional PTY-capable prefix for the interactive pane (defaults to
    /// `exec_command` when empty).
    pub interactive_command: Vec<String>,
    /// Optional argv to create/start the sandbox (`superzej env up`); `{id}` is
    /// substituted. e.g. `["daytona", "create", "--id", "{id}"]`. Empty ⇒ the
    /// sandbox is assumed pre-created.
    pub up_command: Vec<String>,
    /// Optional argv to destroy/stop the sandbox (`superzej env down`).
    pub down_command: Vec<String>,
    /// API base URL for an API-driven provider (provider phase).
    pub api_base: String,
    /// Env var holding the provider API token.
    pub api_key_env: String,
    /// How the interactive pane attaches: native API exec (PTY-over-WebSocket,
    /// no vendor CLI) vs the `interactive_command` CLI bridge. `auto` (default)
    /// prefers native when the provider supports it.
    pub exec: ProviderExecMode,
    /// How the interactive pane connects: the native WSS PTY `exec` (default) or
    /// a local `ssh` client tunneled over the provider's TCP-over-WebSocket proxy.
    /// See [`ProviderConnect`].
    pub connect: ProviderConnect,
    /// Provider sandbox template/image to create from.
    pub template: String,
    /// Working directory inside the sandbox that a `data = "sync"` projection
    /// pushes the local worktree into (and pulls back from). Empty ⇒ `/workspace`.
    pub workdir: String,
    /// Auto-create the sandbox on first open if it doesn't exist yet (API
    /// providers): the "feels-local" warm-on-open. Off by default — creating a
    /// paid cloud sandbox should be opt-in; otherwise run `superzej env provision`.
    pub auto_provision: bool,
    /// Checkpoint the sandbox on worktree close (API providers that support it):
    /// "suspend on close" for fast resume. Off by default (each checkpoint may
    /// incur storage cost).
    pub auto_checkpoint: bool,
    /// Which Nix installer the provisioner uses in a fresh sandbox (speedup).
    pub nix_installer: NixInstaller,
    /// Parallelize Nix downloads in the sandbox: sets `http-connections` +
    /// `max-substitution-jobs` to this value (clamped 1..=256). `0` ⇒ leave Nix's
    /// defaults (the biggest cheap win on the download-bound devShell build).
    pub nix_parallel_downloads: u32,
    /// Extra Nix binary-cache substituter URL so the repo's devShell is a
    /// download (not a build) in the sandbox. Empty ⇒ disabled.
    pub binary_cache_url: String,
    /// Public key trusting `binary_cache_url` (required to use it).
    pub binary_cache_key: String,
    /// Push the built devShell closure to `binary_cache_url` during provisioning
    /// (needs a signing key in the env), so later sandboxes download it. Default off.
    pub binary_cache_push: bool,
    /// P2P devShell speedup (opt-in, default off): transfer the repo's devShell
    /// closure — already built on the HOST (you run `nix develop`/direnv locally) —
    /// straight into the sandbox store (host `nix copy --to file://` → fs upload →
    /// sandbox `nix copy --from file://`), so the in-sandbox devShell is a local
    /// store hit, not a rebuild/redownload. No hosted cache needed; the host is the
    /// cache. No-op when the repo has no nix devShell or the host hasn't built it.
    pub push_devshell: bool,
    /// Skip the blocking devShell build during provisioning (opt-in, default off).
    /// The repo's devShell then builds lazily in-pane on first `direnv`/`nix
    /// develop` instead of gating the loading screen on a multi-minute toolchain
    /// build. Use when you want the shell to come up immediately and don't need a
    /// prebuilt (checkpointed) devShell.
    pub skip_devshell_warm: bool,
    /// Serve the HOST `/nix/store` as a live HTTP binary cache reachable from the
    /// sandbox over the reverse tunnel (opt-in, default off), so an in-sandbox `nix
    /// develop`/`direnv` SUBSTITUTES prebuilt store paths from the host instead of
    /// building from source. A general substituter covering the whole host store —
    /// strictly more capable than `push_devshell`'s one-shot devShell upload, which
    /// it supersedes when set. Needs a provider with the resident bridge (sprites).
    pub host_cache: bool,
    /// VPS providers only: vendor region/location (e.g. Hetzner `fsn1`).
    /// Empty ⇒ the provider's default.
    pub region: String,
    /// VPS providers only: vendor size/plan/server-type (e.g. Hetzner `cx22`).
    /// Empty ⇒ the provider's default.
    pub size: String,
    /// VPS providers only: hard cap on concurrently-managed instances — the
    /// spend guardrail enforced at create. `0` ⇒ the built-in default (5).
    pub max_instances: u32,
    /// VPS providers only: ceiling on any instance's lifetime in seconds; the
    /// reaper destroys older ones (a VPS bills until destroyed — there is no
    /// free suspended state). `0` ⇒ no ceiling.
    pub max_lifetime_secs: u64,
}

impl EnvProviderConfig {
    pub(crate) fn is_default(&self) -> bool {
        self.provider.is_empty()
            && self.id.is_empty()
            && self.exec_command.is_empty()
            && self.interactive_command.is_empty()
            && self.up_command.is_empty()
            && self.down_command.is_empty()
            && self.api_base.is_empty()
            && self.api_key_env.is_empty()
            && self.exec == ProviderExecMode::Auto
            && self.template.is_empty()
            && self.workdir.is_empty()
            && !self.auto_provision
            && !self.auto_checkpoint
            && self.nix_installer == NixInstaller::Official
            && self.nix_parallel_downloads == 0
            && self.binary_cache_url.is_empty()
            && self.binary_cache_key.is_empty()
            && !self.binary_cache_push
            && !self.push_devshell
            && !self.skip_devshell_warm
            && !self.host_cache
            && self.region.is_empty()
            && self.size.is_empty()
            && self.max_instances == 0
            && self.max_lifetime_secs == 0
    }

    /// `http-connections`/`max-substitution-jobs` value to use, clamped to a sane
    /// range; `None` when unset (leave Nix's defaults).
    pub fn nix_parallel(&self) -> Option<u32> {
        (self.nix_parallel_downloads > 0).then(|| self.nix_parallel_downloads.clamp(1, 256))
    }

    /// The sandbox working dir for `sync` (config value or the `/workspace` default).
    pub fn sync_workdir(&self) -> String {
        let w = self.workdir.trim();
        if w.is_empty() {
            "/workspace".to_string()
        } else {
            w.to_string()
        }
    }

    /// The control-exec argv template (`{id}` unexpanded): the configured
    /// `exec_command`, or — for VPS providers, which have no vendor CLI — the
    /// szhost self-bridge (`szhost vps-ssh <name> --`), so panes, chrome
    /// git/fs reads, and the persisted worktree location all route over the
    /// same ssh transport with zero per-env config. The single source of truth
    /// for both `envbuild` (placement prefixes) and the warm-pool claim rebind.
    pub fn control_command_template(&self) -> Vec<String> {
        if !self.exec_command.is_empty() {
            return self.exec_command.clone();
        }
        if vps_provider_kind(&self.provider) {
            let exe = std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "szhost".to_string());
            return vec![exe, "vps-ssh".into(), "{id}".into(), "--".into()];
        }
        Vec::new()
    }
}

/// Whether `name` names a commodity-VPS provider kind (Hetzner today;
/// DigitalOcean/Vultr as their adapters land). The core-side mirror of
/// `superzej_svc::vps::is_vps_provider` — keep the two lists in sync.
pub fn vps_provider_kind(name: &str) -> bool {
    matches!(name.trim(), "hetzner")
}

/// `[metrics]` — Prometheus scrape targets for sidebar metrics display.
/// Each target is scraped directly via HTTP; no Prometheus server required.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MetricsConfig {
    /// Scrape interval in seconds.
    #[serde(alias = "interval-secs")]
    pub interval_secs: f64,
    /// Request timeout in milliseconds.
    #[serde(alias = "timeout-ms")]
    pub timeout_ms: u64,
    /// Max response body size in bytes (prevent runaway).
    #[serde(alias = "max-body-bytes")]
    pub max_body_bytes: usize,
    /// Scrape targets.
    pub targets: Vec<MetricsTarget>,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        MetricsConfig {
            interval_secs: 5.0,
            timeout_ms: 500,
            max_body_bytes: 1_048_576, // 1 MiB
            targets: Vec::new(),
        }
    }
}

/// One Prometheus scrape target.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MetricsTarget {
    /// Display name in the sidebar.
    pub name: String,
    /// URL to scrape (e.g., `http://localhost:9091/metrics`).
    pub url: String,
    /// Metrics to display (allowlist). Empty = all.
    #[serde(default)]
    pub metrics: Vec<String>,
    /// Optional labels to match (e.g., `instance="localhost:9091"`).
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

/// `[lifecycle.pool]` — an optional pool of pre-provisioned, unclaimed sandboxes
/// per (repo, env) so a brand-new worktree opens instantly. `size = 0` disables
/// it (the default); enabling it requires the DB worktree→sandbox mapping.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PoolConfig {
    /// Pre-provisioned spare sandboxes to keep ready per (repo, env). `0` = off.
    pub size: usize,
    /// Destroy an unclaimed pool member older than this (seconds).
    pub max_idle_secs: u64,
    /// Recycle checkpointed spares by restoring them IN PLACE (seconds) instead
    /// of destroy+rebuild (minutes) when they go stale or their worktree is
    /// deleted — guarded by lockfile freshness, with destroy as the fallback.
    /// The kill-switch: `false` restores the always-destroy behavior.
    pub recycle: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            size: 0,
            max_idle_secs: 600,
            recycle: true,
        }
    }
}
