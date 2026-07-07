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
    /// Hibernation (snapshot-then-destroy on idle) for this env's claimed
    /// sandboxes: `auto` (default — on for commodity VPS, off for
    /// scale-to-zero providers), `on`, or `off`. See `[lifecycle]`
    /// `hibernate_after_secs` and `[lifecycle.snapshot]`.
    pub hibernate: HibernateMode,
    /// Per-env idle-seconds override before hibernation. `0` ⇒ the global
    /// `[lifecycle] hibernate_after_secs`.
    pub hibernate_idle_secs: u64,
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
            && self.hibernate == HibernateMode::Auto
            && self.hibernate_idle_secs == 0
    }

    /// Whether this env's provider is scale-to-zero (an idle sandbox self-suspends
    /// for free — see [`provider_scale_to_zero`]). Drives the warm-pool idle policy:
    /// scale-to-zero spares are parked indefinitely, others are aged out to reclaim
    /// spend.
    pub fn scale_to_zero(&self) -> bool {
        provider_scale_to_zero(&self.provider)
    }

    /// Whether hibernation (snapshot-then-destroy on idle) applies to this
    /// env's provider: the explicit `hibernate = on|off` wins; `auto` resolves
    /// on for commodity VPS (bills while it exists) and off for scale-to-zero
    /// providers (idle compute already ~free).
    pub fn hibernate_enabled(&self) -> bool {
        match self.hibernate {
            HibernateMode::On => true,
            HibernateMode::Off => false,
            HibernateMode::Auto => vps_provider_kind(&self.provider),
        }
    }

    /// The idle TTL before hibernation for this env: the per-env override, or
    /// `global` (`[lifecycle] hibernate_after_secs`) when unset. `0` ⇒ off.
    pub fn hibernate_idle(&self, global: u64) -> u64 {
        if self.hibernate_idle_secs > 0 {
            self.hibernate_idle_secs
        } else {
            global
        }
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
        if wss_native_provider_kind(&self.provider) {
            // WSS-native providers (sprites) have no vendor CLI. The
            // control-plane READ path (chrome git/gh/fs reads + the persisted
            // worktree `GitLoc`) shells out through the szhost self-bridge,
            // which runs the command over the provider's native exec API. Panes
            // attach over that API directly (see `native_exec_for`), so this
            // prefix drives only the reads — `envbuild` keeps the *interactive*
            // prefix empty for these providers. Without it the location blob had
            // an empty prefix, `GitLoc::from_db` fell back to Local, and every
            // read ran `git -C /workspace` on the HOST (no such dir) → blank
            // branch / ahead-behind / a stale panel.
            let exe = std::env::current_exe()
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "szhost".to_string());
            return vec![exe, "sprite-exec".into(), "{id}".into(), "--".into()];
        }
        Vec::new()
    }
}

/// Whether `name` names a commodity-VPS provider kind (Hetzner + DigitalOcean;
/// Vultr as its adapter lands). The core-side mirror of
/// `superzej_svc::vps::is_vps_provider` — keep the two lists in sync.
pub fn vps_provider_kind(name: &str) -> bool {
    matches!(name.trim(), "hetzner" | "digitalocean")
}

/// Whether `name` names a provider whose control plane runs over the szhost
/// exec self-bridge (`szhost sprite-exec <id> --`) rather than a vendor CLI or
/// ssh — the WSS-native exec providers. The core-side mirror of
/// `superzej_svc::provider::exec_api_by_name` — keep the two lists in sync.
pub fn wss_native_provider_kind(name: &str) -> bool {
    matches!(name.trim(), "sprites")
}

