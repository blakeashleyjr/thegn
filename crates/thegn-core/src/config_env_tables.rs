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
    /// Vercel and Sprites; default none. What model credential reaches the
    /// sandbox. See docs/help/vercel-sandbox.md.
    pub enum ModelCredentials: "model credentials" {
        None = "none", Upload = "upload", Proxy = "proxy", Inject = "inject",
    } default = None;
}

config_enum! {
    /// Vercel only; default `deny`. What network egress a sandbox gets:
    /// `deny` blocks everything, `registries` adds a built-in allow-list of
    /// package registries and git hosts, `allow-all` leaves egress open.
    /// `egress_allow` entries are added on top of `deny`/`registries`.
    pub enum SandboxEgress: "sandbox egress" {
        Deny = "deny", Registries = "registries", AllowAll = "allow-all",
    } default = Deny;
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
    /// - `exec` — the provider's native WSS PTY exec (the default; thegn's
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
            // THE-66: agent forwarding OFF by default (see RemoteConfig). A
            // user host that wants `-A` sets `forward_agent = true` explicitly.
            forward_agent: false,
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
            && !self.forward_agent
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
    /// Optional argv to create/start the sandbox (`thegn env up`); `{id}` is
    /// substituted. e.g. `["daytona", "create", "--id", "{id}"]`. Empty ⇒ the
    /// sandbox is assumed pre-created.
    pub up_command: Vec<String>,
    /// Optional argv to destroy/stop the sandbox (`thegn env down`).
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
    /// Interactive-pane transport for a provider reached over ssh (VPS/machine0):
    /// `mosh` (default — resilient roaming/latency, auto-falls-back to ssh when
    /// the VM lacks `mosh-server` or its network blocks mosh UDP) or `ssh`. The
    /// control plane always uses ssh.
    pub transport: RemoteTransport,
    /// Provider sandbox template/image to create from.
    pub template: String,
    /// machine0 + NixOS only: a Nix flake ref applied to the VM after create via
    /// `nixos-rebuild switch --flake <ref>` (over ssh — there is no provider
    /// tool for it). A local `path#attr` is uploaded to the VM first; a flake URL
    /// (`github:owner/repo#host`) is applied verbatim. Empty ⇒ no NixOS apply.
    pub provision_flake: String,
    /// Working directory inside the sandbox that a `data = "sync"` projection
    /// pushes the local worktree into (and pulls back from). Empty ⇒ `/workspace`.
    pub workdir: String,
    /// Auto-create the sandbox on first open if it doesn't exist yet (API
    /// providers): the "feels-local" warm-on-open. Off by default — creating a
    /// paid cloud sandbox should be opt-in; otherwise run `thegn env provision`.
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
    /// Deprecated compatibility key. Host-side devShell evaluation and closure
    /// transfer are disabled; target-side provisioning remains the only path.
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
    /// strictly more capable than the removed one-shot devShell upload. Needs a
    /// provider with the resident bridge (sprites).
    pub host_cache: bool,
    /// VPS providers only: vendor region/location (e.g. Hetzner `fsn1`).
    /// Empty ⇒ the provider's default.
    pub region: String,
    /// Vendor size/plan/server-type (e.g. Hetzner `cx22`, machine0 `large`).
    /// Empty or `"auto"` ⇒ the provider's default, or — for providers with a
    /// priced size catalog (machine0) — the cheapest size meeting the
    /// `min_vcpu`/`min_ram_gb`/`min_disk_gb`/`gpu`/`nvme` requirements below.
    pub size: String,
    /// Dynamic-size requirement (providers with a size catalog, e.g. machine0):
    /// minimum vCPUs. `0` ⇒ unconstrained. Only consulted when `size` is
    /// unset/`auto`.
    pub min_vcpu: u32,
    /// Dynamic-size requirement: minimum RAM in GiB. `0` ⇒ unconstrained.
    /// Sprites: sets the memory policy limit (MiB = GiB × 1024).
    pub min_ram_gb: u32,
    /// Dynamic-size requirement: minimum disk in GiB. `0` ⇒ unconstrained.
    pub min_disk_gb: u32,
    /// Dynamic-size requirement: require a GPU size.
    pub gpu: bool,
    /// Dynamic-size requirement: require an NVMe-backed local disk.
    pub nvme: bool,
    /// VPS providers and sprites: hard cap on concurrently-managed instances,
    /// the spend guardrail enforced at create (0 = no cap for sprites, 5 for
    /// VPS).
    pub max_instances: u32,
    /// Fly and VPS providers: ceiling on any instance's lifetime in seconds;
    /// their reaper destroys older ones (a VPS bills until destroyed — there is
    /// no free suspended state). Sprites: enforced in the background only with
    /// `lifetime_reap = true`; otherwise reported by `thegn env reap
    /// --provider sprites`. Other provider kinds
    /// have no lifetime reaper: `config validate` warns when this is set. `0` ⇒
    /// no ceiling.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub max_lifetime_secs: u64,
    /// Hibernation (snapshot-then-destroy on idle) for this env's claimed
    /// sandboxes: `auto` (default — on for commodity VPS, off for
    /// scale-to-zero providers), `on`, or `off`. See `[lifecycle]`
    /// `hibernate_after_secs` and `[lifecycle.snapshot]`.
    pub hibernate: HibernateMode,
    /// Per-env idle-seconds override before hibernation. `0` ⇒ the global
    /// `[lifecycle] hibernate_after_secs`.
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
    pub hibernate_idle_secs: u64,
    /// Vercel only: the team id (`team_…`). Not secrets.
    pub team_id: String,
    /// Vercel only: the project id (`prj_…`). Not secrets.
    pub project_id: String,
    /// Vercel and Sprites: which model credential reaches the sandbox; default
    /// none. `upload` copies the claude/codex login into the sandbox's tmpfs;
    /// `proxy`/`inject` are refused. Only the user's own config table may set it.
    #[serde(default)]
    pub model_credentials: ModelCredentials,
    /// Whether the user supplied `model_credentials`; omitted Sprites settings
    /// retain the historical forwarding behavior without adding a config key.
    #[serde(skip)]
    #[schemars(skip)]
    pub model_credentials_explicit: bool,
    /// Vercel only: host suffixes the interactive WebSocket ticket URL may
    /// point at. Empty ⇒ the built-in `vercel.run`, `vercel.sh`, `vercel.com`.
    /// `vercel.app` (and anything under it) is never accepted.
    #[serde(default)]
    pub interactive_host_suffixes: Vec<String>,
    /// Vercel only: the sandbox's network egress. Default `deny`; see
    /// [`SandboxEgress`] and docs/help/sandbox-network.md.
    #[serde(default)]
    pub egress: SandboxEgress,
    /// Vercel only: extra hosts a `deny`/`registries` sandbox may reach, in
    /// the egress host-pattern grammar (`x.com`, `*.x.com`).
    #[serde(default)]
    pub egress_allow: Vec<String>,
    /// Sprites only, opt-in: destroy proven sprites older than `max_lifetime_secs`
    /// in the background (never a `running` one). Off by default.
    #[serde(default)]
    pub lifetime_reap: bool,
    /// `claude` / `codex` only: a host directory whose login is uploaded into a
    /// sandbox instead of the live host login (a dedicated account). Only the
    /// user's own config table may set it.
    #[serde(default)]
    pub login_homes: std::collections::BTreeMap<String, String>,
}