/// Whether a provider *kind* is **scale-to-zero**: an idle sandbox self-suspends
/// for effectively free (compute billed only while awake; the filesystem
/// persists). This is the single source of truth the warm-pool policy consults
/// (`superzej_svc::provider::ProviderCaps::scale_to_zero` mirrors it by kind).
///
/// `sprites` (Fly's scale-to-zero Firecracker microVMs: a ~30s idle timeout,
/// zero idle compute charge) and `fly` (a stopped Fly machine bills only for its
/// rootfs, and start/stop is fast) qualify. Everything else — VPS (a powered-off
/// instance still bills), Daytona (no confirmed free idle), unknown kinds — is
/// **false** on purpose: a wrong `false` merely keeps the safe age-out behavior,
/// while a wrong `true` would park billed instances forever.
pub fn provider_scale_to_zero(name: &str) -> bool {
    matches!(name.trim(), "sprites" | "fly")
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

config_enum! {
    /// How far ahead of focus to eagerly provision provider sandboxes (so the
    /// minutes-long one-time provisioning happens in the background, not on the
    /// hot path). Non-provider worktrees are always a no-op.
    pub enum EagerScope: "eager scope" {
        Off = "off" | "none",
        ActiveWorktreePlusNew = "active" | "active_plus_new" | "focus",
        ActiveWorkspace = "workspace",
        All = "all",
    } default = ActiveWorktreePlusNew;
}

config_enum! {
    /// Whether an env's claimed sandbox may HIBERNATE when idle: snapshot the
    /// worktree's git/file state to the durable snapshot store, destroy the
    /// compute, and transparently recreate + restore on next open.
    /// - `auto` — on for commodity-VPS providers (they bill while the instance
    ///   exists, even stopped), off for scale-to-zero providers (idle compute
    ///   is already ~free there).
    /// - `on` / `off` — force it either way.
    pub enum HibernateMode: "hibernate mode" {
        Auto = "auto", On = "on" | "true", Off = "off" | "false",
    } default = Auto;
}

config_enum! {
    /// Where worktree hibernation snapshots are stored.
    /// - `local` — a directory on this host (`$XDG_STATE_HOME/superzej/snapshots`
    ///   unless `dir` overrides it). Zero config, no credentials.
    /// - `s3` — an S3-compatible bucket (`bucket`/`endpoint`/`region`/`prefix`
    ///   plus `access_key`/`secret_key` secret refs).
    pub enum SnapshotBackend: "snapshot backend" {
        Local = "local" | "fs", S3 = "s3",
    } default = Local;
}

/// `[lifecycle]` — budget-governed warm/suspend policy for managed-provider
/// sandboxes. The defaults are budget-safe: superzej's background sidebar/activity
/// polling never wakes a suspended sandbox, and idle sandboxes suspend after the
/// TTL while the few most-recently-used (and any busy/pane-held) stay warm.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LifecycleConfig {
    /// Master switch. When off, the policy is inert (no suspend, no gating) —
    /// today's behavior. On by default (the default settings only *reduce* cost).
    pub enabled: bool,
    /// Max managed-provider sandboxes kept warm at once (bounds discretionary
    /// keeps; active/busy/pane-held worktrees are always kept).
    pub max_warm: usize,
    /// Idle seconds before a non-essential warm sandbox may suspend.
    pub idle_ttl_secs: u64,
    /// How far ahead of focus to eagerly provision (hide the provisioning cost).
    pub eager: EagerScope,
    /// Always keep the active worktree's sandbox warm.
    pub keep_active_warm: bool,
    /// Keep a busy worktree (live in-sandbox process) warm past the idle TTL.
    pub keep_busy_warm: bool,
    /// Serve cached git glyphs/activity for non-warm provider worktrees instead of
    /// running an in-sandbox query that would wake them (the core budget fix).
    pub serve_cached_glyphs: bool,
    /// Optional spend guardrail: est. $/warm-sandbox-hour for the ceiling math.
    pub cost_per_warm_hour: f64,
    /// Trim the warm set so estimated warm spend stays under this $/hour. `0` ⇒ off.
    pub cost_ceiling_per_hour: f64,
    /// Optional warm pool of pre-provisioned spares (`[lifecycle.pool]`).
    pub pool: PoolConfig,
    /// Idle seconds before an eligible CLAIMED sandbox hibernates: its git/file
    /// state is snapshotted to the durable store, the compute is destroyed, and
    /// the next open transparently recreates + restores it. Applies to envs
    /// whose `hibernate` mode resolves on (default: commodity VPS). `0` ⇒ off
    /// globally. Deliberately much longer than `idle_ttl_secs`: suspend is
    /// instant to undo, hibernate costs a re-provision on next open.
    pub hibernate_after_secs: u64,
    /// `[lifecycle.snapshot]` — where hibernation snapshots live.
    pub snapshot: SnapshotStoreConfig,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_warm: 2,
            idle_ttl_secs: 300,
            eager: EagerScope::ActiveWorktreePlusNew,
            keep_active_warm: true,
            keep_busy_warm: true,
            serve_cached_glyphs: true,
            cost_per_warm_hour: 0.0,
            cost_ceiling_per_hour: 0.0,
            pool: PoolConfig::default(),
            hibernate_after_secs: 3600,
            snapshot: SnapshotStoreConfig::default(),
        }
    }
}