impl EnvProviderConfig {
    pub fn model_credentials_is_explicit(&self) -> bool {
        self.model_credentials_explicit
    }

    pub fn mark_model_credentials_explicit(&mut self) {
        self.model_credentials_explicit = true;
    }

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
            && self.connect == ProviderConnect::Exec
            && self.transport == RemoteTransport::Mosh
            && self.template.is_empty()
            && self.provision_flake.is_empty()
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
            && self.min_vcpu == 0
            && self.min_ram_gb == 0
            && self.min_disk_gb == 0
            && !self.gpu
            && !self.nvme
            && self.max_instances == 0
            && self.max_lifetime_secs == 0
            && self.hibernate == HibernateMode::Auto
            && self.hibernate_idle_secs == 0
            && self.team_id.is_empty()
            && self.project_id.is_empty()
            && self.model_credentials == ModelCredentials::None
            && !self.model_credentials_explicit
            && self.interactive_host_suffixes.is_empty()
            && self.egress == SandboxEgress::Deny
            && self.egress_allow.is_empty()
            && !self.lifetime_reap
            && self.login_homes.is_empty()
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
        // Vercel: no hibernation in b154. Its snapshot semantics differ, and the
        // hibernator's background exec would resume a stopped sandbox.
        if EnvProviderKind::of(&self.provider) == EnvProviderKind::Vercel {
            return false;
        }
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
    /// `exec_command`, or — for providers with no vendor CLI — a thegn
    /// self-bridge: `thegn vps-ssh <name> --` for VPS kinds AND fly (the bridge
    /// resolves a fly machine's IP through the fly provider), `machine0-ssh`
    /// for machine0, `sprite-exec` for sprites. Panes, chrome git/fs reads, and
    /// the persisted worktree location all route over that transport with zero
    /// per-env config. The single source of truth for both `envbuild`
    /// (placement prefixes) and the warm-pool claim rebind.
    ///
    /// `daytona` and `custom` have no built-in bridge: with no `exec_command`
    /// this is EMPTY, and `Placement::transport_refusal` halts the launch
    /// (never a host fallback).
    ///
    /// `exec_command` / `interactive_command` are vendor-CLI bridges. For
    /// every kind except the WSS-native ones they win over the built-in
    /// bridge; for sprites they apply ONLY with `exec = "cli"` — under
    /// `auto`/`api` the native API (`sprite-exec`) is the transport and a
    /// stale `exec_command` is ignored (THE-2054), so a host without the
    /// vendor CLI still reaches the sandbox.
    pub fn control_command_template(&self) -> Vec<String> {
        let wss_cli_only =
            wss_native_provider_kind(&self.provider) && self.exec != ProviderExecMode::Cli;
        if !self.exec_command.is_empty() && !wss_cli_only {
            return self.exec_command.clone();
        }
        let kind = EnvProviderKind::of(&self.provider);
        if kind.is_vps() || kind == EnvProviderKind::Fly {
            let exe = crate::util::self_exe_str();
            return vec![exe, "vps-ssh".into(), "{id}".into(), "--".into()];
        }
        if self.provider.trim() == "machine0" {
            // machine0 reaches its VM over ssh (MCP control plane, ssh data
            // plane) — the same self-bridge role `vps-ssh` plays, but IP + ssh
            // user are resolved via the machine0 provider (`vm_get`), not the VPS
            // registry API. Panes AND chrome reads run through it.
            let exe = crate::util::self_exe_str();
            return vec![exe, "machine0-ssh".into(), "{id}".into(), "--".into()];
        }
        if kind == EnvProviderKind::Vercel {
            // Vercel has no vendor CLI and no ssh: panes AND chrome reads run
            // through the `vercel-exec` self-bridge over the sandbox command
            // API. Not WSS-native (`exec_api` is false), so a configured
            // `exec_command` above still wins, like every non-WSS kind.
            let exe = crate::util::self_exe_str();
            return vec![exe, "vercel-exec".into(), "{id}".into(), "--".into()];
        }
        if wss_native_provider_kind(&self.provider) {
            // WSS-native providers (sprites) have no vendor CLI. The
            // control-plane READ path (chrome git/gh/fs reads + the persisted
            // worktree `GitLoc`) shells out through the thegn self-bridge,
            // which runs the command over the provider's native exec API. Native
            // panes attach over that API directly (see `native_exec_for`);
            // `envbuild` also uses this prefix as the *interactive* prefix
            // (unless `exec = "cli"`), so a launch argv built from it (headless
            // sessions, `sandbox-argv`) runs inside the sprite, never on the
            // host. Without it the location blob had
            // an empty prefix, `GitLoc::from_db` fell back to Local, and every
            // read ran `git -C /workspace` on the HOST (no such dir) → blank
            // branch / ahead-behind / a stale panel.
            let exe = crate::util::self_exe_str();
            return vec![exe, "sprite-exec".into(), "{id}".into(), "--".into()];
        }
        Vec::new()
    }
}

/// Preserve whether a trusted config layer explicitly supplied the
/// `model_credentials` key. The marker is runtime-only and never enters the
/// serialized schema.
pub fn mark_model_credentials_explicit(cfg: &mut crate::config::Config, raw: &serde_json::Value) {
    let Some(envs) = raw.get("env").and_then(serde_json::Value::as_object) else {
        return;
    };
    for (name, value) in envs {
        let Some(provider) = value.get("provider").and_then(serde_json::Value::as_object) else {
            continue;
        };
        if provider.contains_key("model_credentials")
            && let Some(env) = cfg.env.get_mut(name)
        {
            env.provider.mark_model_credentials_explicit();
        }
    }
}

config_enum! {
    /// The managed-sandbox provider vocabulary (`[env.<name>.provider]
    /// provider = …`). The single source: every "is this kind a VPS / native
    /// exec / ssh-reached / scale-to-zero" question is a method here, the
    /// host factory matches it exhaustively, and `thegn_svc::vps::VpsKind`
    /// parses through it. `Custom` is the empty/unknown value — an
    /// `exec_command`-driven provider with no lifecycle API.
    pub enum EnvProviderKind: "env provider" {
        Custom = "custom" | "",
        Daytona = "daytona",
        Sprites = "sprites",
        Hetzner = "hetzner",
        DigitalOcean = "digitalocean",
        Fly = "fly",
        Machine0 = "machine0",
        Vercel = "vercel",
    } default = Custom;
}

impl EnvProviderKind {
    /// Parse a config `provider` string; unknown names are [`Self::Custom`].
    pub fn of(name: &str) -> Self {
        Self::from_str_validated(name).unwrap_or(Self::Custom)
    }

    /// Commodity VPS (Hetzner, DigitalOcean; Vultr as its adapter lands):
    /// lifecycle over the vendor REST API, reach over plain ssh.
    pub fn is_vps(self) -> bool {
        matches!(self, Self::Hetzner | Self::DigitalOcean)
    }

    /// Control plane over the thegn exec self-bridge (`thegn sprite-exec
    /// <id> --`, WSS-native exec) rather than a vendor CLI or ssh.
    pub fn exec_api(self) -> bool {
        matches!(self, Self::Sprites)
    }

    /// Reaches its sandbox over an **ssh subprocess self-bridge**
    /// (`machine0-ssh` / `vps-ssh`) rather than a WSS native-exec relay: the
    /// interactive pane is a SEPARATE process that exec()s ssh, so a
    /// connect/resolve failure is observed at pane-exit and recorded in the
    /// host's connect-health registry (see `agent::env_halt_reason`).
    pub fn ssh_reached(self) -> bool {
        matches!(self, Self::Machine0 | Self::Fly) || self.is_vps()
    }

    /// **Scale-to-zero**: an idle sandbox self-suspends for effectively free
    /// (compute billed only while awake; the filesystem persists). `sprites`
    /// (~30s idle timeout, zero idle compute), `fly` (a stopped machine bills
    /// only for its rootfs), `machine0` (an idle VM `vm_suspend`s to a state
    /// billed only for storage), `vercel` (a persistent sandbox stops at its
    /// session timeout and keeps its filesystem as a snapshot; a stopped
    /// session bills no compute). Everything else is `false` on purpose: a
    /// wrong `false` keeps the safe age-out, a wrong `true` parks billed
    /// instances forever.
    pub fn scale_to_zero(self) -> bool {
        matches!(
            self,
            Self::Sprites | Self::Fly | Self::Machine0 | Self::Vercel
        )
    }

    /// Graduated to the **stable** release channel: Sprites and Vercel Sandbox
    /// environments (interactive worktrees and `thegn job run` selected
    /// runners). Plain SSH/mosh envs are not providers and are stable through
    /// the `Remote` clamp instead. Every other provider — and the managed
    /// `[host.*]` pool —
    /// stays behind the dev-only [`crate::channel::Feature::Providers`] gate,
    /// so `Config::clamp_to_channel` keeps exactly these kinds on stable.
    pub fn stable_channel(self) -> bool {
        matches!(self, Self::Sprites | Self::Vercel)
    }

    /// Self-suspends on idle without thegn's help (the warm-pool policy's
    /// "don't also age it out" case).
    pub fn self_suspends(self) -> bool {
        matches!(self, Self::Sprites | Self::Vercel)
    }