/// `[lifecycle.snapshot]` — the durable store for worktree hibernation
/// snapshots (git bundle + uncommitted patch + untracked tar per worktree).
/// Only what those artifacts carry survives a hibernate; everything else on
/// the VM (build caches, containers, ad-hoc data) is ephemeral by design.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SnapshotStoreConfig {
    /// `local` (default, this host's disk) or `s3`.
    pub backend: SnapshotBackend,
    /// Local backend: root directory override. Empty ⇒
    /// `$XDG_STATE_HOME/superzej/snapshots`.
    pub dir: String,
    /// Snapshots retained per (repo, worktree, env); older ones are pruned
    /// after each capture. Clamped to ≥ 1.
    pub keep: usize,
    /// Abort a hibernate (keep the VM alive, warn) if any single artifact
    /// exceeds this many MiB — the guard against tarring a huge untracked
    /// dataset through memory. `0` ⇒ no ceiling.
    pub max_artifact_mb: u64,
    /// S3 backend: bucket name (required for `backend = "s3"`).
    pub bucket: String,
    /// S3 backend: endpoint URL for S3-compatible stores (R2, B2, MinIO…).
    /// Empty ⇒ AWS S3.
    pub endpoint: String,
    /// S3 backend: region.
    pub region: String,
    /// Key prefix inside the bucket.
    pub prefix: String,
    /// Secret ref for the access key id (`env:VAR`, `keyring:<name>`,
    /// `file:/path`, or a bare env-var name).
    pub access_key: String,
    /// Secret ref for the secret access key (same forms as `access_key`).
    pub secret_key: String,
}

impl Default for SnapshotStoreConfig {
    fn default() -> Self {
        Self {
            backend: SnapshotBackend::Local,
            dir: String::new(),
            keep: 3,
            max_artifact_mb: 512,
            bucket: String::new(),
            endpoint: String::new(),
            region: "us-east-1".into(),
            prefix: "superzej".into(),
            access_key: "env:AWS_ACCESS_KEY_ID".into(),
            secret_key: "env:AWS_SECRET_ACCESS_KEY".into(),
        }
    }
}

impl SnapshotStoreConfig {
    /// Retention floor: keeping zero snapshots would make every hibernate
    /// destroy the only copy of the work it just captured.
    pub fn keep_clamped(&self) -> usize {
        self.keep.max(1)
    }

    /// The per-artifact byte ceiling, `None` when uncapped.
    pub fn max_artifact_bytes(&self) -> Option<u64> {
        (self.max_artifact_mb > 0).then(|| self.max_artifact_mb * 1024 * 1024)
    }
}