    /// Whether a reaper enforces `max_lifetime_secs` for this kind
    /// (`fly_reaper` / `vps_reaper` / `vercel_reaper`).
    pub fn lifetime_reaped(self) -> bool {
        matches!(self, Self::Fly | Self::Vercel) || self.is_vps()
    }

    /// Whether this provider's lifecycle (provision, teardown, orphan reaping,
    /// and warm pool) has not been verified against a live account. `thegn doctor`
    /// displays this state by owner decision THE-142, and for Daytona because
    /// its upstream is in maintenance (closed-core, archived public repo).
    pub fn live_unverified(self) -> bool {
        matches!(self, Self::Hetzner | Self::Daytona | Self::Vercel)
    }

    /// Credential-free kind: no host credential (agent logins, dotfiles,
    /// atuin, git token) is ever uploaded to a sandbox of this kind. The repo
    /// goes in as a host-built bundle and the credential-bearing provisioning
    /// steps are stripped and refused (`provider_credentials`, enforced by
    /// lane 5 of b154).
    pub fn credential_free(self) -> bool {
        matches!(self, Self::Vercel)
    }

    /// Whether the automatic session-exit pull-back (THE-2021) may call this
    /// provider. Off for Vercel: the pull would resume a sandbox that stopped
    /// at its timeout and bill compute with nobody at the keyboard, so Vercel
    /// pulls are left to an explicit `thegn env pull`.
    pub fn exit_pull_back_enabled(self) -> bool {
        !matches!(self, Self::Vercel)
    }

    /// Whether `[lifecycle] eager = "workspace"|"all"` may create this kind's
    /// sandbox in the background for a worktree the user has not focused. Off
    /// for Vercel and Sprites: a paid sandbox is created only from an explicit
    /// open or provision (the focused worktree still provisions on focus).
    pub fn eager_create_enabled(self) -> bool {
        !matches!(self, Self::Vercel | Self::Sprites)
    }

    /// Lowers `network_allow` / `network_block` to a provider policy API and
    /// reads the applied policy back before handoff. This is the static
    /// capability of the kind; a per-worktree report replaces it later.
    ///
    /// | kind         | egress_api |
    /// |--------------|------------|
    /// | custom       | no         |
    /// | daytona      | no         |
    /// | sprites      | yes (`rules_from` + `egress_admit`) |
    /// | hetzner      | no         |
    /// | digitalocean | no         |
    /// | fly          | no         |
    /// | machine0     | no         |
    /// | vercel       | yes (allow-list policy; block-only refused) |
    pub fn egress_api(self) -> bool {
        matches!(self, Self::Sprites | Self::Vercel)
    }

    /// Native checkpoint/restore.
    ///
    /// | kind         | checkpoints |
    /// |--------------|-------------|
    /// | custom       | no          |
    /// | daytona      | no          |
    /// | sprites      | yes (Sprites checkpoints) |
    /// | hetzner      | no (a powered-off VPS still bills; recycle destroys) |
    /// | digitalocean | no          |
    /// | fly          | no          |
    /// | machine0     | yes (machine0 images) |
    /// | vercel       | no          |
    pub fn checkpoints(self) -> bool {
        matches!(self, Self::Sprites | Self::Machine0)
    }

    /// A files transport exists.
    ///
    /// | kind         | files |
    /// |--------------|-------|
    /// | custom       | no    |
    /// | daytona      | no    |
    /// | sprites      | yes (Sprites API) |
    /// | hetzner      | yes (ssh shim) |
    /// | digitalocean | yes (ssh shim) |
    /// | fly          | yes (ssh to the machine's dedicated IPv4) |
    /// | machine0     | yes (ssh) |
    /// | vercel       | yes (fs/read + gzipped-tar fs/write) |
    pub fn files(self) -> bool {
        matches!(
            self,
            Self::Sprites
                | Self::Hetzner
                | Self::DigitalOcean
                | Self::Fly
                | Self::Machine0
                | Self::Vercel
        )
    }
}

/// Whether `name` names a commodity-VPS provider kind. See
/// [`EnvProviderKind::is_vps`] — the one source.
pub fn vps_provider_kind(name: &str) -> bool {
    EnvProviderKind::of(name).is_vps()
}

/// Whether `name` names a WSS-native exec provider. See
/// [`EnvProviderKind::exec_api`].
pub fn wss_native_provider_kind(name: &str) -> bool {
    EnvProviderKind::of(name).exec_api()
}

/// Whether a provider reaches its sandbox over an ssh self-bridge. See
/// [`EnvProviderKind::ssh_reached`].
pub fn ssh_reached_provider_kind(name: &str) -> bool {
    EnvProviderKind::of(name).ssh_reached()
}

/// Whether a provider *kind* is scale-to-zero. See
/// [`EnvProviderKind::scale_to_zero`].
pub fn provider_scale_to_zero(name: &str) -> bool {
    EnvProviderKind::of(name).scale_to_zero()
}

/// Whether a provider self-suspends on idle. See
/// [`EnvProviderKind::self_suspends`].
pub fn provider_self_suspends(name: &str) -> bool {
    EnvProviderKind::of(name).self_suspends()
}

/// `[metrics]` — Prometheus scrape targets for sidebar metrics display.
/// Each target is scraped directly via HTTP; no Prometheus server required.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MetricsConfig {
    /// Scrape interval in seconds.
    #[serde(alias = "interval-secs")]
    #[schemars(range(max = "crate::time_policy::MAX_CADENCE_SECS"))]
    pub interval_secs: f64,
    /// Request timeout in milliseconds.
    #[serde(alias = "timeout-ms")]
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_MILLIS"))]
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

config_enum! {
    /// How a `[[metrics.targets]]` entry produces Prometheus text.
    /// - `prometheus` — HTTP scrape of a `/metrics` endpoint (the default,
    ///   today's behaviour).
    /// - `command` — run a configured argv (no shell) and parse its stdout.
    ///   **Global config only** (see [`MetricsTarget::command`]).
    pub enum MetricsTargetKind: "metrics target kind" {
        Prometheus = "prometheus" | "http" | "scrape",
        Command = "command" | "exec",
    } default = Prometheus;
}

/// One metrics collector: a Prometheus HTTP scrape (default) or a command whose
/// stdout is Prometheus text format.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct MetricsTarget {
    /// Display name in the sidebar.
    pub name: String,
    /// For `kind = "prometheus"`: URL to scrape (e.g.
    /// `http://localhost:9091/metrics`). Ignored (may be empty) for command
    /// collectors.
    #[serde(default)]
    pub url: String,
    /// Collector kind. Defaults to `prometheus` so existing targets are
    /// unchanged.
    #[serde(default)]
    pub kind: MetricsTargetKind,
    /// For `kind = "command"`: the argv to execute, `command[0]` the program and
    /// the rest its arguments. Run **without a shell** — no word splitting, no
    /// glob, no `$VAR` expansion — with the target's timeout and body cap
    /// applied to its stdout. A collector needing a secret reads it from its own
    /// environment, never from thegn config.
    ///
    /// Command collectors are a config-driven code-execution door and are
    /// therefore **global config only**: a repo/workspace `.thegn.*` overlay
    /// cannot define one (see `Config::repo_command_collector_warnings`).
    #[serde(default)]
    pub command: Vec<String>,
    /// Metrics to display (allowlist). Empty = all.
    #[serde(default)]
    pub metrics: Vec<String>,
    /// Optional labels to match (e.g., `instance="localhost:9091"`).
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
}

impl MetricsTarget {
    /// The argv for a command collector — `None` for a prometheus target or a
    /// command target with an empty/whitespace-only program. The single place
    /// that decides an entry is a *runnable* command, so the supervisor and the
    /// validators agree.
    pub fn command_argv(&self) -> Option<&[String]> {
        if self.kind != MetricsTargetKind::Command {
            return None;
        }
        match self.command.first() {
            Some(prog) if !prog.trim().is_empty() => Some(&self.command),
            _ => None,
        }
    }
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
    /// - `local` — a directory on this host (`$XDG_STATE_HOME/thegn/snapshots`
    ///   unless `dir` overrides it). Zero config, no credentials.
    /// - `s3` — an S3-compatible bucket (`bucket`/`endpoint`/`region`/`prefix`
    ///   plus `access_key`/`secret_key` secret refs).
    pub enum SnapshotBackend: "snapshot backend" {
        Local = "local" | "fs", S3 = "s3",
    } default = Local;
}