/// `[lifecycle.pool]` — an optional pool of pre-provisioned, unclaimed sandboxes
/// per (repo, env) so a brand-new worktree opens instantly. `size = 0` leaves it
/// to the auto default (one parked spare for a scale-to-zero provider the user
/// already opted into via `auto_provision`; off otherwise — see
/// `effective_pool_target`); a non-zero `size` sets it explicitly.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PoolConfig {
    /// Pre-provisioned spare sandboxes to keep ready per (repo, env). `0` = the
    /// provider-aware auto default; a non-zero value overrides it (clamped to a
    /// runaway ceiling). The `+`/`-` hotkey persists a per-repo+env override that
    /// wins over both (including `0` to force the pool off).
    pub size: usize,
    /// AgeOut (billed-when-stopped, e.g. VPS) providers only: destroy an unclaimed
    /// pool member idle longer than this (seconds). Ignored for scale-to-zero
    /// providers, whose idle spares self-suspend for free and are parked, not
    /// aged out (they still rotate on flake.lock drift).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_env(name: &str) -> EnvProviderConfig {
        EnvProviderConfig {
            provider: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn hibernate_auto_resolves_on_for_vps_off_for_scale_to_zero() {
        // The whole point: VPS bills while it exists; sprites/fly idle ~free.
        assert!(provider_env("hetzner").hibernate_enabled());
        assert!(provider_env("digitalocean").hibernate_enabled());
        assert!(!provider_env("sprites").hibernate_enabled());
        assert!(!provider_env("fly").hibernate_enabled());
        // Unknown/no-files providers stay off under auto.
        assert!(!provider_env("daytona").hibernate_enabled());
        assert!(!provider_env("").hibernate_enabled());
    }

    #[test]
    fn hibernate_explicit_mode_wins_over_auto() {
        let mut e = provider_env("sprites");
        e.hibernate = HibernateMode::On;
        assert!(e.hibernate_enabled());
        let mut e = provider_env("hetzner");
        e.hibernate = HibernateMode::Off;
        assert!(!e.hibernate_enabled());
    }

    #[test]
    fn hibernate_idle_prefers_the_per_env_override() {
        let mut e = provider_env("hetzner");
        assert_eq!(e.hibernate_idle(3600), 3600);
        e.hibernate_idle_secs = 120;
        assert_eq!(e.hibernate_idle(3600), 120);
        // Both zero ⇒ off.
        e.hibernate_idle_secs = 0;
        assert_eq!(e.hibernate_idle(0), 0);
    }

    #[test]
    fn hibernate_fields_participate_in_is_default() {
        let mut e = EnvProviderConfig::default();
        assert!(e.is_default());
        e.hibernate = HibernateMode::Off;
        assert!(!e.is_default());
        let e = EnvProviderConfig {
            hibernate_idle_secs: 5,
            ..Default::default()
        };
        assert!(!e.is_default());
    }

    #[test]
    fn snapshot_store_defaults_are_local_and_clamped() {
        let s = SnapshotStoreConfig::default();
        assert_eq!(s.backend, SnapshotBackend::Local);
        assert_eq!(s.keep, 3);
        assert_eq!(s.keep_clamped(), 3);
        assert_eq!(s.max_artifact_bytes(), Some(512 * 1024 * 1024));
        assert_eq!(s.access_key, "env:AWS_ACCESS_KEY_ID");
        let zero = SnapshotStoreConfig {
            keep: 0,
            max_artifact_mb: 0,
            ..Default::default()
        };
        // keep=0 would prune the only copy of just-captured work; clamp to 1.
        assert_eq!(zero.keep_clamped(), 1);
        assert_eq!(zero.max_artifact_bytes(), None);
    }

    #[test]
    fn hibernate_and_backend_enums_parse_their_aliases() {
        assert_eq!(
            HibernateMode::from_str_validated("true").unwrap(),
            HibernateMode::On
        );
        assert_eq!(
            HibernateMode::from_str_validated("off").unwrap(),
            HibernateMode::Off
        );
        assert!(HibernateMode::from_str_validated("sometimes").is_err());
        assert_eq!(
            SnapshotBackend::from_str_validated("fs").unwrap(),
            SnapshotBackend::Local
        );
        assert_eq!(
            SnapshotBackend::from_str_validated("s3").unwrap(),
            SnapshotBackend::S3
        );
    }

    #[test]
    fn lifecycle_defaults_include_hibernation() {
        let l = LifecycleConfig::default();
        assert_eq!(l.hibernate_after_secs, 3600);
        assert_eq!(l.snapshot.backend, SnapshotBackend::Local);
    }
}