/// `[lifecycle]` — budget-governed warm/suspend policy for managed-provider
/// sandboxes. The defaults are budget-safe: thegn's background sidebar/activity
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
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
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
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
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
    /// `$XDG_STATE_HOME/thegn/snapshots`.
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
    /// S3 backend: sign the x-amz-server-side-encryption: AES256 header into every snapshot upload (default). Set false only for an
    /// S3-compatible store that rejects SSE-S3; it never permits a plaintext endpoint.
    pub server_side_encryption: bool,
    /// S3 backend: sign `If-None-Match: *` into every immutable artifact and
    /// manifest upload (default). S3-compatible stores that reject conditional
    /// writes must set it false.
    pub conditional_create: bool,
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
            prefix: "thegn".into(),
            access_key: "env:AWS_ACCESS_KEY_ID".into(),
            secret_key: "env:AWS_SECRET_ACCESS_KEY".into(),
            server_side_encryption: true,
            conditional_create: true,
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
    /// Pre-provisioned spare sandboxes to keep ready per (repo, env). `0` (the
    /// default) = **off**: the pool is strictly opt-in (idle spares consume
    /// provider quota, fatal on a low-limit account). A non-zero value opts in
    /// (clamped to a runaway ceiling). The `+`/`-` hotkey persists a per-repo+env
    /// override that wins over this (including `0` to force the pool off).
    pub size: usize,
    /// AgeOut (billed-when-stopped, e.g. VPS) providers only: destroy an unclaimed
    /// pool member idle longer than this (seconds). Ignored for scale-to-zero
    /// providers, whose idle spares self-suspend for free and are parked, not
    /// aged out (they still rotate on flake.lock drift).
    #[schemars(range(max = "crate::time_policy::MAX_DURATION_SECS"))]
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

    #[test]
    fn model_credentials_round_trips_and_defaults_to_none() {
        use crate::seam::Kind;
        assert_eq!(ModelCredentials::default(), ModelCredentials::None);
        for mode in ModelCredentials::ALL {
            assert_eq!(
                ModelCredentials::from_str_validated(mode.as_str()),
                Ok(*mode)
            );
            let pc: EnvProviderConfig =
                toml::from_str(&format!("model_credentials = {:?}\n", mode.as_str())).unwrap();
            assert_eq!(pc.model_credentials, *mode);
            let back = toml::to_string(&pc).unwrap();
            assert!(
                back.contains(&format!("model_credentials = \"{}\"", mode.as_str())),
                "{back}"
            );
        }
        assert!(ModelCredentials::from_str_validated("key").is_err());
        let pc: EnvProviderConfig = toml::from_str("").unwrap();
        assert_eq!(pc.model_credentials, ModelCredentials::None);
        assert!(pc.interactive_host_suffixes.is_empty());
        assert!(pc.is_default());
    }

    #[test]
    fn model_credentials_presence_marker_keeps_omitted_distinct_from_none() {
        let mut cfg = crate::config::Config::default();
        cfg.env.insert(
            "unset".into(),
            crate::config::EnvConfig {
                provider: EnvProviderConfig {
                    provider: "sprites".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(!cfg.env["unset"].provider.model_credentials_is_explicit());
        crate::config::apply_json_overlay(
            &mut cfg,
            serde_json::json!({
                "env": {
                    "set": {
                        "provider": {
                            "provider": "sprites",
                            "model_credentials": "none"
                        }
                    }
                }
            }),
        )
        .unwrap();
        assert!(!cfg.env["unset"].provider.model_credentials_is_explicit());
        assert!(cfg.env["set"].provider.model_credentials_is_explicit());
        assert!(!cfg.env["set"].provider.is_default());
    }

    #[test]
    fn new_vercel_keys_make_the_provider_table_non_default() {
        let creds = EnvProviderConfig {
            model_credentials: ModelCredentials::Upload,
            ..Default::default()
        };
        assert!(!creds.is_default());
        let hosts = EnvProviderConfig {
            interactive_host_suffixes: vec!["vercel.run".into()],
            ..Default::default()
        };
        assert!(!hosts.is_default());
    }

    #[test]
    fn egress_defaults_to_deny_and_round_trips() {
        let pc: EnvProviderConfig = toml::from_str("").unwrap();
        assert_eq!(pc.egress, SandboxEgress::Deny);
        assert!(pc.egress_allow.is_empty());
        assert!(pc.is_default());
        for mode in &[
            SandboxEgress::Deny,
            SandboxEgress::Registries,
            SandboxEgress::AllowAll,
        ] {
            let pc: EnvProviderConfig = toml::from_str(&format!(
                "egress = {:?}\negress_allow = [\"a.com\"]\n",
                mode.as_str()
            ))
            .unwrap();
            assert_eq!(pc.egress, *mode);
            assert!(!pc.is_default());
            assert!(toml::to_string(&pc).unwrap().contains("egress_allow"));
        }
        assert!(SandboxEgress::from_str_validated("open").is_err());
        let pc = EnvProviderConfig {
            egress: SandboxEgress::Registries,
            ..Default::default()
        };
        assert!(!pc.is_default());
    }

    #[test]
    fn lifetime_reap_and_login_homes_round_trip_and_default_off() {
        let pc: EnvProviderConfig = toml::from_str("").unwrap();
        assert!(!pc.lifetime_reap);
        assert!(pc.login_homes.is_empty());
        assert!(pc.is_default());

        let pc: EnvProviderConfig = toml::from_str(
            "lifetime_reap = true\nlogin_homes = { claude = \"/srv/a\", codex = \"/srv/b\" }\n",
        )
        .unwrap();
        assert!(pc.lifetime_reap);
        assert_eq!(
            pc.login_homes.get("claude").map(String::as_str),
            Some("/srv/a")
        );
        assert_eq!(
            pc.login_homes.get("codex").map(String::as_str),
            Some("/srv/b")
        );
        let back: EnvProviderConfig = toml::from_str(&toml::to_string(&pc).unwrap()).unwrap();
        assert!(back.lifetime_reap);
        assert_eq!(back.login_homes, pc.login_homes);
    }

    #[test]
    fn lifetime_reap_and_login_homes_make_the_provider_table_non_default() {
        let reap = EnvProviderConfig {
            lifetime_reap: true,
            ..Default::default()
        };
        assert!(!reap.is_default());
        let homes = EnvProviderConfig {
            login_homes: [("claude".to_string(), "/srv/a".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        assert!(!homes.is_default());
    }

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
        // Regression: `connect` was omitted from is_default(), so a connect-only
        // provider table was skip-serialized and lost through the config
        // round-trip. A non-default `connect` must make is_default() false.
        let e = EnvProviderConfig {
            connect: ProviderConnect::Ssh,
            ..Default::default()
        };
        assert!(!e.is_default(), "connect = ssh must not be default");
    }

    #[test]
    fn machine0_classification_and_provision_flake() {
        // machine0 is scale-to-zero (vm_suspend) but is NOT a commodity VPS kind
        // (no ssh-key/ledger/reaper path) and NOT WSS-native (pane rides ssh).
        assert!(provider_scale_to_zero("machine0"));
        assert!(provider_scale_to_zero(" machine0 "));
        assert!(!vps_provider_kind("machine0"));
        assert!(!wss_native_provider_kind("machine0"));
        // machine0 idles ~free ⇒ hibernate-on-idle stays OFF under auto.
        assert!(!provider_env("machine0").hibernate_enabled());
        // machine0 is scale-to-zero but does NOT self-suspend (needs explicit
        // vm_suspend); only sprites self-idles.
        assert!(!provider_self_suspends("machine0"));
        assert!(!provider_self_suspends("fly"));
        assert!(provider_self_suspends("sprites"));
        // ssh-reached kinds (pane is a separate `*-ssh` subprocess): machine0, fly,
        // and the VPS kinds — but NOT the WSS-native sprites or an unknown provider.
        assert!(ssh_reached_provider_kind("machine0"));
        assert!(ssh_reached_provider_kind(" fly "));
        assert!(ssh_reached_provider_kind("hetzner"));
        assert!(ssh_reached_provider_kind("digitalocean"));
        assert!(!ssh_reached_provider_kind("sprites"));
        assert!(!ssh_reached_provider_kind("daytona"));
        assert!(!ssh_reached_provider_kind(""));
        // The pane/chrome reach the VM via the machine0-ssh self-bridge.
        let tpl = EnvProviderConfig {
            provider: "machine0".into(),
            ..Default::default()
        }
        .control_command_template();
        assert_eq!(tpl.get(1).map(String::as_str), Some("machine0-ssh"));
        assert_eq!(tpl.last().map(String::as_str), Some("--"));
        // The NixOS provision_flake + a non-mosh transport participate in is_default().
        let e = EnvProviderConfig {
            provision_flake: "github:me/nix#host".into(),
            ..Default::default()
        };
        assert!(!e.is_default());
        assert!(
            EnvProviderConfig::default().is_default(),
            "default transport is mosh"
        );
        let e = EnvProviderConfig {
            transport: RemoteTransport::Ssh,
            ..Default::default()
        };
        assert!(!e.is_default());
    }

    fn with_exec_command(provider: &str, exec: ProviderExecMode) -> EnvProviderConfig {
        EnvProviderConfig {
            provider: provider.into(),
            exec,
            exec_command: vec!["vendor".into(), "exec".into(), "{id}".into(), "--".into()],
            ..Default::default()
        }
    }

    fn vendor_argv() -> Vec<String> {
        vec!["vendor".into(), "exec".into(), "{id}".into(), "--".into()]
    }

    fn assert_sprite_exec(tpl: &[String]) {
        assert_eq!(tpl.len(), 4, "{tpl:?}");
        assert_eq!(&tpl[1..], ["sprite-exec", "{id}", "--"], "{tpl:?}");
    }

    #[test]
    fn sprites_auto_ignores_stale_exec_command() {
        // THE-2054: a leftover vendor-CLI `exec_command` must not override the
        // native API transport when `exec = "auto"`.
        let tpl = with_exec_command("sprites", ProviderExecMode::Auto).control_command_template();
        assert_sprite_exec(&tpl);
    }

    #[test]
    fn sprites_api_ignores_exec_command() {
        let tpl = with_exec_command("sprites", ProviderExecMode::Api).control_command_template();
        assert_sprite_exec(&tpl);
    }

    #[test]
    fn sprites_cli_uses_exec_command() {
        let tpl = with_exec_command("sprites", ProviderExecMode::Cli).control_command_template();
        assert_eq!(tpl, vendor_argv());
    }

    #[test]
    fn non_wss_kinds_keep_exec_command_precedence() {
        for kind in ["hetzner", "daytona"] {
            let tpl = with_exec_command(kind, ProviderExecMode::Auto).control_command_template();
            assert_eq!(tpl, vendor_argv(), "{kind}");
        }
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
    fn snapshot_store_sse_and_conditional_create_defaults_on_and_parse_opt_outs() {
        assert!(SnapshotStoreConfig::default().server_side_encryption);
        assert!(SnapshotStoreConfig::default().conditional_create);

        let opted_out = toml::from_str::<SnapshotStoreConfig>("server_side_encryption = false")
            .expect("SSE opt-out parses");
        assert!(!opted_out.server_side_encryption);

        let opted_out = toml::from_str::<SnapshotStoreConfig>("conditional_create = false")
            .expect("conditional-create opt-out parses");
        assert!(!opted_out.conditional_create);

        let empty = toml::from_str::<SnapshotStoreConfig>("").expect("empty config parses");
        assert!(empty.server_side_encryption);
        assert!(empty.conditional_create);
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

    #[test]
    fn lifetime_reaped_matches_the_scheduled_reapers() {
        for kind in [
            EnvProviderKind::Custom,
            EnvProviderKind::Daytona,
            EnvProviderKind::Sprites,
            EnvProviderKind::Hetzner,
            EnvProviderKind::DigitalOcean,
            EnvProviderKind::Fly,
            EnvProviderKind::Machine0,
            EnvProviderKind::Vercel,
        ] {
            match kind {
                EnvProviderKind::Fly
                | EnvProviderKind::Hetzner
                | EnvProviderKind::DigitalOcean
                | EnvProviderKind::Vercel => {
                    assert!(kind.lifetime_reaped(), "{kind:?} is reaped");
                }
                EnvProviderKind::Custom
                | EnvProviderKind::Daytona
                | EnvProviderKind::Sprites
                | EnvProviderKind::Machine0 => {
                    assert!(!kind.lifetime_reaped(), "{kind:?} has no lifetime reaper");
                }
            }
        }
    }

    #[test]
    fn live_unverified_marks_hetzner_daytona_and_vercel() {
        for kind in [
            EnvProviderKind::Custom,
            EnvProviderKind::Daytona,
            EnvProviderKind::Sprites,
            EnvProviderKind::Hetzner,
            EnvProviderKind::DigitalOcean,
            EnvProviderKind::Fly,
            EnvProviderKind::Machine0,
            EnvProviderKind::Vercel,
        ] {
            match kind {
                EnvProviderKind::Hetzner | EnvProviderKind::Daytona | EnvProviderKind::Vercel => {
                    assert!(kind.live_unverified(), "{kind:?} is live-unverified");
                }
                EnvProviderKind::Custom
                | EnvProviderKind::Sprites
                | EnvProviderKind::DigitalOcean
                | EnvProviderKind::Fly
                | EnvProviderKind::Machine0 => {
                    assert!(!kind.live_unverified(), "{kind:?} is not live-unverified");
                }
            }
        }
    }

    /// Background eager scopes never create a paid sandbox: Vercel and Sprites
    /// are off, every other kind is on.
    #[test]
    fn eager_create_is_off_only_for_paid_sandbox_kinds() {
        for kind in [
            EnvProviderKind::Custom,
            EnvProviderKind::Daytona,
            EnvProviderKind::Sprites,
            EnvProviderKind::Hetzner,
            EnvProviderKind::DigitalOcean,
            EnvProviderKind::Fly,
            EnvProviderKind::Machine0,
            EnvProviderKind::Vercel,
        ] {
            let paid = matches!(kind, EnvProviderKind::Vercel | EnvProviderKind::Sprites);
            assert_eq!(kind.eager_create_enabled(), !paid, "{kind:?}");
        }
        assert!(!EnvProviderKind::of("sprites").eager_create_enabled());
        assert!(!EnvProviderKind::of("vercel").eager_create_enabled());
    }

    #[test]
    fn vercel_kind_predicates() {
        let k = EnvProviderKind::of("vercel");
        assert_eq!(k, EnvProviderKind::Vercel);
        assert_eq!(EnvProviderKind::of(" vercel "), EnvProviderKind::Vercel);
        assert_eq!(k.as_str(), "vercel");
        assert!(!k.exec_api(), "no resident bridge / native pane path");
        assert!(!k.ssh_reached(), "no ssh");
        assert!(k.scale_to_zero());
        assert!(k.self_suspends());
        assert!(k.lifetime_reaped());
        assert!(k.live_unverified());
        assert!(!k.is_vps());
        assert!(k.credential_free());
        // Credential-free is Vercel only.
        for other in [
            EnvProviderKind::Custom,
            EnvProviderKind::Daytona,
            EnvProviderKind::Sprites,
            EnvProviderKind::Hetzner,
            EnvProviderKind::DigitalOcean,
            EnvProviderKind::Fly,
            EnvProviderKind::Machine0,
        ] {
            assert!(!other.credential_free(), "{other:?}");
            assert!(other.exit_pull_back_enabled(), "{other:?}");
            assert_eq!(
                other.eager_create_enabled(),
                other != EnvProviderKind::Sprites,
                "{other:?}"
            );
        }
        // Only an explicit `thegn env pull` may resume a stopped Vercel sandbox.
        assert!(!k.exit_pull_back_enabled());
        // Background eager scopes never create a paid Vercel sandbox.
        assert!(!k.eager_create_enabled());
        // Scale-to-zero ⇒ hibernate stays off under auto.
        assert!(!provider_env("vercel").hibernate_enabled());
        assert!(!wss_native_provider_kind("vercel"));
        assert!(!ssh_reached_provider_kind("vercel"));
        let mut on = provider_env("vercel");
        on.hibernate = HibernateMode::On;
        assert!(
            !on.hibernate_enabled(),
            "explicit on never hibernates vercel"
        );
    }

    #[test]
    fn vercel_control_template_is_the_vercel_exec_bridge() {
        let tpl = provider_env("vercel").control_command_template();
        assert_eq!(tpl.len(), 4, "{tpl:?}");
        assert_eq!(tpl[0], crate::util::self_exe_str());
        assert_eq!(&tpl[1..], ["vercel-exec", "{id}", "--"], "{tpl:?}");
    }

    #[test]
    fn vercel_configured_exec_command_wins() {
        // Not WSS-native: a configured exec_command wins under every exec mode,
        // matching the other non-WSS kinds.
        for mode in [
            ProviderExecMode::Auto,
            ProviderExecMode::Api,
            ProviderExecMode::Cli,
        ] {
            let tpl = with_exec_command("vercel", mode).control_command_template();
            assert_eq!(tpl, vendor_argv(), "{mode:?}");
        }
    }

    #[test]
    fn vercel_ids_participate_in_is_default() {
        let e = EnvProviderConfig {
            team_id: "team_abc".into(),
            ..Default::default()
        };
        assert!(!e.is_default());
        let e = EnvProviderConfig {
            project_id: "prj_abc".into(),
            ..Default::default()
        };
        assert!(!e.is_default());
    }

    #[test]
    fn env_provider_kind_capability_table() {
        use crate::seam::Kind as _;
        // (kind, egress_api, checkpoints, files): today's provider caps, pinned.
        let table = [
            (EnvProviderKind::Custom, false, false, false),
            (EnvProviderKind::Daytona, false, false, false),
            (EnvProviderKind::Sprites, true, true, true),
            (EnvProviderKind::Hetzner, false, false, true),
            (EnvProviderKind::DigitalOcean, false, false, true),
            (EnvProviderKind::Fly, false, false, true),
            (EnvProviderKind::Machine0, false, true, true),
            (EnvProviderKind::Vercel, true, false, true),
        ];
        assert_eq!(
            table.len(),
            EnvProviderKind::ALL.len(),
            "every kind is in the table"
        );
        for kind in [
            EnvProviderKind::Custom,
            EnvProviderKind::Daytona,
            EnvProviderKind::Sprites,
            EnvProviderKind::Hetzner,
            EnvProviderKind::DigitalOcean,
            EnvProviderKind::Fly,
            EnvProviderKind::Machine0,
            EnvProviderKind::Vercel,
        ] {
            let row = table
                .iter()
                .find(|row| row.0 == kind)
                .unwrap_or_else(|| panic!("{kind:?} missing from the table"));
            assert_eq!(kind.egress_api(), row.1, "{kind:?} egress_api");
            assert_eq!(kind.checkpoints(), row.2, "{kind:?} checkpoints");
            assert_eq!(kind.files(), row.3, "{kind:?} files");
        }
    }
}
