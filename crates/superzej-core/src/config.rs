//! User configuration — a layered, validated system.
//!
//! Precedence, lowest to highest:
//!   1. built-in defaults (`Config::default`)
//!   2. `$XDG_CONFIG_HOME/superzej/config.toml` (or `--config <path>`)
//!   3. `SUPERZEJ_<SECTION>_<KEY>` environment overrides (see [`env_overlay`])
//!   4. CLI flags (a [`ConfigOverlay`] built by `main`)
//!
//! Plus a repo-root `.superzej.{toml,yaml,yml,json}` overlay, scoped to
//! `[sandbox]`, applied per-repo in [`Config::repo_sandbox`].
//!
//! Bad values never block: an unknown enum value warns and falls back to the
//! default (the strict check lives in `superzej config validate`). The
//! home-manager module renders the file; keys match the serde field names.

use crate::env::Environment;
use crate::remote::GitLoc;
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Prefix a config diagnostic and emit it as a warning. Centralised so the
/// validated-enum deserializers and the env/flag layers speak with one voice.
pub fn config_warn(msg: &str) {
    crate::msg::warn(&format!("config: {msg}"));
}

/// Expand a config value that may be an environment-variable reference.
///
/// A string of the form `"env:VAR_NAME"` is replaced by the value of the
/// environment variable `VAR_NAME`. A string of the form `"file:PATH"` is
/// replaced by the (trimmed) contents of `PATH` — `~` is expanded to `$HOME`.
/// Any other non-empty string is returned as-is. An empty string, a missing
/// environment variable, or an unreadable/empty file all return `None`.
///
/// Used for secrets-refs (API keys, VPN auth keys) so credentials live in the
/// environment or an out-of-tree file rather than in plaintext in the config.
pub fn expand_env_ref(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(var_name) = v.strip_prefix("env:") {
        std::env::var(var_name)
            .ok()
            .filter(|s| !s.trim().is_empty())
    } else if let Some(path) = v.strip_prefix("file:") {
        let path = expand_tilde(path.trim());
        std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        Some(v.to_string())
    }
}

/// Expand a leading `~` / `~/` to `$HOME` (best-effort; returns the input
/// unchanged when `$HOME` is unset or the path has no leading tilde).
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    if path == "~"
        && let Ok(home) = std::env::var("HOME")
    {
        return home;
    }
    path.to_string()
}

/// Declare a string-backed, validated, TOML-friendly enum.
///
/// Generates `Default`, `Display`, `as_str`, `from_str_validated` (strict, for
/// `config validate`), and serde impls. Deserialization is **infallible**: an
/// unrecognised value warns and yields the default, so a typo never blocks a
/// launch. `Serialize` round-trips to the canonical string (for `config show`).
macro_rules! config_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident : $kind:literal {
            $( $variant:ident = $canon:literal $(| $alias:literal)* ),+ $(,)?
        } default = $def:ident;
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, schemars::JsonSchema)]
        $vis enum $name { $( $variant ),+ }

        impl $name {
            /// Strict parse: `Err` (with the valid set) on an unknown value.
            pub fn from_str_validated(s: &str) -> Result<Self, String> {
                match s.trim().to_ascii_lowercase().as_str() {
                    $( $canon $(| $alias)* => Ok($name::$variant), )+
                    other => Err(format!(
                        "unknown {} {:?}; expected one of: {}",
                        $kind, other, [$( $canon ),+].join(", ")
                    )),
                }
            }
            /// The canonical string form (what serialization emits).
            pub fn as_str(self) -> &'static str {
                match self { $( $name::$variant => $canon ),+ }
            }
        }
        impl Default for $name { fn default() -> Self { $name::$def } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Ok($name::from_str_validated(&s).unwrap_or_else(|e| {
                    config_warn(&e);
                    $name::default()
                }))
            }
        }
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }
    };
}

config_enum! {
    /// TUI used for the agent/tool/repo pickers.
    pub enum Picker: "picker" {
        Auto = "auto", Gum = "gum", Fzf = "fzf", Select = "select",
    } default = Auto;
}
config_enum! {
    /// Whether the outer terminal renders curly underlines (conflict
    /// squiggles). "auto" sniffs $TERM/$TERM_PROGRAM; unsupported terminals
    /// degrade to a single underline.
    pub enum UndercurlMode: "undercurl mode" {
        Auto = "auto", On = "on", Off = "off",
    } default = Auto;
}
config_enum! {
    /// Color fidelity sent to the outer terminal. "auto" sniffs the terminal
    /// (COLORTERM / $TERM / WT_SESSION / NO_COLOR) and degrades truecolor →
    /// 256 → 16 → mono; the explicit values pin a depth.
    pub enum ColorMode: "color mode" {
        Auto = "auto",
        Truecolor = "truecolor" | "24bit",
        Ansi256 = "256",
        Ansi16 = "16",
        None = "none" | "mono",
    } default = Auto;
}
config_enum! {
    /// Glyph fidelity for chrome (box drawing, dots, arrows, logotype). "auto"
    /// sniffs the locale + terminal; "ascii" forces 7-bit fallbacks for bare
    /// terminals/fonts.
    pub enum GlyphMode: "glyph mode" {
        Auto = "auto", Unicode = "unicode", Ascii = "ascii",
    } default = Auto;
}
config_enum! {
    /// Where worktrees live on disk.
    pub enum WorktreeMode: "worktree_mode" {
        Global = "global", InRepo = "in_repo",
    } default = Global;
}
config_enum! {
    /// Auto branch-name style.
    pub enum NameScheme: "name_scheme" {
        Words = "words", Numbered = "numbered",
    } default = Words;
}
config_enum! {
    /// Sandbox backend selector (the config-facing set; the runtime detection
    /// enum lives in `sandbox.rs`). `Auto` walks `backend_chain`.
    pub enum SandboxBackend: "sandbox backend" {
        Auto = "auto",
        Podman = "podman" | "podman-rootless" | "rootless-podman",
        PodmanRootful = "podman-rootful" | "rootful-podman",
        Docker = "docker",
        Smol = "smol" | "smolmachines",
        Bwrap = "bwrap" | "bubblewrap",
        Systemd = "systemd" | "systemd-run",
        Apple = "apple" | "container",
        Wsl = "wsl",
        WinAppContainer = "winappcontainer" | "appcontainer",
        WinJobObject = "winjobobject" | "jobobject",
        None = "none" | "host",
    } default = Auto;
}
config_enum! {
    /// Sandbox network mode.
    pub enum Network: "sandbox network" {
        Nat = "nat", Host = "host", None = "none",
    } default = Nat;
}
config_enum! {
    /// Sandbox hardening preset — a named bundle of OS-isolation knobs
    /// (read-only root, capability drops, no-new-privileges, a pids cap, and a
    /// network floor). Selectable per level: global `[sandbox] profile`, per
    /// workspace/repo via the `.superzej.toml` overlay, or `SUPERZEJ_SANDBOX_PROFILE`.
    /// The embedded agent gets its own container hardened by `agent_profile`.
    ///
    /// - `open`     — no hardening; reproduces pre-preset behavior (back-compat).
    /// - `hardened` — read-only root + no-new-privileges + pids cap; networking
    ///                and capabilities left intact so interactive dev (fetch,
    ///                debuggers, ping) keeps working. The default.
    /// - `sealed`   — full lockdown: `network=none`, read-only root, drop ALL
    ///                capabilities, no-new-privileges, tighter pids cap.
    /// - `sealed-tunnel` — the same lockdown as `sealed` (read-only root, drop
    ///                ALL capabilities on the worktree, no-new-privileges, tight
    ///                pids cap) EXCEPT the worktree has no *direct* host egress
    ///                but is attached to its `[sandbox.vpn]` overlay: its only
    ///                route is the tunnel (the sidecar holds NET_ADMIN/TUN, not
    ///                the worktree). With no VPN configured it degrades to
    ///                `network=none`, i.e. behaves exactly like `sealed`.
    pub enum SandboxProfile: "sandbox profile" {
        Open = "open" | "off" | "none",
        Hardened = "hardened" | "guarded",
        Sealed = "sealed" | "locked" | "isolated",
        SealedTunnel = "sealed-tunnel" | "tunnel-only" | "vpn-only",
    } default = Hardened;
}
config_enum! {
    /// Whether superzej pre-warms a worktree's `direnv` cache **on the host** so
    /// the in-sandbox `direnv` hook works against the read-only `/nix/store`.
    /// A cold `nix-direnv` (`use flake`) cache makes the in-pane direnv try to
    /// rebuild the devShell, which fails on the read-only store + no daemon;
    /// warming on the host (writable store + daemon) lets the pane replay the
    /// cached env read-only. See [`crate::direnv`].
    ///
    /// - `auto`         — warm + `direnv allow` the worktree (trusts the repo's
    ///                    own `.envrc`, the same boundary `inject_devshell`
    ///                    already crosses). The default.
    /// - `allowed-only` — warm only worktrees the user has already
    ///                    `direnv allow`-ed; never auto-allows.
    /// - `off`          — never warm (the in-pane direnv falls back as before).
    pub enum WarmDirenv: "warm_direnv" {
        Auto = "auto" | "on" | "true",
        AllowedOnly = "allowed-only" | "allowed_only" | "allowed",
        Off = "off" | "false" | "none" | "no",
    } default = Auto;
}
config_enum! {
    /// How an env-bundle's Tier-2 dotfiles are materialized into its managed
    /// HOME (`[bundle.<n>.dotfiles] mode`). See [`crate::bundle`].
    ///
    /// - `symlink`  — symlink each source entry into the managed HOME (cheapest;
    ///                edits to the source are reflected live). The default.
    /// - `template` — copy the source tree (a private snapshot; edits to the
    ///                source do not leak into the bundle until re-materialized).
    pub enum DotfileMode: "dotfile mode" {
        Symlink = "symlink" | "link", Template = "template" | "copy",
    } default = Symlink;
}
config_enum! {
    /// `[sandbox.home] strategy` — how hard superzej tries to reproduce *your*
    /// host shell inside a sandbox/remote. A ladder mirroring the repo-toolchain
    /// tiers, applied by the env provisioner (see [`crate::envplan`]).
    ///
    /// - `clean`        — no personal dotfiles/tools; just a plain rc-free login
    ///                    shell (also the runtime watchdog fallback). Bulletproof,
    ///                    feels foreign. Good for throwaway sandboxes.
    /// - `portable`     — install `tools`, run `dotfiles_repo`/`setup`, and upload
    ///                    only PORTABLE dotfiles. A dotfile that hard-codes absent
    ///                    paths (e.g. a home-manager rc full of `/nix/store/…`) is
    ///                    SKIPPED with a warning rather than uploaded broken. The
    ///                    default — safe everywhere.
    /// - `tool-parity`  — `portable` plus: tools your dotfiles reference but didn't
    ///                    declare are installed too (Nix-first), so a portable rc
    ///                    that calls e.g. `atuin`/`starship` lights up.
    /// - `host-parity`  — reproduce the host nix closure so your EXACT dotfiles
    ///                    work unchanged (cache-first; experimental). Heavy; meant
    ///                    for long-lived boxes, set per-env via
    ///                    `[env.<name>.sandbox.home] strategy = "host-parity"`.
    pub enum ShellStrategy: "shell strategy" {
        Clean = "clean",
        Portable = "portable",
        ToolParity = "tool-parity" | "tool_parity" | "toolparity",
        HostParity = "host-parity" | "host_parity" | "hostparity",
    } default = Portable;
}

impl SandboxProfile {
    /// Mount the container root filesystem read-only (writable: the worktree,
    /// cache binds, and a tmpfs `/tmp`).
    pub fn read_only_root(self) -> bool {
        matches!(
            self,
            SandboxProfile::Hardened | SandboxProfile::Sealed | SandboxProfile::SealedTunnel
        )
    }
    /// Set `no-new-privileges` so setuid/setgid binaries can't escalate.
    pub fn no_new_privileges(self) -> bool {
        matches!(
            self,
            SandboxProfile::Hardened | SandboxProfile::Sealed | SandboxProfile::SealedTunnel
        )
    }
    /// Cap the number of processes (fork-bomb containment); `None` = unlimited.
    pub fn pids_limit(self) -> Option<i64> {
        match self {
            SandboxProfile::Open => None,
            SandboxProfile::Hardened => Some(512),
            SandboxProfile::Sealed | SandboxProfile::SealedTunnel => Some(256),
        }
    }
    /// Linux capabilities to drop. `sealed`/`sealed-tunnel` drop everything;
    /// `hardened` leaves the runtime's defaults so debuggers (ptrace), `ping`
    /// (NET_RAW), and low-port binds keep working. Under `sealed-tunnel` the
    /// worktree still drops ALL caps — the tunnel's NET_ADMIN/TUN live in the
    /// VPN sidecar, never in the worktree container.
    pub fn drop_capabilities(self) -> Vec<String> {
        match self {
            SandboxProfile::Sealed | SandboxProfile::SealedTunnel => vec!["ALL".to_string()],
            _ => Vec::new(),
        }
    }
    /// Capabilities to add back after dropping (reserved for future tuning).
    pub fn add_capabilities(self) -> Vec<String> {
        Vec::new()
    }
    /// Force `network=none` regardless of the configured network mode. Both
    /// `sealed` and `sealed-tunnel` impose this floor; for `sealed-tunnel` a
    /// resolved VPN attachment lifts it back up to a tunnel-only route (handled
    /// in `sandbox::resolve_scoped`, not here), so with no VPN it stays sealed.
    pub fn forces_no_network(self) -> bool {
        matches!(self, SandboxProfile::Sealed | SandboxProfile::SealedTunnel)
    }
    /// Whether this profile permits a VPN attachment. Plain `sealed` refuses
    /// (its contract is "no network, no caps"); `sealed-tunnel` is the explicit
    /// posture for "locked down, but on its own tunnel". Non-sealed profiles
    /// permit it freely.
    pub fn permits_vpn(self) -> bool {
        !matches!(self, SandboxProfile::Sealed)
    }
}
config_enum! {
    /// What to do when no sandbox backend is available.
    pub enum OnMissing: "on_missing" {
        Warn = "warn", Prompt = "prompt", Fail = "fail",
    } default = Warn;
}
config_enum! {
    /// Interactive remote transport (the control plane always uses ssh).
    pub enum RemoteTransport: "remote transport" {
        Mosh = "mosh", Ssh = "ssh",
    } default = Mosh;
}
config_enum! {
    /// Where a remote worktree lives.
    pub enum RemoteMode: "remote mode" {
        Remote = "remote", LocalExec = "local_exec", Sshfs = "sshfs",
    } default = Remote;
}
config_enum! {
    /// Where a named environment's processes run (its [`crate::placement`]).
    /// `local` runs on the host; `ssh`/`mosh` over ssh; `k8s` inside a pod via
    /// `kubectl exec`; `provider` through a managed-sandbox provider (Daytona, …).
    pub enum PlacementMode: "placement" {
        Local = "local" | "host",
        Ssh = "ssh" | "mosh" | "remote",
        K8s = "k8s" | "kubernetes" | "kube",
        Provider = "provider",
    } default = Local;
}
config_enum! {
    /// Where an environment's worktree files physically live. `in_env` (the
    /// default) keeps files where the env runs — on the host for `local`, in the
    /// pod/sandbox for remote placements (today's behavior). `local_exec` keeps
    /// files on the host and only execs remotely. `sshfs` FUSE-mounts the remote
    /// tree locally; `sync` keeps a local working copy kept coherent with the
    /// remote via a changed-files (rsync) delta. Both `sshfs`/`sync` run the pane
    /// *locally at the mountpoint* (the placement is used only to project the
    /// tree); their mount/sync lifecycle is auto-run on pane spawn/close.
    pub enum DataMode: "data mode" {
        InEnv = "in_env" | "remote" | "native",
        LocalExec = "local_exec" | "local",
        Sshfs = "sshfs" | "mount",
        Sync = "sync" | "rsync",
    } default = InEnv;
}
config_enum! {
    /// Default log verbosity (the `SUPERZEJ_LOG` env filter can refine it
    /// per-module). Maps to a `tracing` level.
    pub enum LogLevel: "log level" {
        Error = "error", Warn = "warn", Info = "info", Debug = "debug", Trace = "trace",
    } default = Info;
}
config_enum! {
    /// Log file encoding.
    pub enum LogFormat: "log format" {
        Text = "text", Json = "json",
    } default = Text;
}
config_enum! {
    /// Where a configured pin appears when opened.
    pub enum PinLocation: "pin location" {
        Tab = "tab",
        Layout = "layout" | "pane" | "active_layout" | "active-layout",
        Strip = "strip" | "top" | "top-strip" | "top_strip",
        Float = "float" | "floating" | "scratch",
        Corner = "corner",
    } default = Tab;
}

config_enum! {
    /// Which screen corner a `location = "corner"` pin docks in.
    pub enum PinCorner: "pin corner" {
        TopLeft = "top_left" | "top-left" | "tl",
        TopRight = "top_right" | "top-right" | "tr",
        BottomLeft = "bottom_left" | "bottom-left" | "bl",
        BottomRight = "bottom_right" | "bottom-right" | "br",
    } default = BottomRight;
}

config_enum! {
    /// Whether a pin is global (all workspaces) or workspace-scoped.
    pub enum PinScope: "pin scope" {
        Global = "global" | "everywhere" | "all",
        Workspace = "workspace" | "local",
    } default = Global;
}

config_enum! {
    /// How the LLM proxy chooses among a route's backends. Milestone 1 implements
    /// `sequential` (ordered failover); the others are reserved for the AR
    /// intelligent-routing work (cost-aware tiering, speculative cascade).
    pub enum RoutingStrategy: "routing strategy" {
        Sequential = "sequential" | "failover" | "ordered",
        LoadBalanced = "load_balanced" | "balanced",
        Speculative = "speculative" | "cascade",
    } default = Sequential;
}

config_enum! {
    /// Token-reduction aggressiveness for in-flight tool-output compression
    /// (group W). `conservative` is lossless-ish (ANSI/progress/blank-line
    /// cleanup); higher levels add repeated-line/JSON/whitespace folding and
    /// head/tail truncation.
    pub enum CompressionLevel: "compression level" {
        Off = "off" | "none",
        Conservative = "conservative",
        Balanced = "balanced",
        Aggressive = "aggressive",
    } default = Conservative;
}

/// `[llm_proxy]` — the AI-traffic chokepoint daemon (`szproxy`). The shell never
/// hard-depends on this; AI is strictly additive, so the default is disabled.
/// When `enabled`, the host launches `szproxy` as a pinned daemon and agents
/// point their `OPENAI_BASE_URL`/`ANTHROPIC_BASE_URL` at `listen`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LlmProxyConfig {
    /// Whether the host should launch + manage the proxy daemon.
    pub enabled: bool,
    /// Address the daemon binds (and agents target).
    pub listen: String,
    /// Backend selection strategy.
    pub routing: RoutingStrategy,
    /// On a budget-cap breach, refuse the request (`true`) or downgrade to a
    /// cheaper tier (`false`). The kill-switch always refuses.
    pub refuse_on_breach: bool,
    /// Path to the proxy's routes document (JSON), passed to `szproxy` as
    /// `SZPROXY_CONFIG`. Empty means no backends are configured yet.
    pub config_path: String,
    /// Streaming: seconds to wait for a backend's first usable output before
    /// falling through (TTFB / empty-completion peek window).
    pub first_byte_timeout_secs: u64,
    /// Streaming: seconds of upstream silence after which a committed stream is
    /// terminated.
    pub idle_timeout_secs: u64,
    /// Streaming: keep-alive cadence (seconds) emitted during upstream silence.
    pub heartbeat_secs: u64,
    /// In-flight token reduction: compress noisy `tool` output before it's sent
    /// upstream (group W). Off by default — AI transforms are opt-in.
    pub token_reduction: bool,
    /// Aggressiveness when `token_reduction` is on.
    pub token_reduction_level: CompressionLevel,
    /// Route a launched agent's model traffic through the proxy at `listen` by
    /// injecting provider config into the agent's environment at spawn. Separate
    /// from `enabled` (which launches `szproxy`): set this to point the agent at
    /// an already-running proxy without launching our own. This governs the
    /// `SUPERZEJ_PROXY_*` vars the pi extension reads — NOT `ANTHROPIC_BASE_URL`
    /// (see `route_claude`).
    pub route_agent: bool,
    /// Additionally route Claude Code / the Anthropic SDK (anything honoring
    /// `ANTHROPIC_BASE_URL`) through the proxy. Off by default: claude talks to
    /// Anthropic directly so a proxy/tunnel hiccup can't break it (a bare
    /// `ANTHROPIC_BASE_URL = http://127.0.0.1:<proxy>` with a down tunnel yields
    /// `ConnectionRefused` and has no upstream fallback). Only meaningful when
    /// `route_agent` is also on. The pi extension routes regardless via
    /// `SUPERZEJ_PROXY_*`; this switch is specifically for the `ANTHROPIC_*` vars.
    pub route_claude: bool,
    /// The pi-side API id for the proxy endpoint. The proxy serves the Anthropic
    /// Messages API (`/v1/messages`); pi's OpenAI client speaks the Responses API,
    /// which the proxy does not implement — so `anthropic-messages` is the default.
    pub agent_api: String,
    /// The model id the agent requests from the proxy (the proxy maps it to a
    /// real backend, e.g. `model-proxy/standard` → its standard route).
    pub agent_model: String,
    /// "The bouncer": run a launched agent inside its sealed `agent_profile`
    /// container, route its built-in `bash`/`read`/`edit`/`write` tools back
    /// through superzej over a bind-mounted unix-socket ACP channel, and gate
    /// the consequential ones (shell + edit + write) behind an interactive
    /// allow/deny overlay. Off by default — the additive integration (pi runs
    /// its own tools in-process, edits auto-apply) stays the default. When the
    /// resolved `agent_profile` forces no network (`sealed`), the agent's model
    /// traffic is relayed to the proxy over a unix socket too (full egress seal);
    /// otherwise it reaches the proxy via the container gateway.
    pub bouncer: bool,
    /// Base URL an agent running INSIDE a remote/provider sandbox (a sprite VM
    /// that can't reach host loopback) uses to reach `szproxy` — a tunnel/public
    /// endpoint, e.g. `https://proxy.example.ts.net`. When set (and `route_agent`),
    /// superzej injects `ANTHROPIC_BASE_URL` + the per-worktree virtual key into
    /// the provider exec env so ANY agent there (pi, claude code, …) routes
    /// through the proxy by default. Empty ⇒ no remote proxy injection (the
    /// in-sprite agent would talk to the upstream model directly with its key).
    pub remote_base_url: String,
}

impl Default for LlmProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "127.0.0.1:8383".to_string(),
            routing: RoutingStrategy::default(),
            refuse_on_breach: true,
            config_path: String::new(),
            first_byte_timeout_secs: 45,
            idle_timeout_secs: 120,
            heartbeat_secs: 10,
            token_reduction: false,
            token_reduction_level: CompressionLevel::default(),
            route_agent: false,
            route_claude: false,
            agent_api: "anthropic-messages".to_string(),
            agent_model: "model-proxy/standard".to_string(),
            bouncer: false,
            remote_base_url: String::new(),
        }
    }
}

impl LlmProxyConfig {
    /// Env vars for an agent/shell running inside a REMOTE/provider sandbox so its
    /// model traffic routes through `szproxy` by default. Empty unless `route_agent`
    /// is on; the loopback URL is then reachable via the reverse tunnel superzej
    /// stands up (empty/`auto` `remote_base_url`) or via an explicit external URL.
    /// Sets the `SUPERZEJ_PROXY_*` vars the pi extension reads. `virtual_key`, when
    /// given, becomes the proxy auth key (else the passthrough master key is used).
    /// Only when `route_claude` is also on does it additionally set
    /// `ANTHROPIC_BASE_URL` (+ the virtual key as `ANTHROPIC_API_KEY`) so claude
    /// code / the Anthropic SDK route through the proxy too; by default they talk
    /// to Anthropic directly (a down proxy tunnel can't break claude).
    pub fn remote_agent_env(&self, virtual_key: Option<&str>) -> Vec<(String, String)> {
        let url = match self.remote_base_url() {
            Some(u) => u,
            None => return Vec::new(),
        };
        let mut v = vec![
            ("SUPERZEJ_PROXY_BASE_URL".to_string(), url.clone()),
            ("SUPERZEJ_PROXY_API".to_string(), self.agent_api.clone()),
            ("SUPERZEJ_PROXY_MODEL".to_string(), self.agent_model.clone()),
        ];
        if self.route_claude {
            v.push(("ANTHROPIC_BASE_URL".to_string(), url));
        }
        if let Some(k) = virtual_key.map(str::trim).filter(|k| !k.is_empty()) {
            v.push(("SUPERZEJ_PROXY_KEY".to_string(), k.to_string()));
            if self.route_claude {
                v.push(("ANTHROPIC_API_KEY".to_string(), k.to_string()));
            }
        }
        v
    }

    /// Env for an agent running ON THE HOST (not a sandbox) so its model traffic
    /// routes through the proxy over the LOCAL `listen` loopback directly — no
    /// tunnel, no relay. Mirrors [`remote_agent_env`](Self::remote_agent_env) but
    /// always targets `http://127.0.0.1:<listen-port>`, so it stays correct even
    /// when `remote_base_url` points at an external endpoint used for *remote*
    /// sandboxes. Sets the `SUPERZEJ_PROXY_*` vars the pi extension reads, and —
    /// only when `route_claude` is also on — `ANTHROPIC_BASE_URL`
    /// (claude/codex/Anthropic SDK). Empty unless `route_agent`; no auth key (the
    /// pi extension falls back to its default), matching the keyless sprite path.
    /// See [`crate::config::LlmProxyConfig`].
    pub fn local_agent_env(&self) -> Vec<(String, String)> {
        if !self.route_agent {
            return Vec::new();
        }
        let url = format!("http://127.0.0.1:{}", self.listen_port());
        let mut v = vec![
            ("SUPERZEJ_PROXY_BASE_URL".to_string(), url.clone()),
            ("SUPERZEJ_PROXY_API".to_string(), self.agent_api.clone()),
            ("SUPERZEJ_PROXY_MODEL".to_string(), self.agent_model.clone()),
        ];
        if self.route_claude {
            v.push(("ANTHROPIC_BASE_URL".to_string(), url));
        }
        v
    }

    /// The proxy base URL an in-remote agent should use, or `None` if remote
    /// routing is off. `remote_base_url = "auto"` ⇒ the in-sandbox reverse tunnel
    /// at `http://127.0.0.1:<proxy-port>` (superzej stands the tunnel up); an
    /// explicit URL is used verbatim. `None` unless `route_agent` + a value set.
    pub fn remote_base_url(&self) -> Option<String> {
        if !self.route_agent {
            return None;
        }
        let url = self.remote_base_url.trim();
        // `route_agent` alone is the single switch: an empty (or explicit "auto")
        // `remote_base_url` resolves to the in-sandbox reverse tunnel at
        // `http://127.0.0.1:<proxy-port>`. An explicit URL is used verbatim.
        if url.is_empty() || url == "auto" {
            Some(format!("http://127.0.0.1:{}", self.listen_port()))
        } else {
            Some(url.to_string())
        }
    }

    /// The loopback port the in-sandbox reverse tunnel should listen on (so the
    /// injected `ANTHROPIC_BASE_URL` resolves), or `None` unless `route_agent` +
    /// `remote_base_url = "auto"`. The host starts a tunnel on this port that
    /// dials the real `szproxy`.
    pub fn remote_tunnel_port(&self) -> Option<u16> {
        // The tunnel is needed whenever the resolved base URL is the loopback
        // (empty or "auto" under `route_agent`); an explicit external URL needs no
        // tunnel.
        let url = self.remote_base_url.trim();
        (self.route_agent && (url.is_empty() || url == "auto")).then(|| self.listen_port())
    }

    /// The port from `listen` (e.g. `127.0.0.1:8383` → 8383; 8383 on parse fail).
    pub fn listen_port(&self) -> u16 {
        self.listen
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(8383)
    }

    /// The launch spec for the `szproxy` daemon — `(program, args, env)` — or
    /// `None` when the proxy is disabled. The host feeds this to its process
    /// supervisor (e.g. as a `restart = "always"` pinned daemon). `SZPROXY_LISTEN`
    /// and `SZPROXY_CONFIG` mirror the standalone env knobs the daemon reads.
    ///
    /// Launching is gated on `enabled` ONLY — orthogonal to `route_agent`. This
    /// lets `route_agent` point agents at an EXTERNAL proxy already listening on
    /// `listen` (e.g. a separate model-proxy) without superzej trying to bind the
    /// same port and colliding. Run superzej's own szproxy with `enabled = true`.
    pub fn launch_spec(&self) -> Option<(String, Vec<String>, BTreeMap<String, String>)> {
        if !self.enabled {
            return None;
        }
        let mut env = BTreeMap::new();
        env.insert("SZPROXY_LISTEN".to_string(), self.listen.clone());
        if !self.config_path.is_empty() {
            env.insert("SZPROXY_CONFIG".to_string(), self.config_path.clone());
        }
        env.insert(
            "SZPROXY_FIRST_BYTE_TIMEOUT".to_string(),
            self.first_byte_timeout_secs.to_string(),
        );
        env.insert(
            "SZPROXY_STREAM_IDLE_TIMEOUT".to_string(),
            self.idle_timeout_secs.to_string(),
        );
        env.insert(
            "SZPROXY_STREAM_HEARTBEAT_INTERVAL".to_string(),
            self.heartbeat_secs.to_string(),
        );
        env.insert(
            "SZPROXY_COMPRESS".to_string(),
            if self.token_reduction { "1" } else { "0" }.to_string(),
        );
        env.insert(
            "SZPROXY_COMPRESS_LEVEL".to_string(),
            self.token_reduction_level.as_str().to_string(),
        );
        env.insert(
            "SZPROXY_ROUTING".to_string(),
            self.routing.as_str().to_string(),
        );
        Some(("szproxy".to_string(), Vec::new(), env))
    }
}

config_enum! {
    /// `[merge_queue] conflict_handoff` — what happens to a branch that the fold
    /// can't land cleanly. `"agent"` (default) dispatches the worktree's agent to
    /// rebase onto the new `main` and re-resolve; `"notify"` just raises a
    /// notification and leaves it queued; `"manual"` leaves it silently queued.
    pub enum ConflictHandoff: "conflict handoff" {
        Agent = "agent",
        Notify = "notify",
        Manual = "manual" | "off" | "none",
    } default = Agent;
}

/// `[merge_queue]` — the local "fold-actor": fold parallel worktree branches into
/// `target_branch` in the object database (no checkout), auto-landing every
/// branch that merges clean and deferring only genuine conflicts. On by default;
/// the core (fold + integrate) is AI-free — the conflict handoff is the only AI
/// touch and only fires on a deferral. See `superzej_core::fold` for the pure
/// engine and the host `integrate` runner for the I/O (test-gate + CAS).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MergeQueueConfig {
    /// Master switch. When off, the `integrate` command and auto-drain are inert.
    pub enabled: bool,
    /// Branch the fold advances. `"auto"` resolves it per repo (HEAD/default).
    pub target_branch: String,
    /// Shell command run on the folded tip (in a throwaway worktree) to gate the
    /// CAS-advance — the "does the union build?" check. Empty disables the gate
    /// (textual-clean only). Example: `just ci` or `cargo test --workspace`.
    pub gate_command: String,
    /// Whether to run `gate_command` at all. Off ⇒ advance as soon as branches
    /// merge without text conflicts.
    pub gate_on: bool,
    /// On a red gate, re-land branches incrementally to find the offender and
    /// defer just that one, instead of failing the whole batch.
    pub bisect_on_red: bool,
    /// Fold automatically when an agent signals done (ACP `AgentEnd` / dispatch
    /// → merged), so a burst of completions drains the queue without a keystroke.
    pub auto_drain: bool,
    /// Auto-commit uncommitted worktree work before folding (a branch must be a
    /// commit for `merge-tree`). Off ⇒ only committed branch tips are folded and
    /// dirty worktrees are skipped with a warning.
    pub snapshot_dirty: bool,
    /// Conflicting paths confined to these (matched by exact path or basename,
    /// e.g. `Cargo.lock` matches `crates/x/Cargo.lock`) are classified
    /// regenerable — resolved by regenerating, not handed to a human.
    pub regenerate_paths: Vec<String>,
    /// Command run (in a throwaway worktree, cwd = repo) to rebuild the
    /// `regenerate_paths` artifacts when a branch's *only* conflicts are in them,
    /// turning that defer into an automatic land. Empty disables regeneration
    /// (regenerable conflicts just defer). E.g. `cargo update --workspace`.
    pub regenerate_command: String,
    /// What to do with a deferred (conflicting) branch.
    pub conflict_handoff: ConflictHandoff,
}

impl Default for MergeQueueConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            target_branch: "auto".to_string(),
            gate_command: String::new(),
            gate_on: true,
            bisect_on_red: true,
            auto_drain: true,
            snapshot_dirty: false,
            regenerate_paths: vec!["Cargo.lock".to_string()],
            regenerate_command: String::new(),
            conflict_handoff: ConflictHandoff::default(),
        }
    }
}

/// `[replay]` — per-pane time-travel recording. Every byte a pane emits is
/// appended to a bounded in-memory ring with periodic keyframe markers, so the
/// user can scrub a pane's history like a video (`Alt+r`) and search for any
/// string that ever appeared on screen — including inside full-screen apps
/// (vim/htop) whose output never reaches scrollback. On by default; bounded by
/// both a byte and a duration budget. When `enabled = false` no ring is
/// allocated and `PtyPane::feed` does a single null check (free when off).
/// Distinct from the whole-session asciinema `Recorder` (`Ctrl+Alt+r`).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ReplayConfig {
    /// Master switch. Off ⇒ no recording ring, zero allocation.
    pub enabled: bool,
    /// Per-pane byte budget for the ring; oldest events (and any keyframe whose
    /// byte range no longer exists) are evicted past this.
    pub max_bytes_per_pane: u64,
    /// Per-pane duration budget in seconds; events older than this are evicted.
    pub max_duration_secs: u64,
    /// Capture a keyframe marker after this many ms of activity …
    pub keyframe_interval_ms: u64,
    /// … or after this many bytes, whichever comes first.
    pub keyframe_interval_bytes: u64,
    /// During playback, a gap larger than this (ms) between recorded events is
    /// collapsed to a short constant so idle stretches don't stall the scrub.
    pub idle_threshold_ms: u64,
    /// Mirror each pane's ring to `$XDG_STATE_HOME/superzej/replay/<session>/
    /// <pane>.szr` on an off-loop writer thread, so scrubbing reaches into the
    /// previous run after a restart. Off by default — the one feature with real
    /// disk cost.
    pub persist: bool,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes_per_pane: 8 * 1024 * 1024,
            max_duration_secs: 1800,
            keyframe_interval_ms: 4000,
            keyframe_interval_bytes: 262144,
            idle_threshold_ms: 1000,
            persist: false,
        }
    }
}

config_enum! {
    /// `[media] backend` — how superzej talks to your player. `"auto"` (the
    /// default) picks the right backend for the current OS: Linux → MPRIS,
    /// Windows → SMTC, macOS → AppleScript. `"none"` disables. `"mpris"` is the
    /// Linux D-Bus standard (native `zbus`, `playerctl` CLI fallback) covering
    /// Spotify desktop, mpv, ncspot, spotify-player, musikcube, moc, VLC, cmus, …
    /// `"mpv"` drives a single mpv instance over its JSON IPC socket. `"smtc"` is
    /// the Windows System Media Transport Controls session manager.
    /// `"applescript"` drives macOS Music.app + Spotify via `osascript`.
    /// `"jellyfin"` is reserved. A backend selected on the wrong OS is inert.
    pub enum MediaBackendKind: "media backend" {
        Auto = "auto",
        None = "none" | "off",
        Mpris = "mpris" | "dbus" | "playerctl",
        Mpv = "mpv",
        Smtc = "smtc" | "windows" | "gsmtc",
        AppleScript = "applescript" | "macos" | "osascript",
        Jellyfin = "jellyfin",
    } default = Auto;
}

config_enum! {
    /// The action bound to a bare "media" key / the play-pause toggle's sibling.
    /// Mostly informational today (each transport op has its own action id); kept
    /// so a single configurable "media key" can be wired later.
    pub enum MediaDefaultAction: "media default action" {
        PlayPause = "play_pause" | "toggle",
        Next = "next",
        Previous = "previous" | "prev",
        VolumeUp = "volume_up",
        VolumeDown = "volume_down",
    } default = PlayPause;
}

/// `[media]` — media-player control. The shell never depends on this; it is
/// strictly additive. Defaults on with the `mpris` backend, which degrades to
/// inert (no badge, no watcher) wherever D-Bus and `playerctl` are both absent.
/// When `enabled`, the host resolves the configured `backend` and surfaces a
/// now-playing statusbar badge, a panel section, transport keybinds, and
/// playlist/player pickers. Only the selected backend's sub-table is consulted.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MediaConfig {
    /// Master switch. `false` ⇒ no watcher, no badge, no panel section, and all
    /// `media-*` actions are inert.
    pub enabled: bool,
    /// Which control backend to use.
    pub backend: MediaBackendKind,
    /// Preferred players (bus-name tails, e.g. `["spotify", "mpv"]`) when several
    /// expose MPRIS at once; the first match wins. Empty ⇒ pick the first player
    /// that is actively playing, else the first available.
    pub players_priority: Vec<String>,
    /// Reserved single-"media key" action (each transport op already has its own
    /// bindable action id).
    pub default_action: MediaDefaultAction,
    /// Volume step (0.0..=1.0) applied by `media-volume-up`/`-down`.
    pub volume_step: f64,
    /// Fallback poll cadence (seconds) for backends without a push-signal stream
    /// (mpv IPC / `playerctl`). The native MPRIS path uses D-Bus signals instead,
    /// so this never fires for it (preserving the ~0%-idle contract).
    pub poll_interval_secs: u64,
    pub mpv: MpvMediaConfig,
}

impl Default for MediaConfig {
    fn default() -> Self {
        MediaConfig {
            enabled: true,
            backend: MediaBackendKind::Auto,
            players_priority: Vec::new(),
            default_action: MediaDefaultAction::PlayPause,
            volume_step: 0.05,
            poll_interval_secs: 3,
            mpv: MpvMediaConfig::default(),
        }
    }
}

impl MediaConfig {
    /// Lower this config into the backend-resolution input the `superzej-media`
    /// leaf consumes (the leaf must not depend on core). When disabled the
    /// backend maps to `None`, so `superzej_media::client_for` stays inert.
    pub fn resolve_opts(&self) -> superzej_media::ResolveOpts {
        use superzej_media::BackendKind;
        let backend = if !self.enabled {
            BackendKind::None
        } else {
            match self.backend {
                MediaBackendKind::Auto => BackendKind::Auto,
                MediaBackendKind::None => BackendKind::None,
                MediaBackendKind::Mpris => BackendKind::Mpris,
                MediaBackendKind::Mpv => BackendKind::Mpv,
                MediaBackendKind::Smtc => BackendKind::Smtc,
                MediaBackendKind::AppleScript => BackendKind::AppleScript,
                MediaBackendKind::Jellyfin => BackendKind::Jellyfin,
            }
        };
        superzej_media::ResolveOpts {
            backend,
            players_priority: self.players_priority.clone(),
            mpv_socket: self.mpv.socket.clone(),
        }
    }
}

/// `[media.mpv]` — mpv JSON-IPC backend. Point `socket` at the path mpv was
/// launched with via `--input-ipc-server=<path>`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MpvMediaConfig {
    pub socket: String,
}

impl Default for MpvMediaConfig {
    fn default() -> Self {
        MpvMediaConfig {
            socket: "/tmp/mpvsocket".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NamedCommand {
    pub name: String,
    pub command: String,
    /// Optional list of hint overrides for the statusbar when this tool is focused.
    #[serde(default)]
    pub hints: Vec<CommandHint>,
    /// Optional account-provider id (`"codex"`, `"claude"`) for client-side
    /// account switching. When unset, the provider is inferred from the
    /// command's program basename. See [`crate::account`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// A statusbar hint override for a specific tool.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommandHint {
    pub key: String,
    pub label: String,
}

/// A `[[accounts]]` entry — one credential home for a coding-agent provider
/// (`codex`/`claude`), used by client-side account switching. superzej points
/// the agent's credential-home env var (`CODEX_HOME` / `CLAUDE_CONFIG_DIR`) at
/// the chosen account on launch, so the user's real `~/.codex` / `~/.claude` is
/// never modified. `dir` omitted ⇒ superzej manages the dir under the state dir
/// (use "Add account" to log in); `dir` set ⇒ adopt an existing login dir.
/// See [`crate::account`].
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Account {
    pub name: String,
    pub provider: String,
    /// Credential home directory (`~` expanded). When absent, superzej manages a
    /// dir at `$XDG_STATE_HOME/superzej/accounts/<provider>/<slug>/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// A `[[pins]]` entry — a named program that opens either as its own session
/// tab (`location = "tab"`, the default), as a tiled pane in the active layout
/// (`location = "layout"`), or as a small bordered overlay docked in a screen
/// **corner** (`location = "corner"`, e.g. a `mpv --vo=tct` video player in the
/// bottom-right). Pins are summoned via `Alt-1..9` / the tabbar's pin chips, and
/// can be global (all workspaces) or workspace-scoped. See `src/commands/pin.rs`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Pin {
    pub name: String,
    pub command: String,
    /// Explicit argv. When non-empty it is launched directly (no shell); when
    /// empty, `command` is run via the login shell (`sh -lc`).
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory for the pin's pane. Tab pins default to `$HOME`; layout
    /// pins default to the focused repo/worktree when it can be resolved.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Where the pin appears when opened.
    #[serde(default)]
    pub location: PinLocation,
    /// Whether the pin is global (all workspaces) or workspace-scoped.
    #[serde(default)]
    pub scope: PinScope,
    /// Which workspace this pin belongs to (only used when `scope = "workspace"`).
    #[serde(default)]
    pub workspace: Option<String>,
    /// When to start this pin: "lazy" (on first access) or "eager" (when session starts).
    #[serde(default)]
    pub start: PinStart,
    /// When to restart this pin after it exits.
    #[serde(default)]
    pub restart: PinRestart,
    /// Whether to allow multiple instances or enforce singleton behavior.
    #[serde(default = "default_true")]
    pub singleton: bool,
    /// Per-program environment variables injected into the pin's process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Display label override for the strip/chip (defaults to `name`).
    #[serde(default)]
    pub label: Option<String>,
    /// Relative weight of this pin within the top strip (defaults to 1.0).
    #[serde(default)]
    pub ratio: Option<f32>,
    /// Which screen corner a `location = "corner"` pin docks in.
    #[serde(default)]
    pub corner: PinCorner,
    /// Corner-pin width: a percentage of screen columns (`"30%"`) or an absolute
    /// column count (`"40"`). Defaults to ~30% when unset.
    #[serde(default)]
    pub corner_width: Option<String>,
    /// Corner-pin height: a percentage of screen rows (`"30%"`) or an absolute
    /// row count (`"12"`). Defaults to ~30% when unset.
    #[serde(default)]
    pub corner_height: Option<String>,
}

impl Pin {
    /// The label shown on the strip/chip (falls back to `name`).
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }

    /// This pin's strip weight (defaults to 1.0; non-positive values clamp to 1.0).
    pub fn strip_weight(&self) -> f32 {
        match self.ratio {
            Some(r) if r > 0.0 => r,
            _ => 1.0,
        }
    }
}

/// When to start a pin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PinStart {
    #[default]
    Lazy,
    Eager,
}

/// When to restart a pin after it exits.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum PinRestart {
    #[default]
    Never,
    Always,
    OnFailure,
}

/// A general task kind. Tests are the first first-class consumer; other kinds
/// feed Problems, Timeline, and Search Everywhere without inventing a second
/// command model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskKind {
    #[default]
    Custom,
    Test,
    Build,
    Lint,
    Run,
}

/// A `[[tasks]]` entry — a named command that can be run from the host and whose
/// output can be parsed by feature-specific consumers (Tests, Problems, Timeline).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Task {
    pub name: String,
    pub command: String,
    /// Extra argv fragments appended to `command` by the host runner.
    #[serde(default)]
    pub args: Vec<String>,
    /// Working directory, relative to the worktree when not absolute.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Environment overrides for the task process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Kind-specific routing; `test` tasks feed the Tests panel.
    #[serde(default)]
    pub kind: TaskKind,
    /// Optional output/discovery matcher (`cargo-test`, `pytest`, `go-test`, ...).
    #[serde(default)]
    pub matcher: Option<String>,
    /// Scope label (`worktree`, `workspace`, or custom); currently informational.
    #[serde(default)]
    pub scope: Option<String>,
}

/// A `[[worktree_templates]]` entry — a reusable preset applied when creating a
/// worktree (item 54): base branch + branch prefix + sandbox/agent defaults,
/// plus an optional initial pane layout and pins to start.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema, Default)]
#[serde(default)]
pub struct WorktreeTemplate {
    /// Template name (shown in the new-worktree wizard's template picker).
    pub name: String,
    /// Base-branch override (empty = the configured/auto-resolved base).
    pub base: Option<String>,
    /// Branch-prefix override for worktrees created from this template.
    pub branch_prefix: Option<String>,
    /// Sandbox backend to pre-select (`podman`/`docker`/`bwrap`/`none`).
    pub sandbox: Option<String>,
    /// Agent to pre-select (e.g. `claude`, or `shell` for none).
    pub agent: Option<String>,
    /// Pin names (from `[[pins]]`) to start in the new worktree.
    pub pins: Vec<String>,
    /// A saved named layout (item 115) to apply as the initial pane layout.
    /// Takes precedence over `commands`.
    pub layout: Option<String>,
    /// Shorthand initial layout: an even split running each command (a `None`
    /// command — empty string — is a plain shell). Ignored when `layout` is set.
    pub commands: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// A user-defined keybind action (`[[actions]]`): a chord bound to either a
/// shell command (`run`) or a built-in composite operation (`action` +
/// `params`), optionally surfaced in the Cmd+K menu. Exactly one of `run` /
/// `action` must be set; see `src/keymap.rs`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CustomAction {
    /// Stable id + default menu/hint label.
    pub name: String,
    /// Key chord (e.g. "Alt D"); validated by the host keymap.
    pub key: String,
    /// Shell command line run via `sh -c`. Mutually exclusive with `action`.
    #[serde(default)]
    pub run: Option<String>,
    /// Name of a built-in composite operation (e.g. `new-worktree`,
    /// `new-pane`) to invoke in-process. Mutually exclusive with `run`.
    #[serde(default)]
    pub action: Option<String>,
    /// String parameters for `action` (e.g. `sandbox`, `agent`, `name`).
    /// Meaningless without `action`; the host keymap validates the keys.
    #[serde(default)]
    pub params: std::collections::BTreeMap<String, String>,
    /// Show in the command palette.
    #[serde(default)]
    pub menu: bool,
    /// Short statusbar hint (defaults to `name`).
    #[serde(default)]
    pub hint: Option<String>,
    #[serde(default = "default_true")]
    pub floating: bool,
    #[serde(default = "default_true")]
    pub close_on_exit: bool,
}

config_enum! {
    /// Where a git custom command's output goes: discarded, shown in a
    /// popup overlay, or run in a floating terminal pane.
    pub enum GitCmdOutput: "git command output" {
        None = "none", Popup = "popup", Terminal = "terminal",
    } default = Popup;
}

/// An input collected before a git custom command runs; the response is
/// referenced in the command template as `{{ .Form.<key> }}`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GitPrompt {
    /// Template lookup key (`{{ .Form.<key> }}`).
    pub key: String,
    /// Prompt title shown to the user (defaults to `key`).
    #[serde(default)]
    pub title: Option<String>,
    /// Prompt kind; only `input` exists today.
    #[serde(default = "default_prompt_kind", rename = "type")]
    pub kind: String,
}

fn default_prompt_kind() -> String {
    "input".into()
}

fn default_git_context() -> String {
    "global".into()
}

/// A user-defined git custom command (`[[git_commands]]`), lazygit-style: a
/// key reachable from the git panel's custom-commands menu, whose command
/// line expands `{{ .SelectedCommit.Sha }}`-style template variables against
/// the current selection (see `custom_cmd`).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GitCommand {
    /// Menu hotkey (single character) inside the custom-commands menu.
    pub key: String,
    /// Which git view offers it: `commits`, `branches`, `files`, `stash`,
    /// or `global` (every git view).
    #[serde(default = "default_git_context")]
    pub context: String,
    /// Shell command template, run via `sh -c` after expansion.
    pub command: String,
    /// Menu label (defaults to the command text).
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub output: GitCmdOutput,
    #[serde(default)]
    pub prompts: Vec<GitPrompt>,
}

/// UI/Presentation settings (`[ui]`).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct UiConfig {
    /// Language code (e.g. "en-US", "ja-JP"). "auto" to detect from system.
    pub language: String,
    /// Ask before destructive worktree actions (deleting a worktree from disk via the sidebar).
    pub confirm_delete_workspace: bool,
    /// Whether to display the full word for the mode chip (e.g., "Normal" instead of "N").
    pub full_mode_chip: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: "auto".to_string(),
            confirm_delete_workspace: true,
            full_mode_chip: true,
        }
    }
}

/// Git behavior knobs for the panel's write operations (`[git]`).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct GitConfig {
    /// Pass `-c commit.gpgSign=false -c tag.gpgSign=false` to
    /// history-rewriting operations (rebase, amend, cherry-pick) so a gpg
    /// passphrase prompt can never hang a background op. Off by default: a
    /// working gpg-agent signs headlessly.
    pub override_gpg: bool,
    /// Install a `pre-merge-commit` hook that refuses a `git merge` run against
    /// the canonical (primary) checkout from *inside* a superzej sandbox, where
    /// the canonical worktree's filesystem view can be incoherent and silently
    /// corrupt the merge. On by default; points at `szhost integrate` (an
    /// object-DB fold with no checkout). No-op outside a sandbox, in linked
    /// worktrees, and when a foreign hook is already present. Per-merge escape
    /// hatch: `SUPERZEJ_MERGE_GUARD_OFF=1`.
    pub merge_guard: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            override_gpg: false,
            merge_guard: true,
        }
    }
}

/// Host keybinding overrides. The flat `[keybinds]` table remains the
/// default/global layer for backwards compatibility; nested tables such as
/// `[keybinds.vim_normal]` override only the native host's named modes.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct KeybindConfig {
    /// Backwards-compatible flat `[keybinds] action-id = "Chord"` entries.
    #[serde(flatten)]
    pub normal: BTreeMap<String, String>,
    /// Native host vim-normal-mode overrides.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub vim_normal: BTreeMap<String, String>,
    /// Native host vim-insert-mode overrides.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub vim_insert: BTreeMap<String, String>,
    /// Native host emacs-mode overrides.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub emacs: BTreeMap<String, String>,
}

impl KeybindConfig {
    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        self.normal.insert(key, value)
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.normal.get(key)
    }

    pub fn iter(&self) -> std::collections::btree_map::Iter<'_, String, String> {
        self.normal.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.normal.is_empty()
            && self.vim_normal.is_empty()
            && self.vim_insert.is_empty()
            && self.emacs.is_empty()
    }
}

impl<'a> IntoIterator for &'a KeybindConfig {
    type Item = (&'a String, &'a String);
    type IntoIter = std::collections::btree_map::Iter<'a, String, String>;

    fn into_iter(self) -> Self::IntoIter {
        self.normal.iter()
    }
}

/// A named keybind profile (`[profiles.<name>]`). Selected by the top-level
/// `profile` key (or `SUPERZEJ_PROFILE` / `--profile`). A profile may set the
/// default native-host mode (`default_mode = "vim-normal" | "emacs" | "normal"`)
/// and carries its own [`KeybindConfig`] layer, applied under the global
/// `[keybinds]` table. The built-in `vim`/`emacs` presets live in `keymap.rs`;
/// a config-defined profile of the same name overrides the preset.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ProfileConfig {
    /// Native-host mode this profile starts in (`""` ⇒ leave at Normal).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub default_mode: String,
    /// Keybind overrides applied as this profile's layer.
    #[serde(skip_serializing_if = "KeybindConfig::is_empty")]
    pub keybinds: KeybindConfig,
    /// Sandbox policy overrides for this profile (network_allow, network_block,
    /// network_audit, env_passthrough, etc.). Applied after the global `[sandbox]`
    /// and before the repo-root overlay, so per-profile restrictions take effect
    /// without touching per-repo config.
    #[serde(skip_serializing_if = "SandboxOverlay::is_empty")]
    pub sandbox: SandboxOverlay,
    /// Notification routing overrides for this profile (item 427). Applied after
    /// the global `[notifications]` and before any repo-root overlay, so
    /// per-profile rules/DND/sound take effect without touching per-repo config.
    #[serde(skip_serializing_if = "NotificationsOverlay::is_empty")]
    pub notifications: NotificationsOverlay,
}

/// Per-workspace config (`[workspace.<slug>.keybinds]`), keyed by repo slug.
/// Layered above the global + profile keybinds but below a repo-root
/// `.superzej.*` overlay. See [`Config::effective_keybinds`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WorkspaceConfig {
    /// Keybind overrides applied when this workspace is focused.
    #[serde(skip_serializing_if = "KeybindConfig::is_empty")]
    pub keybinds: KeybindConfig,
    /// Default coding-agent account per provider for this workspace
    /// (`[workspace.<slug>] accounts = { codex = "work" }`). Maps a provider id
    /// to an account name; consulted below a worktree override and above the
    /// global active account. See [`crate::account`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub accounts: std::collections::BTreeMap<String, String>,
    /// Extra sandbox bind mounts for this workspace, same format as
    /// `[sandbox] mounts` (`"host"`, `"host:dest"`, `"host:dest:ro|rw|cache"`;
    /// `~` is expanded). These **extend** the global `[sandbox] mounts` (plus
    /// any profile / repo-root `.superzej.*` overlay) for every worktree of
    /// this workspace — the per-workspace half of "bind dirs by default".
    /// (`[workspace.<slug>] sandbox_mounts = ["~/datasets:ro"]`.) Keyed by the
    /// repo slug, like the other `[workspace.<slug>]` keys.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sandbox_mounts: Vec<String>,
    /// Default env-bundle for this workspace (`[workspace.<slug>] env_bundle =
    /// "work"`). Consulted below a worktree override and above the global active
    /// bundle — the same precedence shape as `accounts`. See [`crate::bundle`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_bundle: Option<String>,
}

/// A named **environment bundle** (`[bundle.<name>]`) — a composable unit of env
/// vars + credential/config-dir redirection + per-provider account selection +
/// optional dotfiles, bound at any scope (global/workspace/worktree) and injected
/// at the pane-spawn seam for **every** pane. The "soft" work/personal identity
/// layer (roadmap AU): lighter than a whole-process profile (roadmap H), heavier
/// than the single-var account switch it generalizes. All fields optional; an
/// empty bundle is the no-op identity. Resolution lives in [`crate::bundle`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Bundle {
    /// Names of other bundles merged first (low precedence), for composition
    /// (`extends = ["base"]`). Cycles are broken defensively.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    /// Arbitrary env vars. Values support `env:`/`file:` indirection and
    /// `<scheme>:<ref>` secret resolvers (see `[secrets.resolvers]`).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Per-provider account selection (`accounts = { claude = "work" }`).
    /// Resolved through [`crate::account`] to the credential-home env var +
    /// path-preserving mount — how `account.rs` becomes a bundle consumer.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub accounts: std::collections::BTreeMap<String, String>,
    /// Tier-1 config-dir redirection (`config_dirs = { GIT_CONFIG_GLOBAL =
    /// "~/.config/git/work" }`). Just env vars pointing well-known tools at an
    /// alternate config dir; no file operations. `~` is expanded.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub config_dirs: std::collections::BTreeMap<String, String>,
    /// Tier-2 materialized dotfiles (opt-in) — a source tree symlinked/templated
    /// into the bundle's managed HOME. See [`crate::bundle`] (materialized
    /// off-loop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dotfiles: Option<DotfilesSpec>,
    /// Tier-3 synthetic HOME (opt-in): `"managed"` roots panes at the bundle's
    /// managed HOME; `"<path>"` roots them at an explicit dir; absent ⇒ inherit
    /// the real HOME.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub home: String,
    /// Opt into loading the worktree's `.env` on top of this bundle
    /// (allowlisted + credential-key-filtered; see [`crate::bundle`]).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dotenv: bool,
}

/// Tier-2 dotfile materialization spec (`[bundle.<n>.dotfiles]`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DotfilesSpec {
    /// Source tree to materialize into the managed HOME (`~` expanded).
    pub source: String,
    /// Symlink (default) or template/copy.
    pub mode: DotfileMode,
}

/// `[secrets.resolvers]` — pluggable external secret-resolver commands, keyed by
/// scheme. A bundle value like `ANTHROPIC_API_KEY = "pass:work/anthropic"` runs
/// the `pass` resolver at launch, off the event loop; values are never persisted.
/// The command template may contain `{ref}` (the part after `<scheme>:`),
/// `{key}` and `{file}` (for `sops`-style refs). See [`crate::bundle`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SecretsConfig {
    /// scheme → command template (`pass = "pass show {ref}"`).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub resolvers: std::collections::BTreeMap<String, String>,
}

impl SecretsConfig {
    /// True when no resolvers are configured (so serialization skips the table).
    pub fn is_empty(&self) -> bool {
        self.resolvers.is_empty()
    }
}

/// `[theme]` — visual tuning: the accent, the focus frame color, and optional
/// per-surface overrides of the whole chrome palette (`[theme.colors]`).
/// Invalid hex values warn-and-default; they never block startup.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ThemeConfig {
    /// Named palette preset: "prism" (default), "storm", "light", "abyss",
    /// "ember", "aurora". `[theme.colors]` / `[theme.hues]` overrides apply
    /// on top.
    pub preset: String,
    /// Focus accent as "#rrggbb" (default the signature teal).
    pub accent: String,
    /// Frame/highlight color of the focused pane, tab, and chrome edge
    /// (default the accent teal).
    pub focus_border: String,
    /// Horizontal breathing room (cells) between a pane's frame and its
    /// content, each side.
    pub pane_padding: u16,
    /// Curly-underline support: "auto" (sniff the terminal), "on", "off".
    pub undercurl: UndercurlMode,
    /// Color fidelity: "auto" (sniff + degrade), "truecolor", "256", "16",
    /// "none"/"mono". `NO_COLOR` forces "none" unless an explicit value is set.
    pub color: ColorMode,
    /// Glyph fidelity: "auto" (sniff locale/terminal), "unicode", "ascii".
    pub glyphs: GlyphMode,
    /// Optional overrides for every chrome surface/text color.
    pub colors: ThemeColors,
    /// Optional overrides for the eight semantic hues.
    pub hues: ThemeHues,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        ThemeConfig {
            preset: "prism".into(),
            accent: "#6ee7d8".into(),
            focus_border: "#6ee7d8".into(),
            pane_padding: 0,
            undercurl: UndercurlMode::Auto,
            color: ColorMode::Auto,
            glyphs: GlyphMode::Auto,
            colors: ThemeColors::default(),
            hues: ThemeHues::default(),
        }
    }
}

/// Accent/focus values treated as "not customized" when deciding whether the
/// user's `[theme]` should clobber a preset's own accent: the current default
/// plus the pre-prism defaults (a config that pinned the old default keeps
/// preset-cycling behavior).
const DEFAULTISH_ACCENTS: &[&str] = &["#6ee7d8", "#76eede"];
const DEFAULTISH_FOCUS: &[&str] = &["#6ee7d8", "#9bd1ff"];

/// `[theme.colors]` — all optional "#rrggbb" overrides; unset keys keep the
/// built-in storm-blue defaults (src/theme.rs).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ThemeColors {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg0: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg1: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub panel2: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raise: Option<String>,
    /// Frame lines around unfocused panes and chrome edges (light grey).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dim: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ghost: Option<String>,
    /// Foreground ramp step below ghost (structural glyphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ghost2: Option<String>,
    /// Deepest structural foreground (rules, fills, tracks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ghost3: Option<String>,
    /// Background of layer shadow cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_bg: Option<String>,
    /// Foreground of layer shadow cells.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_fg: Option<String>,
    /// Text inside inverse chips (defaults to bg0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chip_fg: Option<String>,
    /// Sidebar activity dot when a worktree is busy / its agent is working
    /// (defaults to the text tone, "white").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_active: Option<String>,
    /// Sidebar activity dot when an agent is waiting for the user's input
    /// (defaults to the red status hue).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_waiting: Option<String>,
}

/// `[theme.hues]` — all optional "#rrggbb" overrides for the eight semantic
/// hues (identity + status colors); unset keys keep the preset's hues.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ThemeHues {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magenta: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purple: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub green: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amber: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub red: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orange: Option<String>,
}

/// `[monitor]` — the resource managers opened from the top-bar stats widget
/// (highlight a stat with Super+Alt+Up, then Enter). Each is a shell command
/// run in an embedded tiled pane. `system` backs the CPU and MEM segments; `gpu`
/// backs the GPU segment.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MonitorConfig {
    /// CPU/RAM monitor (default `btm`, ClementTsang/bottom).
    pub system: String,
    /// GPU monitor (default `nvtop`).
    pub gpu: String,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        MonitorConfig {
            system: "btm".into(),
            gpu: "nvtop".into(),
        }
    }
}

/// `[stats]` — icons and refresh rate for the top-bar stats widget.
/// Icons can be text ("CPU") or unicode symbols ("⚡"). GPU shows only if detected.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StatsConfig {
    /// Polling interval in seconds.
    pub refresh_secs: f64,
    /// Icon for CPU stat.
    pub cpu_icon: String,
    /// Icon for memory stat.
    pub mem_icon: String,
    /// Icon for network stat.
    pub net_icon: String,
    /// Icon for GPU stat.
    pub gpu_icon: String,
    /// Icon for the CPU/package temperature stat.
    pub temp_icon: String,
    /// Icon for the swap-usage stat.
    pub swap_icon: String,
    /// Icon for the CPU-frequency stat.
    pub freq_icon: String,
    /// Icon for the load-average stat.
    pub load_icon: String,
    /// Icon for the uptime stat.
    pub uptime_icon: String,
    /// Icon for the battery stat (discharging).
    pub battery_icon: String,
    /// Icon shown while the battery is charging / on AC.
    pub battery_charging_icon: String,
    /// Battery percentage at/below which the widget turns red.
    pub battery_warn: u8,
    /// Icon for the disk (free-space) stat.
    pub disk_icon: String,
    /// Free-disk percentage at/below which the `disk` widget turns amber.
    pub disk_free_warn: u8,
    /// Free-disk percentage at/below which the `disk` widget turns red.
    pub disk_free_critical: u8,
    /// Filesystem the `disk` masthead widget measures (any path on it).
    /// Empty = the filesystem holding `worktrees_dir`. `~` expands to home.
    pub disk_path: String,
    /// Available refresh rates for keybind cycling (seconds).
    pub refresh_rates: Vec<f64>,
}

impl Default for StatsConfig {
    fn default() -> Self {
        StatsConfig {
            refresh_secs: 2.0,
            // Nerd Font glyphs by default (the bundled alacritty profile ships
            // a Nerd Font); set plain text ("CPU") if your font lacks them.
            //
            // All stat icons MUST come from the single-width PUA sets
            // (U+E000–U+F8FF, e.g. nf-fa-*). The cell math here and in chrome is
            // `chars().count()` == 1 per glyph, and these glyphs advance exactly
            // one cell. The plane-15 Material Design Icon set (`nf-md-*`,
            // U+F0000+) advances TWO cells in most Nerd Fonts, so an MDI icon
            // shoves its value ~1 cell right and breaks icon/value alignment —
            // do not use them here.
            cpu_icon: "\u{f4bc}".into(),
            mem_icon: "\u{efc5}".into(),
            net_icon: "\u{f1eb}".into(),              // nf-fa-wifi
            gpu_icon: "\u{f2db}".into(),              // nf-fa-microchip
            temp_icon: "\u{f2c7}".into(),             // nf-fa-thermometer_full
            swap_icon: "\u{f0ec}".into(),             // nf-fa-exchange
            freq_icon: "\u{f0e4}".into(),             // nf-fa-tachometer
            load_icon: "\u{f201}".into(),             // nf-fa-line_chart
            uptime_icon: "\u{f017}".into(),           // nf-fa-clock_o
            battery_icon: "\u{f240}".into(),          // nf-fa-battery_full
            battery_charging_icon: "\u{f0e7}".into(), // nf-fa-bolt — lightning bolt
            battery_warn: 25,
            disk_icon: "\u{f0a0}".into(), // nf-fa-hdd_o — hard drive
            disk_free_warn: 15,
            disk_free_critical: 10,
            disk_path: String::new(),
            refresh_rates: vec![1.0, 2.0, 5.0, 10.0],
        }
    }
}

/// `[bars]` — the customizable widget bars framing the workspace. Each slot is
/// an ordered widget-id list; unknown ids warn and are skipped. Built-ins:
/// `brand` (superzej + version), `cpu`, `mem`, `gpu`, `temp` (CPU °C), `net`,
/// `swap`, `freq` (CPU GHz), `load` (1-min load avg, unix), `uptime`, `disk`
/// (free %), `battery`, `date`, `clock` (top bar) and `keyhints`
/// (context-dependent keybinds), `pr` (forge + PR number/state), `status`
/// (transient messages + the keybind-lock badge) for the bottom bar.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct BarsConfig {
    pub top_left: Vec<String>,
    pub top_right: Vec<String>,
    pub bottom_left: Vec<String>,
    pub bottom_right: Vec<String>,
    /// chrono format string for the `date` widget.
    pub date_format: String,
    /// chrono format string for the `clock` widget.
    pub clock_format: String,
}

impl Default for BarsConfig {
    fn default() -> Self {
        BarsConfig {
            top_left: vec!["brand".into()],
            top_right: vec![
                "cpu".into(),
                "mem".into(),
                "disk".into(),
                "gpu".into(),
                "temp".into(),
                "net".into(),
                "battery".into(),
                "date".into(),
                "clock".into(),
            ],
            bottom_left: vec!["keyhints".into()],
            bottom_right: vec![
                "pr".into(),
                "tests".into(),
                "loc".into(),
                "disk".into(),
                "status".into(),
            ],
            date_format: "%a %b %-d".into(),
            clock_format: "%H:%M".into(),
        }
    }
}

/// `[limits]` — resource ceilings for tools launched in floating panes
/// (`superzej tool <name>`). When `systemd-run` is available, the tool runs in a
/// transient `--user --scope` with these caps, so a runaway child (e.g. yazi's
/// `ueberzugpp` image-preview backend, which can leak to tens of GB) is OOM-killed
/// *inside its own cgroup* instead of triggering a global OOM that takes the
/// terminal session down. Scope teardown on tool exit also reaps orphaned
/// children. An empty `tool_mem_max` disables containment.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LimitsConfig {
    /// `MemoryMax` for the tool scope (e.g. "6G"). Empty = no containment.
    pub tool_mem_max: String,
    /// `MemorySwapMax` for the tool scope (e.g. "1G").
    pub tool_mem_swap_max: String,
    /// `CPUQuota` for test/discovery runs (e.g. "150%" = 1.5 cores). Empty =
    /// uncapped. superzej never auto-runs tests, but an explicit run is still
    /// capped so a heavy suite can't pin the machine.
    pub test_cpu_quota: String,
    /// `MemoryMax` for test/discovery runs (e.g. "4G"). Empty = uncapped.
    pub test_mem_max: String,
    /// `nice` increment applied to test/discovery runs when no systemd scope is
    /// available (and as `Nice=` inside the scope when it is). 0 disables.
    pub test_nice: i32,
    /// Max concurrent test/discovery jobs across all worktrees. 1 keeps an
    /// explicit run from competing with another worktree's run for cores.
    pub test_max_parallel: usize,
    /// Wall-clock ceiling (seconds) for an explicit test run before its process
    /// group is killed. Generous, since suites legitimately take a while; the
    /// point is that a wedged run (e.g. blocked on a build lock) can't hang the
    /// panel forever. 0 disables the deadline.
    pub test_timeout_secs: u64,
    /// Wall-clock ceiling (seconds) for test *discovery*. Discovery should be
    /// near-instant (we use no-compile listing where possible), so a short cap
    /// surfaces "another build holds the cargo lock" instead of spinning. 0
    /// disables the deadline.
    pub discover_timeout_secs: u64,
    /// Run superzej-spawned `cargo` (test + discovery) under a private
    /// `CARGO_TARGET_DIR` (`<worktree>/target/superzej`) so it never blocks on
    /// the build-directory lock held by the user's own `cargo`/rust-analyzer.
    /// Costs a separate artifact cache; set false to share the default `target`.
    pub isolated_target_dir: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            tool_mem_max: "6G".into(),
            tool_mem_swap_max: "1G".into(),
            test_cpu_quota: "150%".into(),
            test_mem_max: "4G".into(),
            test_nice: 10,
            test_max_parallel: 1,
            test_timeout_secs: 1800,
            discover_timeout_secs: 45,
            isolated_target_dir: true,
        }
    }
}

/// `[disk]` — per-worktree disk-usage visibility, cleanup, and shared
/// build-cache knobs. Per-worktree `target/` dirs are the dominant disk cost
/// when developing across many worktrees; these surface it, reclaim it, and
/// dedup compilation. All off by default except visibility.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DiskConfig {
    /// Show per-worktree size badges in the sidebar and the statusbar total.
    pub show_sizes: bool,
    /// Statusbar warns (amber, then red at 2×) once total worktree disk exceeds
    /// this many GiB. 0 disables the warning badge.
    pub warn_threshold_gb: u64,
    /// Cadence (seconds) of the background disk scan that refreshes sizes. The
    /// scan runs off the event loop (never blocks it) and is cached in the DB.
    pub scan_interval_secs: u64,
    /// Automatically `cargo clean` a worktree's `target/` when its branch is
    /// merged (PR → MERGED). The checkout is kept; only build artifacts go. The
    /// active worktree and any with a running build are never touched.
    pub auto_clean_on_merge: bool,
    /// Also auto-clean when a PR is closed without merging (open → CLOSED).
    pub clean_on_pr_closed: bool,
    /// Inject `RUSTC_WRAPPER=sccache` into interactive panes so dependency
    /// compilation is shared across worktrees. No-op if `sccache` isn't on PATH.
    pub sccache: bool,
    /// `SCCACHE_DIR` for the shared cache. Empty = sccache's own default.
    /// `~` expands to home; a relative path resolves against the repo root.
    pub sccache_dir: String,
    /// Share one `CARGO_TARGET_DIR` across all worktrees of a repo (injected
    /// into interactive panes). Biggest disk win, but cargo's per-target build
    /// lock serializes concurrent builds across worktrees — opt-in. Empty = off.
    /// `~` expands to home; a relative path resolves against the repo root.
    pub shared_target_dir: String,
}

impl Default for DiskConfig {
    fn default() -> Self {
        DiskConfig {
            show_sizes: true,
            warn_threshold_gb: 100,
            scan_interval_secs: 45,
            auto_clean_on_merge: true,
            clean_on_pr_closed: false,
            sccache: false,
            sccache_dir: String::new(),
            shared_target_dir: String::new(),
        }
    }
}

/// `[pr]` — GitHub PR data feeding the right panel.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PrConfig {
    /// Cache TTL (seconds) before a live `gh` re-fetch.
    pub ttl_secs: u64,
}

impl Default for PrConfig {
    fn default() -> Self {
        PrConfig { ttl_secs: 30 }
    }
}

config_enum! {
    /// Active CI provider for the CI panel/view (AV group). `"auto"` picks the
    /// provider from the worktree's CI-config files + git remote; `"none"`
    /// disables. GitHub reuses the existing `gh`/`GH_TOKEN` auth (no sub-table).
    pub enum CiProviderKind : "ci provider" {
        Auto       = "auto",
        None       = "none",
        Github     = "github",
        Gitlab     = "gitlab",
        Drone      = "drone",
        Woodpecker = "woodpecker",
        Jenkins    = "jenkins",
        Argo       = "argo",
    } default = Auto;
}

/// `[ci]` — cross-provider CI/CD inspection (AV group). Provider-agnostic knobs
/// here; per-provider endpoints/tokens in the sub-tables. Tokens accept the
/// `"env:VAR"` form resolved by [`expand_env_ref`], so secrets stay out of the
/// file.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CiConfig {
    /// Active provider; `"auto"` detects from the worktree.
    pub provider: CiProviderKind,
    /// Cache TTL (seconds) before a background re-fetch of run history.
    pub ttl_secs: u64,
    /// Background poll cadence (seconds) while a run is in-flight / the CI view
    /// has live-refresh on.
    pub poll_interval_secs: u64,
    /// How many recent runs to fetch and display.
    pub max_runs: usize,
    /// Start the full-screen CI view with live refresh enabled (gama `ctrl+l`).
    pub live_refresh: bool,
    /// Cap on fetched log lines (the tail is kept) — bounds memory on huge jobs.
    pub log_tail_lines: usize,
    pub gitlab: GitLabCiConfig,
    pub drone: DroneCiConfig,
    pub woodpecker: WoodpeckerCiConfig,
    pub jenkins: JenkinsCiConfig,
    pub argo: ArgoCiConfig,
}

impl Default for CiConfig {
    fn default() -> Self {
        CiConfig {
            provider: CiProviderKind::Auto,
            ttl_secs: 30,
            poll_interval_secs: 30,
            max_runs: 50,
            live_refresh: false,
            log_tail_lines: 2000,
            gitlab: GitLabCiConfig::default(),
            drone: DroneCiConfig::default(),
            woodpecker: WoodpeckerCiConfig::default(),
            jenkins: JenkinsCiConfig::default(),
            argo: ArgoCiConfig::default(),
        }
    }
}

/// `[ci.gitlab]` — GitLab CI. `host` empty ⇒ gitlab.com; set it for self-hosted.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct GitLabCiConfig {
    pub host: String,
    /// API token. Use `"env:GITLAB_TOKEN"` to read from the environment.
    pub token: String,
}

impl Default for GitLabCiConfig {
    fn default() -> Self {
        GitLabCiConfig {
            host: String::new(),
            token: "env:GITLAB_TOKEN".into(),
        }
    }
}

/// `[ci.drone]` — Drone CI. Requires a server URL + token.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DroneCiConfig {
    pub server: String,
    pub token: String,
}

impl Default for DroneCiConfig {
    fn default() -> Self {
        DroneCiConfig {
            server: "env:DRONE_SERVER".into(),
            token: "env:DRONE_TOKEN".into(),
        }
    }
}

/// `[ci.woodpecker]` — Woodpecker CI (Drone fork). Server URL + token.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WoodpeckerCiConfig {
    pub server: String,
    pub token: String,
}

impl Default for WoodpeckerCiConfig {
    fn default() -> Self {
        WoodpeckerCiConfig {
            server: "env:WOODPECKER_SERVER".into(),
            token: "env:WOODPECKER_TOKEN".into(),
        }
    }
}

/// `[ci.jenkins]` — Jenkins. Per-instance URL + user/API-token (basic auth).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct JenkinsCiConfig {
    pub url: String,
    pub user: String,
    pub token: String,
}

impl Default for JenkinsCiConfig {
    fn default() -> Self {
        JenkinsCiConfig {
            url: String::new(),
            user: String::new(),
            token: "env:JENKINS_TOKEN".into(),
        }
    }
}

/// `[ci.argo]` — Argo Workflows / Argo CD. Server URL + token (k8s-context
/// dependent; empty ⇒ use the ambient `argo`/`argocd`/kubeconfig context).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ArgoCiConfig {
    pub server: String,
    pub token: String,
}

impl Default for ArgoCiConfig {
    fn default() -> Self {
        ArgoCiConfig {
            server: String::new(),
            token: "env:ARGOCD_TOKEN".into(),
        }
    }
}

/// `[issues]` — issue tracker integration (Linear, GitHub Issues, Jira).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IssuesConfig {
    /// Active provider. `"none"` disables the integration. Kept for back-compat;
    /// when `providers` is non-empty it takes precedence over this single value.
    pub provider: IssueProviderKind,
    /// Active providers to aggregate simultaneously, e.g. `["linear", "jira"]`.
    /// When non-empty this wins over the single `provider`; when empty the lone
    /// `provider` is used. Lets a developer track Linear *and* Jira at once.
    #[serde(default)]
    pub providers: Vec<IssueProviderKind>,
    /// Cache TTL (seconds) before a background re-fetch.
    pub ttl_secs: u64,
    /// Maximum issues to fetch and display.
    pub max_issues: usize,
    /// Pre-filter to issues assigned to the authenticated user.
    pub filter_assignee_me: bool,
    /// When a worktree's PR merges, move its linked issue to Done on the tracker.
    /// Off by default — issue lifecycle stays manual unless opted in.
    #[serde(default)]
    pub move_on_merge: bool,
    pub linear: LinearConfig,
    pub github_issues: GitHubIssuesConfig,
    pub jira: JiraConfig,
}

impl Default for IssuesConfig {
    fn default() -> Self {
        IssuesConfig {
            provider: IssueProviderKind::None,
            providers: Vec::new(),
            ttl_secs: 60,
            max_issues: 100,
            filter_assignee_me: true,
            move_on_merge: false,
            linear: LinearConfig::default(),
            github_issues: GitHubIssuesConfig::default(),
            jira: JiraConfig::default(),
        }
    }
}

impl IssuesConfig {
    /// The effective set of providers to aggregate, in config order, with `None`
    /// removed and duplicates collapsed. When `providers` is non-empty it wins;
    /// otherwise the single legacy `provider` is used (unless it is `None`).
    pub fn active_providers(&self) -> Vec<IssueProviderKind> {
        let raw: &[IssueProviderKind] = if self.providers.is_empty() {
            std::slice::from_ref(&self.provider)
        } else {
            &self.providers
        };
        let mut out: Vec<IssueProviderKind> = Vec::new();
        for &p in raw {
            if p != IssueProviderKind::None && !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }
}

config_enum! {
    /// Which issue tracker backend is active.
    pub enum IssueProviderKind : "issue provider" {
        None    = "none",
        Linear  = "linear",
        Github  = "github",
        Jira    = "jira",
    } default = None;
}

/// `[issues.linear]` — Linear.app configuration.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LinearConfig {
    /// API key. Use `"env:LINEAR_API_KEY"` to read from the environment.
    pub api_key: String,
    /// Restrict to a single team id. `""` = all teams.
    pub team_id: String,
    /// Optional workspace slug (used for URLs; inferred if empty).
    pub workspace_slug: String,
}

impl Default for LinearConfig {
    fn default() -> Self {
        LinearConfig {
            api_key: "env:LINEAR_API_KEY".into(),
            team_id: String::new(),
            workspace_slug: String::new(),
        }
    }
}

/// `[issues.github_issues]` — GitHub Issues configuration.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema, Default)]
#[serde(default)]
pub struct GitHubIssuesConfig {
    /// Additional `gh issue list` flags, e.g. `--assignee @me --label bug`.
    pub extra_flags: Vec<String>,
}

/// `[issues.jira]` — Jira Cloud/Server configuration.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct JiraConfig {
    /// Jira instance base URL, e.g. `"https://myorg.atlassian.net"`.
    pub base_url: String,
    /// Jira user email.
    pub email: String,
    /// API token. Use `"env:JIRA_API_TOKEN"` to read from the environment.
    pub api_token: String,
    /// Restrict to a single project key, e.g. `"PROJ"`. `""` = all projects.
    pub project_key: String,
}

impl Default for JiraConfig {
    fn default() -> Self {
        JiraConfig {
            base_url: String::new(),
            email: String::new(),
            api_token: "env:JIRA_API_TOKEN".into(),
            project_key: String::new(),
        }
    }
}

/// `[apps]` — top-level sub-app tab ordering and startup focus.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct AppsConfig {
    /// Tab focused on startup. Valid ids: "work".
    pub default_tab: String,
    /// Ordered top-level tab ids. Unknown ids are ignored; missing built-ins are appended.
    pub tab_order: Vec<String>,
}

impl Default for AppsConfig {
    fn default() -> Self {
        AppsConfig {
            default_tab: "work".into(),
            tab_order: vec!["work".into()],
        }
    }
}

impl AppsConfig {
    pub const BUILTIN_TABS: [&'static str; 1] = ["work"];

    pub fn effective_tab_order(&self) -> Vec<String> {
        let mut out = Vec::new();
        for id in &self.tab_order {
            let id = id.trim();
            if Self::BUILTIN_TABS.contains(&id) && !out.iter().any(|existing| existing == id) {
                out.push(id.to_string());
            }
        }
        for id in Self::BUILTIN_TABS {
            if !out.iter().any(|existing| existing == id) {
                out.push(id.to_string());
            }
        }
        out
    }

    pub fn normalized_default_tab(&self) -> String {
        let default = self.default_tab.trim();
        if self.effective_tab_order().iter().any(|id| id == default) {
            default.to_string()
        } else {
            self.effective_tab_order()
                .into_iter()
                .next()
                .unwrap_or_else(|| "work".into())
        }
    }
}

/// `[watch]` — the per-session daemon that pushes live panel updates.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WatchConfig {
    /// Seconds between PR refreshes (back-off applies on rate limits).
    pub pr_interval_secs: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        WatchConfig {
            pr_interval_secs: 20,
        }
    }
}

/// `[log]` — diagnostics. The stderr sink is always on (level-gated); the file
/// sink under `dir` is opt-in. `SUPERZEJ_LOG` (env) is a `tracing`-style filter
/// that overrides `level` per-module.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LogConfig {
    pub level: LogLevel,
    /// Mirror diagnostics to a rotating file under `dir`.
    pub file: bool,
    /// Log directory ("" => `$XDG_STATE_HOME/superzej/logs`). Tilde-expanded.
    pub dir: String,
    /// Rotate the active log once it exceeds this many MiB.
    pub rotation_size_mb: u64,
    /// How many rotated files to keep.
    pub max_files: usize,
    pub format: LogFormat,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            level: LogLevel::Info,
            file: false,
            dir: String::new(),
            rotation_size_mb: 5,
            max_files: 5,
            format: LogFormat::Text,
        }
    }
}

impl LogConfig {
    /// The resolved log directory (default under `$XDG_STATE_HOME/superzej`).
    pub fn dir_path(&self) -> PathBuf {
        if self.dir.trim().is_empty() {
            util::xdg_state_home().join("superzej/logs")
        } else {
            PathBuf::from(util::expand_tilde(&self.dir))
        }
    }
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

/// `[sandbox.remote]` — optionally run a worktree on a remote machine. Empty
/// `host` means local (the default); set it (e.g. `user@devbox`) to enable.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RemoteConfig {
    pub host: String, // "" => local
    pub port: u16,
    pub transport: RemoteTransport,
    pub mode: RemoteMode,
    pub remote_dir: String,  // where remote worktrees live (mode=remote)
    pub forward_agent: bool, // ssh -A so remote git push uses the host agent
}

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            host: String::new(),
            port: 22,
            transport: RemoteTransport::Mosh,
            mode: RemoteMode::Remote,
            remote_dir: "~/superzej-worktrees".into(),
            forward_agent: true,
        }
    }
}

impl RemoteConfig {
    /// Whether a remote host is configured (otherwise everything is local).
    pub fn is_remote(&self) -> bool {
        !self.host.trim().is_empty()
    }
}

/// `[env.<name>]` — a named, reusable execution environment. Selected per
/// workspace/repo/worktree (DB `env_name`, repo `.superzej.*` `env =`, or the
/// global `[sandbox] default_env`) and resolved by [`Config::resolve_env`].
///
/// An env bundles *placement* (where it runs), an isolation *overlay* (applied
/// on top of the base `[sandbox]`), and a *data* mode (where files live), plus
/// placement-specific `[env.<name>.{ssh,k8s,provider}]` sub-tables.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct EnvConfig {
    /// Where this env's processes run.
    pub placement: PlacementMode,
    /// Where the worktree files live (defaults to "in the env").
    pub data: DataMode,
    /// Isolation overlay applied on top of the base `[sandbox]` (+ profile +
    /// repo overlay). e.g. `backend = "podman"`, `image = "..."`, `profile`.
    #[serde(skip_serializing_if = "SandboxOverlay::is_empty")]
    pub sandbox: SandboxOverlay,
    /// `[env.<name>.ssh]` — SSH/mosh connection knobs (placement = ssh).
    #[serde(skip_serializing_if = "EnvSshConfig::is_default")]
    pub ssh: EnvSshConfig,
    /// `[env.<name>.k8s]` — Kubernetes pod target (placement = k8s).
    #[serde(skip_serializing_if = "EnvK8sConfig::is_default")]
    pub k8s: EnvK8sConfig,
    /// `[env.<name>.provider]` — managed-sandbox provider (placement = provider).
    #[serde(skip_serializing_if = "EnvProviderConfig::is_default")]
    pub provider: EnvProviderConfig,
    /// Per-env override of `[sandbox] failover`. `None` ⇒ inherit the global
    /// policy; `Some(true)` lets *this* env fall back (chain → host) when it
    /// can't be brought up; `Some(false)` forces a halt+warning even if the
    /// global default allows failover. Resolved by [`Config::env_failover`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failover: Option<bool>,
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
    fn is_default(&self) -> bool {
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
    fn is_default(&self) -> bool {
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
}

impl EnvProviderConfig {
    fn is_default(&self) -> bool {
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
}

/// `[sandbox]` — containerize/sandbox a worktree's interactive process. On by
/// default; `backend = "auto"` walks `backend_chain` and falls back to the host
/// shell (with a warning) when nothing is available.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Deserialize,
    serde::Serialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FileAccess {
    Worktree,
    #[default]
    WorktreePlusCaches,
    Custom,
    All,
    Host,
    None,
}

#[derive(
    Debug, Clone, Default, serde::Deserialize, PartialEq, Eq, serde::Serialize, schemars::JsonSchema,
)]
#[serde(default)]
pub struct SandboxLimits {
    pub cpu: Option<String>,
    pub memory: Option<String>,
}

config_enum! {
    /// `[sandbox.vpn] provider` — which overlay/tunnel a sandbox attaches to.
    /// `none` (the default) leaves the worktree's network behavior unchanged.
    /// `headscale` is `tailscale` pointed at a self-hosted control server
    /// (`[sandbox.vpn.tailscale] login_server`).
    pub enum VpnProviderKind: "vpn provider" {
        None = "none" | "off",
        Tailscale = "tailscale" | "ts",
        Headscale = "headscale" | "hs",
        Wireguard = "wireguard" | "wg" | "wg-quick",
        Openvpn = "openvpn" | "ovpn",
        Netbird = "netbird" | "nb",
        Zerotier = "zerotier" | "zt",
        Custom = "custom" | "command",
    } default = None;
}
config_enum! {
    /// How the tunnel is realized for the sandbox.
    ///  - `sidecar` (default): a companion container owns the network namespace;
    ///    the worktree OCI container joins it via `--network container:<sidecar>`,
    ///    so its only egress is the tunnel and its capabilities stay untouched
    ///    (NET_ADMIN/TUN live in the sidecar).
    ///  - `proxy`: a userspace tunnel exposes a SOCKS5/HTTP proxy; the inner
    ///    process is pointed at it via `ALL_PROXY`/`HTTPS_PROXY`. No NET_ADMIN or
    ///    /dev/net/tun needed, but only proxy-aware traffic is tunneled (not a
    ///    containment boundary). The only honest option for bwrap/systemd.
    ///  - `in_container`: run the VPN client inside the worktree container itself
    ///    (needs NET_ADMIN + /dev/net/tun; weakens `hardened`, refused if caps
    ///    are dropped).
    ///  - `netns`: join a host-prepared named network namespace (host-toolchain
    ///    backends; best-effort, needs privilege to set up).
    pub enum VpnMode: "vpn mode" {
        Sidecar = "sidecar",
        Proxy = "proxy",
        InContainer = "in_container" | "in-container",
        Netns = "netns",
    } default = Sidecar;
}
config_enum! {
    /// What to do when the tunnel can't be brought up.
    ///  - `fail` (default): refuse to launch the sandbox (don't silently fall
    ///    back to a less-isolated network).
    ///  - `warn`: launch with the tunnel down (loud warning).
    ///  - `offline`: force `network=none` so nothing leaks onto the host network.
    pub enum VpnOnError: "vpn on_error" {
        Fail = "fail",
        Warn = "warn",
        Offline = "offline",
    } default = Fail;
}
config_enum! {
    /// How DNS resolution inside the sandbox composes with the overlay.
    ///  - `tunnel` (default): the provider owns resolution (MagicDNS / pushed
    ///    resolvers); the `network_allow`/`network_block` filter is bypassed.
    ///  - `filter-front`: chain the allow/block DNS filter in front, forwarding
    ///    to the tunnel's resolver (preserves auditing).
    ///  - `filter-only`: ignore the overlay's pushed DNS, keep the filter only.
    pub enum VpnDnsMode: "vpn dns" {
        Tunnel = "tunnel",
        FilterFront = "filter-front" | "filter_front",
        FilterOnly = "filter-only" | "filter_only",
    } default = Tunnel;
}

/// `[sandbox.vpn]` — attach this worktree's sandbox to its own overlay/tunnel
/// with its own identity, leaving host networking (including any host
/// `tailscaled`) untouched. Disabled by default (`provider = "none"`). Only the
/// selected provider's sub-table is consulted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct VpnConfig {
    pub provider: VpnProviderKind,
    pub mode: VpnMode,
    /// Override the provider's default sidecar image. Empty = provider default.
    pub sidecar_image: String,
    /// Seconds to wait for the tunnel's readiness probe before applying
    /// `on_error`.
    pub ready_timeout_secs: u64,
    pub on_error: VpnOnError,
    pub dns: VpnDnsMode,
    /// Request an ephemeral node identity where the provider supports it
    /// (Tailscale/Headscale/NetBird), so the device auto-deregisters on teardown.
    pub ephemeral: bool,
    pub tailscale: TailscaleConfig,
    pub wireguard: WireguardConfig,
    pub openvpn: OpenvpnConfig,
    pub netbird: NetbirdConfig,
    pub zerotier: ZerotierConfig,
    pub custom: CustomVpnConfig,
}

impl Default for VpnConfig {
    fn default() -> Self {
        VpnConfig {
            provider: VpnProviderKind::None,
            mode: VpnMode::Sidecar,
            sidecar_image: String::new(),
            ready_timeout_secs: 30,
            on_error: VpnOnError::Fail,
            dns: VpnDnsMode::Tunnel,
            ephemeral: true,
            tailscale: TailscaleConfig::default(),
            wireguard: WireguardConfig::default(),
            openvpn: OpenvpnConfig::default(),
            netbird: NetbirdConfig::default(),
            zerotier: ZerotierConfig::default(),
            custom: CustomVpnConfig::default(),
        }
    }
}

impl VpnConfig {
    /// Whether a tunnel is requested at all.
    pub fn is_enabled(&self) -> bool {
        self.provider != VpnProviderKind::None
    }
}

/// `[sandbox.vpn.tailscale]` — Tailscale / Headscale. `login_server` is what
/// makes it Headscale (a self-hosted control server).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TailscaleConfig {
    /// Auth key (secrets-ref: `"env:TS_AUTHKEY"` or `"file:~/.ts/dev.key"`).
    /// Prefer an ephemeral, pre-authorized, tagged key for dev envs.
    pub auth_key: String,
    /// Custom control server, e.g. `"https://headscale.example.com"`.
    pub login_server: String,
    /// ACL tags to advertise, e.g. `["tag:dev"]`.
    pub tags: Vec<String>,
    /// Route egress through this exit node (hostname or IP). `""` = none.
    pub exit_node: String,
    /// Accept subnet routes advertised by the tailnet.
    pub accept_routes: bool,
    /// Node name in the tailnet. `""` = derive from the container name.
    pub hostname: String,
    /// Advertise these CIDRs as subnet routes from the sandbox.
    pub advertise_routes: Vec<String>,
    /// Extra `tailscale up` flags for anything not modeled here.
    pub extra_args: Vec<String>,
}

impl Default for TailscaleConfig {
    fn default() -> Self {
        TailscaleConfig {
            auth_key: "env:TS_AUTHKEY".into(),
            login_server: String::new(),
            tags: Vec::new(),
            exit_node: String::new(),
            accept_routes: false,
            hostname: String::new(),
            advertise_routes: Vec::new(),
            extra_args: Vec::new(),
        }
    }
}

/// `[sandbox.vpn.wireguard]` — a wg-quick tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WireguardConfig {
    /// Path to a wg-quick `.conf` (mounted into the sidecar read-only). Mutually
    /// exclusive with `config`; `config` wins if both are set.
    pub config_path: String,
    /// Inline config body (secrets-ref `"file:..."` recommended to keep keys out
    /// of the superzej config file).
    pub config: String,
}

/// `[sandbox.vpn.openvpn]` — an OpenVPN tunnel.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct OpenvpnConfig {
    /// Path to a `.ovpn` (mounted into the sidecar read-only).
    pub config_path: String,
    /// `user\npass` credentials (secrets-ref `"file:~/.ovpn/creds"`).
    pub auth_user_pass: String,
    /// Extra `openvpn` flags.
    pub extra_args: Vec<String>,
}

/// `[sandbox.vpn.netbird]` — a NetBird mesh.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NetbirdConfig {
    /// Setup key (secrets-ref).
    pub setup_key: String,
    /// Self-hosted management URL. `""` = NetBird's hosted control plane.
    pub management_url: String,
    /// Peer hostname. `""` = derive from the container name.
    pub hostname: String,
}

/// `[sandbox.vpn.zerotier]` — a ZeroTier network.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ZerotierConfig {
    /// 16-hex network id to join.
    pub network_id: String,
    /// Self-hosted controller/moon URL. `""` = ZeroTier's hosted controller.
    pub controller_url: String,
    /// API token (secrets-ref) used to auto-authorize the joining member.
    pub api_token: String,
}

/// `[sandbox.vpn.custom]` — the open escape hatch for any tunnel not modeled
/// above (Nebula, Tinc, a corporate IPsec script, …). The `up`/`down`/
/// `ready_check` commands run via `sh -c`; the template vars `{name}`,
/// `{netns}`, and `{worktree}` are expanded before execution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct CustomVpnConfig {
    /// Command that establishes the tunnel.
    pub up: String,
    /// Command that tears it down (best-effort, run on teardown).
    pub down: String,
    /// Command whose exit-0 means "ready" (polled until `ready_timeout_secs`).
    pub ready_check: String,
    /// Sidecar image when `mode = "sidecar"`. `""` for proxy/netns modes.
    pub image: String,
    /// Extra env passed to the `up`/`ready_check` commands / sidecar.
    pub env: std::collections::BTreeMap<String, String>,
}

// ── Ingress sharing (`[share]`) ─────────────────────────────────────────────
// The *inbound* sibling of `[sandbox.vpn]`: expose a service running inside a
// worktree (a dev server, a PR preview, a webhook/OAuth callback) at a public
// URL. Like the VPN seam this is provider-pluggable; `bore` is the first
// backend. Worktree-level, not a sandbox-network attribute, so it lives at the
// top level rather than under `[sandbox]`.

config_enum! {
    /// `[share] provider` — which tunnel backend gives a worktree port a URL.
    /// `none` (default) disables sharing. Future backends (rathole / zrok /
    /// ngrok / iroh) plug in behind the same `ShareProvider` seam.
    pub enum ShareProviderKind: "share provider" {
        None = "none" | "off",
        Bore = "bore",
        Frp = "frp",
        Tailscale = "tailscale" | "ts",
        Iroh = "iroh" | "dumbpipe",
    } default = None;
}
config_enum! {
    /// `[share.frp] proxy_type` — how frp exposes the port. `https`/`http` get a
    /// vhost subdomain URL; `tcp`/`udp` get a `host:port` address.
    pub enum FrpProxyType: "frp proxy_type" {
        Https = "https",
        Http = "http",
        Tcp = "tcp",
        Udp = "udp",
    } default = Https;
}
config_enum! {
    /// `[share] visibility` — who can reach the share. `bore` only does
    /// `public` (anyone with the URL); `private` (identity-scoped) is reserved
    /// for the iroh/zrok backends.
    pub enum ShareVisibility: "share visibility" {
        Public = "public",
        Private = "private",
    } default = Public;
}
config_enum! {
    /// `[share] on_error` — what to do when a share can't be brought up.
    ///  - `fail` (default): surface the error; the share does not start.
    ///  - `warn`: log a warning and carry on (no URL).
    pub enum ShareOnError: "share on_error" {
        Fail = "fail",
        Warn = "warn",
    } default = Fail;
}
config_enum! {
    /// Who a share is for — the intent the reach-picker offers. Each maps to a
    /// provider via the `[share] public`/`team`/`peer` keys.
    ///  - `public` — anyone with the link (the internet).
    ///  - `team`   — your tailnet / a teammate (identity-scoped).
    ///  - `peer`   — a specific machine you hand a ticket to (P2P).
    pub enum ShareReach: "share reach" {
        Public = "public",
        Team = "team",
        Peer = "peer",
    } default = Public;
}

/// `[share]` — expose a worktree-local port at a public URL. Disabled by
/// default (`provider = "none"`). Only the selected provider's sub-table is
/// consulted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ShareConfig {
    pub provider: ShareProviderKind,
    pub visibility: ShareVisibility,
    pub on_error: ShareOnError,
    /// Seconds to wait for the share's URL to appear before applying `on_error`.
    pub ready_timeout_secs: u64,
    /// Safety guard: when `false`, refuse any share reachable from the public
    /// internet (frp http(s), tailscale `funnel`). Private/team/peer shares are
    /// unaffected. Default `true`.
    pub allow_public: bool,
    /// Intent-first reach → provider mapping for the reach picker. Each defaults
    /// to `none` (unset); when ≥2 are set, `Alt+Shift+S` offers a picker. When
    /// all are unset, the single `provider` is used (no picker).
    pub public: ShareProviderKind,
    pub team: ShareProviderKind,
    pub peer: ShareProviderKind,
    pub bore: BoreConfig,
    pub frp: FrpConfig,
    pub tailscale: TailscaleShareConfig,
    pub iroh: IrohShareConfig,
}

impl Default for ShareConfig {
    fn default() -> Self {
        ShareConfig {
            provider: ShareProviderKind::None,
            visibility: ShareVisibility::Public,
            on_error: ShareOnError::Fail,
            ready_timeout_secs: 20,
            allow_public: true,
            public: ShareProviderKind::None,
            team: ShareProviderKind::None,
            peer: ShareProviderKind::None,
            bore: BoreConfig::default(),
            frp: FrpConfig::default(),
            tailscale: TailscaleShareConfig::default(),
            iroh: IrohShareConfig::default(),
        }
    }
}

impl ShareConfig {
    /// Whether sharing is requested at all (a default `provider` or any reach key).
    pub fn is_enabled(&self) -> bool {
        self.provider != ShareProviderKind::None || !self.configured_reaches().is_empty()
    }

    /// The provider mapped to a reach (`none` if that reach is unset).
    pub fn reach_provider(&self, reach: ShareReach) -> ShareProviderKind {
        match reach {
            ShareReach::Public => self.public,
            ShareReach::Team => self.team,
            ShareReach::Peer => self.peer,
        }
    }

    /// Reaches that map to a real provider, in public→team→peer order. `Public`
    /// is omitted when `allow_public` is off, so the picker never offers a
    /// reach the safety guard would refuse.
    pub fn configured_reaches(&self) -> Vec<ShareReach> {
        [ShareReach::Public, ShareReach::Team, ShareReach::Peer]
            .into_iter()
            .filter(|&r| self.reach_provider(r) != ShareProviderKind::None)
            .filter(|&r| r != ShareReach::Public || self.allow_public)
            .collect()
    }
}

/// `[forward]` — automatically forward dev-server ports bound *inside* a
/// worktree's sandbox to the host's loopback for browser preview. The
/// *outbound-localhost* sibling of [`ShareConfig`]: `[share]` exposes a port at a
/// public URL; `[forward]` makes a sandbox-internal port reachable on the host.
/// On by default — forwards bind loopback only, so this is safe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ForwardConfig {
    /// Auto-detect newly-bound ports inside the worktree's sandbox and forward
    /// them to the host. When `false`, only an explicit `superzej forward` acts.
    pub auto: bool,
    /// Host-port allocation range (`"lo-hi"`, inclusive) used when a detected
    /// port's own number is already taken on the host. Malformed ⇒ `8000-8999`.
    pub range: String,
    /// Container ports never to forward (e.g. an in-sandbox database or sshd).
    pub ignore: Vec<u16>,
    /// If non-empty, an allowlist: ONLY these container ports are forwarded.
    pub only: Vec<u16>,
    /// Host interface to bind forwards on. Loopback keeps previews host-local.
    pub bind: String,
    /// Detector poll cadence in seconds (how often the sandbox is scanned for
    /// newly-bound listening ports).
    pub poll_secs: u64,
    /// Browser command for the "open in browser" action. Empty ⇒ `$BROWSER`,
    /// then `xdg-open`/`open`.
    pub browser: String,
    /// Open the browser automatically when a new forward comes up.
    pub open_on_detect: bool,
}

impl Default for ForwardConfig {
    fn default() -> Self {
        ForwardConfig {
            auto: true,
            range: "8000-8999".into(),
            ignore: Vec::new(),
            only: Vec::new(),
            bind: "127.0.0.1".into(),
            poll_secs: 2,
            browser: String::new(),
            open_on_detect: false,
        }
    }
}

/// `[share.bore]` — <https://github.com/ekzhang/bore>, a tiny TCP tunnel. The
/// client connects out to a relay (`to`) and exposes the local port at
/// `to:<remote_port>`. Run your own `bore server` and set `to`/`secret`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct BoreConfig {
    /// Relay server host (the machine running `bore server`). `""` falls back to
    /// the public `bore.pub` instance (no secret, best-effort).
    pub to: String,
    /// Optional shared HMAC secret (secrets-ref `"env:BORE_SECRET"` /
    /// `"file:~/.bore/secret"`); must match the server's `--secret`.
    pub secret: String,
    /// Remote port to bind on the relay. `0` = let the relay choose.
    pub remote_port: u16,
    /// Local interface the dev server listens on (forwarded to the relay).
    pub local_host: String,
    /// Extra `bore local` flags for anything not modeled here.
    pub extra_args: Vec<String>,
}

impl Default for BoreConfig {
    fn default() -> Self {
        BoreConfig {
            to: String::new(),
            secret: "env:BORE_SECRET".into(),
            remote_port: 0,
            local_host: "127.0.0.1".into(),
            extra_args: Vec::new(),
        }
    }
}

/// `[share.frp]` — <https://github.com/fatedier/frp>, a self-hosted reverse
/// proxy. The client (`frpc`) connects out to your `frps` server and exposes the
/// worktree port; `https`/`http` get a vhost subdomain, `tcp`/`udp` a remote
/// port. The public address is derived from config (frpc never prints it).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct FrpConfig {
    /// `frps` server host (required). Empty ⇒ the provider errors at start.
    pub server_addr: String,
    /// `frps` control port.
    pub server_port: u16,
    /// Shared token (secrets-ref `"env:FRP_TOKEN"` / `"file:~/.frp/token"`);
    /// must match the server's `auth.token`.
    pub token: String,
    pub proxy_type: FrpProxyType,
    /// The base domain the server serves subdomains under (its `subDomainHost`),
    /// e.g. `share.example.com` — used to derive the `https`/`http` URL.
    pub subdomain_host: String,
    /// Subdomain label for `https`/`http`. Empty ⇒ a deterministic per-worktree
    /// slug (`<worktree>-<port>`), so a worktree's preview URL is stable.
    pub subdomain: String,
    /// Remote port for `tcp`/`udp`. `0` = let the server choose.
    pub remote_port: u16,
    /// HTTPS vhost port on the server (for the derived URL when not 443).
    pub vhost_https_port: u16,
    /// Extra `frpc.toml` lines appended verbatim to the `[[proxies]]` block.
    pub extra: Vec<String>,
}

impl Default for FrpConfig {
    fn default() -> Self {
        FrpConfig {
            server_addr: String::new(),
            server_port: 7000,
            token: "env:FRP_TOKEN".into(),
            proxy_type: FrpProxyType::Https,
            subdomain_host: String::new(),
            subdomain: String::new(),
            remote_port: 0,
            vhost_https_port: 443,
            extra: Vec::new(),
        }
    }
}

/// `[share.tailscale]` — expose the worktree port over the worktree's existing
/// `[sandbox.vpn]` tailscale tunnel. `serve` (default) keeps it tailnet-private;
/// `funnel = true` publishes it to the public internet. Requires
/// `[sandbox.vpn] provider = "tailscale"` (or `headscale`) on the worktree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct TailscaleShareConfig {
    /// `false` (default) ⇒ `tailscale serve` (tailnet-only). `true` ⇒
    /// `tailscale funnel` (public internet; ports limited to 443/8443/10000).
    pub funnel: bool,
    /// HTTPS port to expose on (serve/funnel default 443).
    pub https_port: u16,
}

impl Default for TailscaleShareConfig {
    fn default() -> Self {
        TailscaleShareConfig {
            funnel: false,
            https_port: 443,
        }
    }
}

/// `[share.iroh]` — a peer-to-peer TCP tunnel over iroh via `dumbpipe`
/// (<https://github.com/n0-computer/dumbpipe>). NAT-traversing, relay-fallback;
/// the consumer connects with `dumbpipe connect-tcp <ticket>` (not a browser),
/// so the "address" is a ticket. For a self-hosted `iroh-relay`, pass dumbpipe's
/// relay flag/env via `extra_args`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IrohShareConfig {
    /// Extra `dumbpipe listen-tcp` flags (e.g. a custom relay).
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub backend: SandboxBackend,
    /// Default selection for new worktrees; `auto` means use `backend_chain`.
    pub default_backend: SandboxBackend,
    /// Name of the `[env.<name>]` execution environment to use by default when a
    /// worktree/workspace/repo hasn't selected one. Empty ⇒ the implicit
    /// `"default"` env (this `[sandbox]` block + `[sandbox.remote]`, today's
    /// behavior). See [`Config::resolve_env`].
    pub default_env: String,
    pub backend_chain: Vec<String>, // auto detection order; "host" = host fallback
    pub image: String,              // "" => host-toolchain mode
    /// Hardening preset for the worktree's interactive container (shell panes).
    pub profile: SandboxProfile,
    /// Hardening preset for the embedded agent's tool container. When it differs
    /// from `profile`, the agent runs in its own separate (more-locked-down)
    /// container; when equal, it reuses the worktree container.
    pub agent_profile: SandboxProfile,
    pub network: Network,
    pub file_access: FileAccess,
    pub ports: Vec<String>, // e.g. ["8080:8080"]
    pub gpu: Option<String>,
    pub limits: SandboxLimits,
    pub volumes: std::collections::HashMap<String, String>,
    pub compose: Option<String>,
    pub env_passthrough: Vec<String>,
    /// Add common language build caches to `worktree_plus_caches` sandboxes.
    pub auto_caches: bool,
    pub mounts: Vec<String>, // extra binds ("host:dest[:ro|rw|cache]" or "host"); suffix allowed
    pub init_script: String, // runs inside before the agent/shell
    /// Host-side setup commands run (off-loop, via `sh -lc` in the worktree)
    /// once when a worktree is created, before its first pane spawns — for
    /// heavyweight prep that benefits from the host's writable store/daemon and
    /// network (e.g. `mise install`, a cache warm). The built-in `direnv` warm
    /// (`warm_direnv`) runs alongside these.
    pub prepare: Vec<String>,
    /// Pre-warm a worktree's `direnv` cache on the host so the in-sandbox
    /// `direnv` hook works against the read-only `/nix/store`. See
    /// [`crate::direnv`] and [`WarmDirenv`].
    pub warm_direnv: WarmDirenv,
    pub devenv: bool, // wrap inner cmd with `devenv shell --`
    /// Inject the repo's Nix flake `devShell` toolchain (its `PATH` + safe
    /// exported vars) into worktree panes — resolved on the host (writable
    /// store + daemon) and cached, so a sandboxed pane that can't reach the Nix
    /// daemon still gets the project linters/formatters/tools out of the box.
    /// No-op for repos without a flake `devShell`. See [`crate::devenv`].
    pub inject_devshell: bool,
    /// Which flake devShell attribute a sandbox/sprite enters, e.g. `"sandbox"`
    /// for a lean build-only shell (`.#devShells.sandbox`). Empty ⇒ `default`
    /// (unchanged). Exported as `SUPERZEJ_DEVSHELL` into the sandbox (pane + the
    /// provisioning seed), which the repo `.envrc`'s `use flake .#${…:-default}`
    /// reads — so the sandbox builds/enters a smaller closure than full host dev.
    pub devshell: String,
    /// Bind-mount the host Nix daemon socket into the sandbox so full
    /// `nix develop`/`build`/`fmt` work *inside* it. Off by default: it relaxes
    /// the isolation the hardening profiles provide (Tier A `inject_devshell`
    /// already covers read-only tool access without this).
    pub nix_daemon: bool,
    /// Shell to use inside the sandbox. `""` = resolve from the host's `$SHELL`
    /// at pane-spawn time. Set to an absolute path or name (e.g. `"zsh"`) to
    /// override per workspace via `.superzej.toml`.
    pub shell: String,
    pub on_missing: OnMissing,
    /// When the *selected* environment (a named `[env.<name>]` or an explicit
    /// non-local placement — provider/k8s/ssh) cannot be brought up, may superzej
    /// silently fall back to another backend or the host? Default `false`: a
    /// bring-up failure **halts** that worktree's pane and shows a warning,
    /// because a remote/managed env is often required for correctness or safety
    /// and a quiet drop to the host would violate that. Set `true` (globally
    /// here, or per-env via `[env.<name>] failover`) to allow walking the
    /// `backend_chain` down to the host on failure (the historical behavior).
    /// Independent of `on_missing`, which only governs the local `Auto` chain.
    pub failover: bool,
    /// Drive the OCI runtime against a **remote daemon** instead of SSH-wrapping
    /// the whole backend argv: a podman connection URL/name or a docker host
    /// (e.g. `ssh://user@host`). Empty ⇒ local daemon (the default). Injected as
    /// `podman --url`/`--connection` or `docker -H` before every container
    /// subcommand. An alternative reach to `[env.<name>] placement = "ssh"`,
    /// useful when the daemon (not just the shell) lives on another box.
    pub oci_host: String,
    pub remote: RemoteConfig,
    /// Allow-only these hostnames for outbound connections (empty = allow all).
    /// Enforced via a per-container DNS interceptor. Block-list is checked first
    /// when both lists are non-empty.
    pub network_allow: Vec<String>,
    /// Block outbound connections to these hostnames. Takes priority over
    /// network_allow when a hostname matches both.
    pub network_block: Vec<String>,
    /// Log all outbound DNS queries and connection attempts to the audit table.
    pub network_audit: bool,
    /// `[sandbox.vpn]` — attach the worktree to its own overlay/tunnel.
    pub vpn: VpnConfig,
    /// `[sandbox.home]` — the generic, declarative *personal* environment layer
    /// reproduced in every sandbox/remote (CLI tools, dotfiles, bring-your-own
    /// setup) so a sandbox feels like local. See [`HomeConfig`].
    pub home: HomeConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            enabled: true,
            backend: SandboxBackend::Auto,
            default_backend: SandboxBackend::Auto,
            default_env: String::new(),
            backend_chain: [
                "podman-rootless",
                "podman-rootful",
                "docker",
                "bwrap",
                "host",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            image: String::new(),
            profile: SandboxProfile::Hardened,
            agent_profile: SandboxProfile::Sealed,
            network: Network::Nat,
            file_access: FileAccess::default(),
            ports: Vec::new(),
            gpu: None,
            limits: SandboxLimits::default(),
            volumes: std::collections::HashMap::new(),
            compose: None,
            env_passthrough: [
                "SSH_AUTH_SOCK",
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "ANTHROPIC_API_KEY",
                "TERM",
                "COLORTERM",
                "GPG_AGENT_INFO",
                "GNUPGHOME",
                "GPG_TTY",
                // Profile git identity into the sandbox (H): the profile reroot
                // points these at the profile config dirs, path-preservingly
                // mounted via `profile::sandbox_cred_mounts`. Unset on the
                // default profile ⇒ no-op there.
                "GIT_CONFIG_GLOBAL",
                "GH_CONFIG_DIR",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            auto_caches: true,
            mounts: vec![
                "~/.gitconfig:ro".into(),
                "~/.gnupg:rw".into(),
                "/run/user".into(),
            ],
            init_script: String::new(),
            prepare: Vec::new(),
            warm_direnv: WarmDirenv::Auto,
            devenv: false,
            inject_devshell: true,
            devshell: String::new(),
            nix_daemon: false,
            shell: String::new(),
            on_missing: OnMissing::Warn,
            failover: false,
            oci_host: String::new(),
            remote: RemoteConfig::default(),
            network_allow: Vec::new(),
            network_block: Vec::new(),
            network_audit: false,
            vpn: VpnConfig::default(),
            home: HomeConfig::default(),
        }
    }
}

impl SandboxConfig {
    /// Resolve `env_passthrough` into the present `(KEY, VALUE)` pairs from the
    /// host environment. Shared by the OCI spec build and the provider/remote
    /// native-exec env so the SAME secrets (GH_TOKEN, ANTHROPIC_API_KEY, …) reach
    /// EVERY placement, not just containers.
    pub fn passthrough_env(&self) -> Vec<(String, String)> {
        self.env_passthrough
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| (k.clone(), v)))
            .collect()
    }

    /// Like [`passthrough_env`](Self::passthrough_env) but for a **remote**
    /// placement (provider sprite / ssh exec into a different host): drops
    /// host-local socket/path vars that would dangle remotely (an SSH_AUTH_SOCK
    /// or GPG socket path only valid on the host). Those need forwarding (a
    /// reverse bridge), not raw passthrough.
    pub fn passthrough_env_remote(&self) -> Vec<(String, String)> {
        const HOST_LOCAL: &[&str] = &["SSH_AUTH_SOCK", "GPG_AGENT_INFO", "GNUPGHOME", "GPG_TTY"];
        let mut env: Vec<(String, String)> = self
            .passthrough_env()
            .into_iter()
            .filter(|(k, _)| !HOST_LOCAL.contains(&k.as_str()))
            // Normalize an exotic host TERM (xterm-ghostty/kitty/alacritty) to a
            // type the remote's terminfo DB actually has — otherwise `clear`/`tput`/
            // curses fail "unknown terminal type" in a fresh sandbox.
            .map(|(k, v)| {
                if k == "TERM" {
                    (k, remote_safe_term(&v))
                } else {
                    (k, v)
                }
            })
            .collect();
        // Tell the sandbox which flake devShell to enter (`[sandbox] devshell`).
        // The repo `.envrc` reads `SUPERZEJ_DEVSHELL` (`use flake .#${…:-default}`),
        // so a fresh sprite enters the lean build shell instead of the full dev
        // closure. Unset on the host → the default shell, unchanged.
        let attr = self.devshell.trim();
        if !attr.is_empty() {
            env.push(("SUPERZEJ_DEVSHELL".to_string(), attr.to_string()));
        }
        env
    }
}

/// Map a host `TERM` to a value a fresh remote/sandbox terminfo DB is sure to
/// have. Exotic terminal types (`xterm-ghostty`, `xterm-kitty`, `alacritty`, …)
/// aren't usually installed remotely, so `clear`/`tput`/ncurses error with
/// "unknown terminal type". They're all `xterm-256color`-compatible, so downgrade
/// to that; pass universally-shipped types through unchanged.
pub fn remote_safe_term(term: &str) -> String {
    const KNOWN: &[&str] = &[
        "xterm",
        "xterm-256color",
        "xterm-color",
        "screen",
        "screen-256color",
        "tmux",
        "tmux-256color",
        "vt100",
        "vt220",
        "linux",
        "ansi",
        "dumb",
    ];
    let t = term.trim();
    if t.is_empty() || KNOWN.contains(&t) {
        if t.is_empty() {
            "xterm-256color".to_string()
        } else {
            t.to_string()
        }
    } else {
        "xterm-256color".to_string()
    }
}

/// `[sandbox.home]` — the **personal environment layer**: a generic, declarative
/// description of *your* setup (independent of any repo) that superzej reproduces
/// in every sandbox/remote so it works like local. NOT Nix-coupled — tools
/// install Nix-first (consistent names) with a native package-manager fallback;
/// anything bespoke (e.g. an agent CLI, internal tooling) goes in `setup`/
/// `setup_script`. Applied by the env provisioner (see `crate::envplan`).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HomeConfig {
    /// CLI tools to install in every sandbox (e.g. `["fd", "fzf", "atuin"]`).
    /// Installed `nix profile install nixpkgs#<tool>` when Nix is present, else
    /// via the sandbox's native package manager (apt/apk/dnf, best-effort).
    pub tools: Vec<String>,
    /// Host dotfile basenames (relative to your `$HOME`) to upload into the
    /// sandbox home, e.g. `[".zshrc", ".gitconfig"]`. Missing files are skipped.
    pub dotfiles: Vec<String>,
    /// Optional dotfiles repo URL to clone in the sandbox and bootstrap (runs its
    /// `install.sh`/`bootstrap.sh`/`setup.sh` if present). Empty = disabled.
    pub dotfiles_repo: String,
    /// Inline bring-your-own setup commands run (in order) in every sandbox after
    /// the tools/dotfiles — e.g. install an agent CLI or internal tooling.
    pub setup: Vec<String>,
    /// Alternatively, a setup script (a host path uploaded + run, or an absolute
    /// in-sandbox path) executed after `setup`. Empty = disabled.
    pub setup_script: String,
    /// Coding-agent CLIs to make work out-of-the-box inside the sandbox (e.g.
    /// `["claude", "codex"]`). Known agents get installed; every listed agent's
    /// host config/credential dirs are uploaded so it's logged-in in the sandbox.
    /// Carries credentials into the remote — list only agents you trust there.
    pub agents: Vec<String>,
    /// Host services to expose INSIDE the sandbox via a reverse tunnel, so an
    /// in-sandbox process/agent reaching `127.0.0.1:<port>` hits the host's
    /// service (a host `localhost` DB/API, or a host-bound MCP server). Each entry
    /// is a [`crate::revtunnel::parse_reverse_forward`] spec: `"5432"` (same port
    /// both sides), `"8080:5432"` (host loopback port), or `"8080:db.lan:5432"`.
    pub reverse_forwards: Vec<String>,
    /// How hard to reproduce your host shell here. See [`ShellStrategy`]. Default
    /// `portable`: install tools + portable dotfiles, but SKIP (with a warning) a
    /// dotfile that hard-codes paths absent in the sandbox (e.g. a home-manager rc
    /// full of `/nix/store/…`) rather than uploading it broken.
    pub strategy: ShellStrategy,
    /// When `true` (the default), under `portable`/`tool-parity` a host dotfile
    /// that references absent store paths is skipped + warned instead of uploaded.
    /// Set `false` to force-upload every declared dotfile verbatim (you accept the
    /// breakage; a warning is still logged). `host-parity` ignores this (it makes
    /// the paths exist).
    pub portable_dotfiles_only: bool,
    /// `host-parity` only: the user's home-manager flake ref + attr (e.g.
    /// `"github:me/dotfiles#me@host"`) for the in-sandbox `home-manager switch`
    /// fallback when no binary cache is available. Empty = disabled. (Experimental;
    /// the cache-first closure copy reuses `[env.<name>.provider] binary_cache_*`.)
    pub nix_home_flake: String,
    /// Opt-in (default `false`): carry the host's atuin shell-history credentials
    /// and config into every sandbox so its history joins your atuin sync across the
    /// host and sprites, via atuin's own auto-sync. Uploads the dereferenced
    /// `~/.config/atuin/config.toml` plus the `key`/`session` auth files under
    /// `~/.local/share/atuin`, but not the history databases (the sync server
    /// reconciles those). A no-op when atuin is not installed or the host has no
    /// atuin config. Carries credentials into the remote, so enable only where
    /// you trust it.
    pub atuin: bool,
}

impl Default for HomeConfig {
    fn default() -> Self {
        HomeConfig {
            tools: Vec::new(),
            dotfiles: Vec::new(),
            dotfiles_repo: String::new(),
            setup: Vec::new(),
            setup_script: String::new(),
            agents: Vec::new(),
            reverse_forwards: Vec::new(),
            strategy: ShellStrategy::default(),
            // Safe default: drop a non-portable rc rather than ship a broken shell.
            portable_dotfiles_only: true,
            nix_home_flake: String::new(),
            atuin: false,
        }
    }
}

impl HomeConfig {
    /// Any personal-layer work declared at all? (`strategy`/`portable_dotfiles_only`
    /// alone don't count — they only shape how the declared work is applied.)
    pub fn is_enabled(&self) -> bool {
        !self.tools.is_empty()
            || !self.dotfiles.is_empty()
            || !self.dotfiles_repo.trim().is_empty()
            || !self.setup.is_empty()
            || !self.setup_script.trim().is_empty()
            || !self.agents.is_empty()
            || !self.reverse_forwards.is_empty()
            || self.atuin
    }
}

/// Per-env overlay of [`HomeConfig`] — `[env.<name>.sandbox.home]`. Only the keys
/// present override the global `[sandbox.home]`; absent keys inherit it. Lets a
/// big ssh box run `strategy = "host-parity"` while an ephemeral sprite runs
/// `strategy = "clean"`, all from one global base. Field-merged in
/// `SandboxOverlay::apply`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct HomeOverlay {
    pub tools: Option<Vec<String>>,
    pub dotfiles: Option<Vec<String>>,
    pub dotfiles_repo: Option<String>,
    pub setup: Option<Vec<String>>,
    pub setup_script: Option<String>,
    pub agents: Option<Vec<String>>,
    pub reverse_forwards: Option<Vec<String>>,
    pub strategy: Option<ShellStrategy>,
    pub portable_dotfiles_only: Option<bool>,
    pub nix_home_flake: Option<String>,
    pub atuin: Option<bool>,
}

impl HomeOverlay {
    /// No keys present? (so `SandboxOverlay::is_empty` stays accurate.)
    pub fn is_empty(&self) -> bool {
        self.tools.is_none()
            && self.dotfiles.is_none()
            && self.dotfiles_repo.is_none()
            && self.setup.is_none()
            && self.setup_script.is_none()
            && self.agents.is_none()
            && self.reverse_forwards.is_none()
            && self.strategy.is_none()
            && self.portable_dotfiles_only.is_none()
            && self.nix_home_flake.is_none()
            && self.atuin.is_none()
    }
    /// Field-merge present keys into a resolved [`HomeConfig`].
    pub fn apply(&self, base: &mut HomeConfig) {
        if let Some(v) = &self.tools {
            base.tools = v.clone();
        }
        if let Some(v) = &self.dotfiles {
            base.dotfiles = v.clone();
        }
        if let Some(v) = &self.dotfiles_repo {
            base.dotfiles_repo = v.clone();
        }
        if let Some(v) = &self.setup {
            base.setup = v.clone();
        }
        if let Some(v) = &self.setup_script {
            base.setup_script = v.clone();
        }
        if let Some(v) = &self.agents {
            base.agents = v.clone();
        }
        if let Some(v) = &self.reverse_forwards {
            base.reverse_forwards = v.clone();
        }
        if let Some(v) = self.strategy {
            base.strategy = v;
        }
        if let Some(v) = self.portable_dotfiles_only {
            base.portable_dotfiles_only = v;
        }
        if let Some(v) = &self.nix_home_flake {
            base.nix_home_flake = v.clone();
        }
        if let Some(v) = self.atuin {
            base.atuin = v;
        }
    }
}

/// Partial overlay deserialized from a repo-root `.superzej.{toml,yaml,yml,json}`
/// — only the keys present override the global `[sandbox]`. Also reused for the
/// `SUPERZEJ_SANDBOX_*` env layer.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SandboxOverlay {
    pub enabled: Option<bool>,
    pub backend: Option<SandboxBackend>,
    pub default_backend: Option<SandboxBackend>,
    pub default_env: Option<String>,
    pub backend_chain: Option<Vec<String>>,
    pub image: Option<String>,
    pub profile: Option<SandboxProfile>,
    pub agent_profile: Option<SandboxProfile>,
    pub network: Option<Network>,
    pub file_access: Option<FileAccess>,
    pub ports: Option<Vec<String>>,
    pub gpu: Option<String>,
    pub limits: Option<SandboxLimits>,
    pub volumes: Option<std::collections::HashMap<String, String>>,
    pub compose: Option<String>,
    pub env_passthrough: Option<Vec<String>>,
    pub auto_caches: Option<bool>,
    pub mounts: Option<Vec<String>>,
    pub init_script: Option<String>,
    pub prepare: Option<Vec<String>>,
    pub warm_direnv: Option<WarmDirenv>,
    pub devenv: Option<bool>,
    pub inject_devshell: Option<bool>,
    pub nix_daemon: Option<bool>,
    pub shell: Option<String>,
    pub on_missing: Option<OnMissing>,
    pub remote: Option<RemoteOverlay>,
    pub network_allow: Option<Vec<String>>,
    pub network_block: Option<Vec<String>>,
    pub network_audit: Option<bool>,
    /// Whole-table replace (matching `remote`/`limits`): a `[sandbox.vpn]` in an
    /// overlay replaces the global VPN config wholesale rather than field-merging.
    pub vpn: Option<VpnConfig>,
    /// Per-env personal layer (`[env.<name>.sandbox.home]`). Field-merged into the
    /// global `[sandbox.home]` (present keys win, absent inherit). See [`HomeOverlay`].
    pub home: Option<HomeOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct RemoteOverlay {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub transport: Option<RemoteTransport>,
    pub mode: Option<RemoteMode>,
    pub remote_dir: Option<String>,
    pub forward_agent: Option<bool>,
}

impl SandboxOverlay {
    fn apply(self, base: &mut SandboxConfig) {
        if let Some(v) = self.enabled {
            base.enabled = v;
        }
        if let Some(v) = self.backend {
            base.backend = v;
        }
        if let Some(v) = self.default_backend {
            base.default_backend = v;
        }
        if let Some(v) = self.default_env {
            base.default_env = v;
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
        if let Some(v) = self.agent_profile {
            base.agent_profile = v;
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

    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.backend.is_none()
            && self.default_backend.is_none()
            && self.default_env.is_none()
            && self.backend_chain.is_none()
            && self.image.is_none()
            && self.profile.is_none()
            && self.agent_profile.is_none()
            && self.network.is_none()
            && self.env_passthrough.is_none()
            && self.auto_caches.is_none()
            && self.mounts.is_none()
            && self.init_script.is_none()
            && self.prepare.is_none()
            && self.warm_direnv.is_none()
            && self.devenv.is_none()
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

impl RemoteOverlay {
    fn apply(self, base: &mut RemoteConfig) {
        if let Some(v) = self.host {
            base.host = v;
        }
        if let Some(v) = self.port {
            base.port = v;
        }
        if let Some(v) = self.transport {
            base.transport = v;
        }
        if let Some(v) = self.mode {
            base.mode = v;
        }
        if let Some(v) = self.remote_dir {
            base.remote_dir = v;
        }
        if let Some(v) = self.forward_agent {
            base.forward_agent = v;
        }
    }
}

/// The shape of a repo-root `.superzej.*` file: a `[sandbox]` table overlay
/// plus an optional `[keybinds]` table (the most-specific keybind layer).
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct RepoConfigFile {
    sandbox: SandboxOverlay,
    keybinds: KeybindConfig,
    /// Per-repo notification routing overlay, applied on top of global +
    /// profile (see [`Config::effective_notifications`]).
    #[serde(default)]
    notifications: NotificationsOverlay,
    /// Selects a named `[env.<name>]` for every worktree of this repo (the
    /// repo-level layer of env selection). Empty ⇒ inherit the global default.
    #[serde(default)]
    env: String,
}

/// `[drawer]` — the bottom file-manager drawer (hidden by default, toggled with
/// Ctrl+Alt+f). Runs yazi by default, with its config kept separate from the
/// system under a private `config_home`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DrawerConfig {
    /// File manager to run. Empty ⇒ the pinned yazi (`SUPERZEJ_YAZI_BIN`).
    pub command: String,
    /// `YAZI_CONFIG_HOME` for the drawer's yazi. Empty (default) ⇒ a private
    /// `<superzej-dir>/yazi`, fully separate from the user's `~/.config/yazi` and
    /// seeded with superzej's bundled config. "system" (or "none") ⇒ use the
    /// user's own yazi config (no isolation, no seeding). Any other value is used
    /// verbatim (tilde-expanded).
    pub config_home: String,
    /// Drawer height as a percentage ("35%") or a row count.
    pub height: String,
    /// Drawer width: "full" (span the terminal) or "center" (narrower, centered).
    pub width: String,
    /// Whether the bundled/private yazi config allows image preview backends.
    pub image_previews: bool,
    /// Whether the drawer shows git status as a linemode (item 606). When true,
    /// the bundled `git.yazi` plugin is seeded and registered as a fetcher in the
    /// private config; false removes the managed block. No effect on a "system"
    /// `config_home` (superzej never edits the user's own yazi config).
    pub git_status: bool,
    /// Whether drawer yazi launches should be wrapped in a user systemd scope.
    pub contain: bool,
    /// `MemoryMax` for the drawer scope. Empty = omit this property.
    pub memory_max: String,
    /// `MemorySwapMax` for the drawer scope. Empty = omit this property.
    pub memory_swap_max: String,
    /// `CPUQuota` for the drawer scope. Empty = omit this property.
    pub cpu_quota: String,
    /// Maximum hidden drawers to keep alive in native hosts. Zero disables pooling.
    pub pool_limit: usize,
    /// Whether yazi drawers may be prewarmed before the user opens them.
    pub prewarm: bool,
}

impl Default for DrawerConfig {
    fn default() -> Self {
        DrawerConfig {
            command: String::new(),
            config_home: String::new(),
            height: "35%".into(),
            width: "full".into(),
            image_previews: false,
            git_status: true,
            contain: true,
            memory_max: "2G".into(),
            memory_swap_max: "512M".into(),
            cpu_quota: "200%".into(),
            pool_limit: 1,
            prewarm: false,
        }
    }
}

/// `[notifications]` — the aggregated event bus and desktop notification
/// delivery (items 420/421/430). Events from git/agents/tests/logs are
/// surfaced as sidebar badges and (optionally) OS desktop notifications.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Whether to deliver OS desktop notifications (via `notify-send` on Linux).
    /// When false, events still flow to the in-app inbox + sidebar badges.
    pub desktop: bool,
    /// Minimum urgency that triggers a desktop notification: `"low"`,
    /// `"normal"`, or `"critical"`. Lower-urgency events are recorded in the
    /// inbox but never pop a desktop toast.
    pub desktop_min_urgency: String,
    /// How non-agent pane exits route into the attention model (item 524):
    /// `"failures_and_tasks"` (default — crashes + non-shell task completions),
    /// `"failures"` (only non-zero exits), `"all"` (every exit incl. clean
    /// shells), or `"off"`.
    pub process_exit: String,
    /// Per-kind attention priority overrides: maps a notification kind
    /// (snake_case, e.g. `"agent_done"`) to `"alert"`, `"notice"`, or `"info"`.
    /// Unset kinds use their built-in `NotificationKind::default_priority`;
    /// unknown keys/values are ignored. `alert` raises the red flag, `notice`
    /// the neutral unread count, `info` is inbox-only (never counted).
    pub priority: std::collections::BTreeMap<String, String>,
    /// Ordered user routing rules (item 420). Each rule matches on any subset of
    /// selectors (kind/worktree/source/message/priority/mode/profile) and acts by
    /// overriding priority, restricting channels, muting, dropping, or setting a
    /// sound. Evaluated top-to-bottom by [`crate::notification_route::decide`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<NotificationRule>,
    /// Do-not-disturb / quiet-hours (item 426): suppress ephemeral channels
    /// (desktop/toast/sound) for notifications below `allow_priority` during a
    /// configured window or when toggled on at runtime. The inbox always records.
    pub dnd: DndConfig,
    /// Audible sound/bell channel (item 429): terminal `BEL` (default), a
    /// configured command, or off — gated by `min_priority`.
    pub sound: SoundConfig,
    /// Named routing modes (item 427): a rule with a `modes` selector only
    /// applies when the active mode is listed. Values are presets you switch
    /// between at runtime (e.g. `focus`, `away`). The map value carries an
    /// optional human label; membership is what matters.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub modes: std::collections::BTreeMap<String, NotificationMode>,
    /// The routing mode active at startup (`""` ⇒ no mode / the default set of
    /// rules with an empty `modes` selector). Switchable at runtime.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub active_mode: String,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        NotificationsConfig {
            desktop: true,
            desktop_min_urgency: "normal".into(),
            process_exit: "failures_and_tasks".into(),
            priority: std::collections::BTreeMap::new(),
            rules: Vec::new(),
            dnd: DndConfig::default(),
            sound: SoundConfig::default(),
            modes: std::collections::BTreeMap::new(),
            active_mode: String::new(),
        }
    }
}

impl NotificationsConfig {
    /// Effective priority of a kind: a valid config override wins, else the kind's
    /// built-in default. Garbage override values fall through to the default.
    pub fn priority_of(
        &self,
        kind: crate::notification::NotificationKind,
    ) -> crate::notification::Priority {
        self.priority
            .get(kind.as_str())
            .and_then(|s| crate::notification::Priority::parse(s))
            .unwrap_or_else(|| kind.default_priority())
    }

    /// The snake_case names of kinds whose effective priority is `>= min`.
    pub fn kind_names_at_or_above(&self, min: crate::notification::Priority) -> Vec<&'static str> {
        crate::notification::NotificationKind::ALL
            .into_iter()
            .filter(|k| self.priority_of(*k).rank() >= min.rank())
            .map(|k| k.as_str())
            .collect()
    }

    /// Kinds that raise the red ⚑ flag (effective priority `Alert`). Feeds the
    /// alert-count query.
    pub fn alert_kind_names(&self) -> Vec<&'static str> {
        self.kind_names_at_or_above(crate::notification::Priority::Alert)
    }

    /// Kinds that count toward the neutral unread badge (effective priority
    /// `Notice` or above — i.e. everything except `Info`). Feeds the unread-count
    /// query so informational kinds are never counted.
    pub fn counted_unread_kind_names(&self) -> Vec<&'static str> {
        self.kind_names_at_or_above(crate::notification::Priority::Notice)
    }

    /// True when routing rules are present. The host uses this to decide between
    /// the SQL kind-level count fast path (no rules) and rule-aware Rust
    /// aggregation over loaded rows.
    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }
}

/// serde `skip_serializing_if` helper for `bool` fields (skips `false`).
fn is_false(b: &bool) -> bool {
    !*b
}

config_enum! {
    /// `[notifications.sound] mode` — how the audible cue is produced. `bell`
    /// writes a terminal `BEL` on the next render flush; `command` runs a
    /// configured command off-thread; `off` is silent.
    pub enum SoundMode: "notification sound mode" {
        Off = "off" | "none" | "silent",
        Bell = "bell" | "beep" | "terminal",
        Command = "command" | "cmd" | "exec",
    } default = Bell;
}

/// One user routing rule (`[[notifications.rules]]`, item 420). All present
/// selectors must match for the rule to fire (absent selectors are wildcards);
/// the action then reshapes the [`crate::notification_route::RouteDecision`].
/// Matching + regex/glob compilation live in `notification_route.rs`; this is
/// pure config data.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationRule {
    /// Optional human note (ignored by matching).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub name: String,
    // --- selectors ---
    /// Match a single kind (snake_case, e.g. `"test_failed"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Match any of these kinds (union with `kind`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// Glob over the notification's `worktree_path` (`*` any run, `?` any char).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Prefix match on `source_ref` (e.g. `"linear:"`, `"pr:"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Regex matched against the message text (unanchored).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Only fire when the (base) effective priority is `>=` this
    /// (`"info"`/`"notice"`/`"alert"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_priority: Option<String>,
    /// Only fire when the active routing mode is one of these (empty = any mode).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<String>,
    /// Only fire under this active profile (empty = any profile).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    // --- actions ---
    /// Override the effective priority (`"info"`/`"notice"`/`"alert"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub set_priority: Option<String>,
    /// Restrict delivery to this channel subset. Values from
    /// `inbox`/`desktop`/`toast`/`sound`. `None` ⇒ leave channels at default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<Vec<String>>,
    /// Suppress every ephemeral channel (desktop/toast/sound); inbox still records.
    #[serde(skip_serializing_if = "is_false")]
    pub mute: bool,
    /// Drop entirely — no inbox record, no delivery.
    #[serde(skip_serializing_if = "is_false")]
    pub drop: bool,
    /// Override the sound: `"bell"`, `"off"`, or a command string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound: Option<String>,
    /// Stop evaluating further rules after this one matches.
    #[serde(skip_serializing_if = "is_false")]
    pub stop: bool,
}

/// `[notifications.dnd]` — do-not-disturb / quiet hours (item 426).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DndConfig {
    /// Startup state of the manual toggle. The runtime toggle overrides the
    /// schedule; this seeds it.
    pub enabled: bool,
    /// Quiet windows, each `"HH:MM-HH:MM"` with an optional leading weekday token
    /// (`"Sat"`, `"mon-fri"`); ranges may wrap past midnight (`"22:00-08:00"`).
    /// Empty ⇒ no scheduled DND (only the manual toggle applies).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub windows: Vec<String>,
    /// Notifications at or above this priority still deliver during DND
    /// (`"info"`/`"notice"`/`"alert"`, default `"alert"`).
    pub allow_priority: String,
}

impl Default for DndConfig {
    fn default() -> Self {
        DndConfig {
            enabled: false,
            windows: Vec::new(),
            allow_priority: "alert".into(),
        }
    }
}

/// `[notifications.sound]` — the audible cue (item 429).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SoundConfig {
    /// How to produce the cue: `bell` (default), `command`, or `off`.
    pub mode: SoundMode,
    /// Minimum effective priority that makes a sound (`"info"`/`"notice"`/
    /// `"alert"`, default `"alert"`).
    pub min_priority: String,
    /// Command for `mode = "command"` (run best-effort, off-thread). A literal
    /// command line, e.g. `"paplay /usr/share/sounds/alert.oga"`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub command: String,
    /// Optional per-priority command overrides (keys `info`/`notice`/`alert`),
    /// consulted before `command` when `mode = "command"`.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub per_priority: std::collections::BTreeMap<String, String>,
}

impl Default for SoundConfig {
    fn default() -> Self {
        SoundConfig {
            mode: SoundMode::Bell,
            min_priority: "alert".into(),
            command: String::new(),
            per_priority: std::collections::BTreeMap::new(),
        }
    }
}

/// `[notifications.modes.<name>]` — a named routing mode (item 427). Currently
/// just an optional label; membership drives rule `modes` selectors.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationMode {
    /// Human label for the status chip / palette (defaults to the map key).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// `[profiles.<p>.notifications]` — per-profile routing overlay (item 427).
/// Present fields replace the corresponding global `[notifications]` fields for
/// the active profile; absent fields inherit. Mirrors [`SandboxOverlay`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NotificationsOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub desktop_min_urgency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_exit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<NotificationRule>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dnd: Option<DndConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound: Option<SoundConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<std::collections::BTreeMap<String, NotificationMode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_mode: Option<String>,
}

impl NotificationsOverlay {
    /// True when nothing is set — lets `ProfileConfig` skip serialization.
    pub fn is_empty(&self) -> bool {
        self.desktop.is_none()
            && self.desktop_min_urgency.is_none()
            && self.process_exit.is_none()
            && self.priority.is_none()
            && self.rules.is_none()
            && self.dnd.is_none()
            && self.sound.is_none()
            && self.modes.is_none()
            && self.active_mode.is_none()
    }

    /// Apply present fields onto `base` (present wins, absent inherits).
    pub fn apply(self, base: &mut NotificationsConfig) {
        if let Some(v) = self.desktop {
            base.desktop = v;
        }
        if let Some(v) = self.desktop_min_urgency {
            base.desktop_min_urgency = v;
        }
        if let Some(v) = self.process_exit {
            base.process_exit = v;
        }
        if let Some(v) = self.priority {
            base.priority = v;
        }
        if let Some(v) = self.rules {
            base.rules = v;
        }
        if let Some(v) = self.dnd {
            base.dnd = v;
        }
        if let Some(v) = self.sound {
            base.sound = v;
        }
        if let Some(v) = self.modes {
            base.modes = v;
        }
        if let Some(v) = self.active_mode {
            base.active_mode = v;
        }
    }
}

/// `[strip]` — the top pinned-program strip (a horizontal band above the center
/// rendering live `location = "strip"` pins side by side). Hidden when empty;
/// toggled with Ctrl+Alt+t and resized at runtime.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StripConfig {
    /// Fraction of the center band the strip occupies (0.05–0.9, default 0.2).
    pub ratio: f32,
    /// Whether the strip is shown when it has at least one pin (default true).
    pub visible: bool,
}

impl Default for StripConfig {
    fn default() -> Self {
        StripConfig {
            ratio: 0.2,
            visible: true,
        }
    }
}

impl StripConfig {
    /// The configured ratio clamped to a sane band so the center always survives.
    pub fn clamped_ratio(&self) -> f32 {
        self.ratio.clamp(0.05, 0.9)
    }
}

/// `[search]` — incremental pane-history and global search.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SearchConfig {
    /// Lines of terminal output kept per pane for searching. A larger value
    /// lets you search further back in history at the cost of a small amount of
    /// heap per pane.
    pub history_lines: usize,
    /// Maximum number of fuzzy-matched results returned per search. Capped at
    /// the UI renderer's visible row count; higher values are just sorted but
    /// not all drawn.
    pub max_results: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig {
            history_lines: 10_000,
            max_results: 1_000,
        }
    }
}

/// `[lsp]` — language-server integration (symbols, navigation, hover,
/// diagnostics). Servers start lazily on first use and stay warm per worktree.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LspConfig {
    /// Master switch. When `false`, no servers start and LSP features fall back
    /// to the tree-sitter / regex providers.
    pub enabled: bool,
    /// Show hover/signature previews (the floating popup).
    pub hover: bool,
    /// Per-language server command overrides. An entry's `command = ""` disables
    /// that language; omitted languages use the built-in default (`rust-analyzer`,
    /// `typescript-language-server`, `pyright-langserver`, `gopls`), used only
    /// when found on `PATH`.
    pub servers: Vec<LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        LspConfig {
            enabled: true,
            hover: true,
            servers: Vec::new(),
        }
    }
}

/// One `[[lsp.servers]]` override.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema, Default)]
#[serde(default)]
pub struct LspServerConfig {
    /// Language key: `"rust"`, `"typescript"`, `"tsx"`, `"javascript"`,
    /// `"python"`, or `"go"`.
    pub lang: String,
    /// Server executable (looked up on `PATH` if it has no `/`). `""` disables.
    pub command: String,
    /// Arguments passed to the server (e.g. `["--stdio"]`).
    pub args: Vec<String>,
}

/// `[palette]` — Search Everywhere palette behavior and result caps.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PaletteConfig {
    /// Maximum content search matches across all files (streamed, capped at this).
    pub content_max_results: usize,
    /// Maximum file-path results returned after fuzzy ranking.
    pub file_max_results: usize,
    /// Maximum symbol matches returned.
    pub symbol_max_results: usize,
    /// Include hidden files (dotfiles, .gitignored paths) in the file index.
    pub content_search_hidden: bool,
}

impl Default for PaletteConfig {
    fn default() -> Self {
        PaletteConfig {
            content_max_results: 500,
            file_max_results: 200,
            symbol_max_results: 100,
            content_search_hidden: false,
        }
    }
}

/// `[panel]` — the right accordion panel.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PanelConfig {
    /// Section display order, by key (`"changes"`, `"git"`, `"files"`,
    /// `"tests"`, `"debug"`, `"sandbox"`, `"db"`, `"telemetry"`, `"keys"`).
    /// Sections omitted from the list are hidden; an empty list (the default)
    /// shows every section in its built-in order. Unknown keys are ignored.
    pub sections: Vec<String>,
    /// When Esc returns focus to the center terminal from any chrome zone,
    /// snap the right panel back to its default (Normal) width and close the
    /// bottom file drawer. `false` leaves both exactly as the user left them.
    #[serde(default = "default_true")]
    pub collapse_on_escape: bool,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            sections: Vec::new(),
            collapse_on_escape: true,
        }
    }
}

/// Default `keymap_preset` (no IDE overlay). A free function so both the field
/// default and the `skip_serializing_if` predicate agree.
fn default_preset() -> String {
    "default".into()
}

fn is_default_preset(s: &str) -> bool {
    s.is_empty() || s == "default"
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
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            size: 0,
            max_idle_secs: 600,
        }
    }
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
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    // --- scalar values (must serialize before any sub-table for TOML) ---
    pub worktrees_dir: String,
    pub workspaces_dir: String,
    pub base_branch: String,
    pub window_margin: usize,
    pub branch_prefix: String,
    pub picker: Picker,
    pub worktree_mode: WorktreeMode,
    pub name_scheme: NameScheme,
    pub auto_remove_worktree: bool,
    /// Ask before destructive worktree actions (deleting a worktree from
    /// disk via the sidebar). Set `false` to act immediately.
    pub confirm_delete: bool,
    pub repo_roots: Vec<String>,
    pub repo_scan_depth: usize,
    /// Active keybind profile name (`""` ⇒ none). Selects `[profiles.<name>]`;
    /// overridden by `SUPERZEJ_PROFILE` / `--profile`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub profile: String,
    /// IDE keymap preset overlaid on the built-in defaults (item 621):
    /// `"default"` (none), `"vscode"`, or `"jetbrains"`. Familiar chords mapped
    /// to existing host actions; user `[keybinds]` still override it. The
    /// `vim`/`emacs` mode overlays are separate (see `[profiles]`/`default_mode`).
    #[serde(skip_serializing_if = "is_default_preset")]
    pub keymap_preset: String,
    // --- arrays of tables (must serialize before any plain sub-table) ---
    pub agents: Vec<NamedCommand>,
    pub tools: Vec<NamedCommand>,
    /// Coding-agent subscription accounts (`[[accounts]]`) for client-side
    /// account switching. See [`crate::account`].
    pub accounts: Vec<Account>,
    pub pins: Vec<Pin>,
    pub tasks: Vec<Task>,
    pub worktree_templates: Vec<WorktreeTemplate>,
    pub actions: Vec<CustomAction>,
    pub git_commands: Vec<GitCommand>,
    pub plugins: Vec<crate::plugin_api::PluginManifest>,
    // --- sub-tables ---
    #[serde(default)]
    pub ui: UiConfig,
    pub git: GitConfig,
    pub theme: ThemeConfig,
    pub monitor: MonitorConfig,
    pub stats: StatsConfig,
    pub metrics: MetricsConfig,
    pub apps: AppsConfig,
    pub bars: BarsConfig,
    pub pr: PrConfig,
    pub issues: IssuesConfig,
    /// `[ci]` — cross-provider CI/CD inspection (AV group).
    pub ci: CiConfig,
    pub watch: WatchConfig,
    pub log: LogConfig,
    pub sandbox: SandboxConfig,
    pub limits: LimitsConfig,
    /// `[disk]` — disk-usage visibility, cleanup, and shared build caches.
    pub disk: DiskConfig,
    pub drawer: DrawerConfig,
    pub notifications: NotificationsConfig,
    pub strip: StripConfig,
    pub panel: PanelConfig,
    pub search: SearchConfig,
    pub palette: PaletteConfig,
    pub lsp: LspConfig,
    /// The LLM proxy daemon (`[llm_proxy]`). Disabled by default — AI is additive.
    pub llm_proxy: LlmProxyConfig,
    /// `[merge_queue]` — the local fold-actor (parallel-branch integration).
    /// On by default; the core is AI-free (agent handoff only fires on conflict).
    pub merge_queue: MergeQueueConfig,
    /// `[replay]` — per-pane time-travel recording + scrub/search (`Alt+r`). On
    /// by default, bounded 8 MiB / 30 m per pane; free when disabled.
    pub replay: ReplayConfig,
    /// `[media]` — media-player control. On by default (`mpris` backend), inert
    /// where D-Bus/`playerctl` are absent. Additive — the shell never depends on it.
    pub media: MediaConfig,
    /// `[share]` — expose a worktree port at a public URL. Disabled by default.
    pub share: ShareConfig,
    /// `[forward]` — auto-forward sandbox-internal dev-server ports to the host's
    /// loopback for browser preview. On by default (loopback-only ⇒ safe).
    pub forward: ForwardConfig,
    /// `[lifecycle]` — budget-governed warm/suspend policy for managed-provider
    /// sandboxes (keep recently-used ones warm for fast resume; let idle ones
    /// suspend; provision ahead of focus). Budget-safe defaults.
    pub lifecycle: LifecycleConfig,
    /// Rebind a built-in action by id, e.g. `new-worktree = "Ctrl w"`. The flat
    /// table is the global/default layer; nested mode tables are native-host only.
    pub keybinds: KeybindConfig,
    /// Named keybind profiles (`[profiles.<name>]`), selected by `profile`.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub profiles: std::collections::BTreeMap<String, ProfileConfig>,
    /// Per-workspace config keyed by repo slug (`[workspace.<slug>]`).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub workspace: std::collections::BTreeMap<String, WorkspaceConfig>,
    /// Named execution environments (`[env.<name>]`) — the reusable library a
    /// workspace/repo/worktree selects from. See [`Config::resolve_env`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, EnvConfig>,
    /// Named environment bundles (`[bundle.<name>]`) — soft work/personal
    /// identities bound per scope and injected at every pane spawn. See
    /// [`crate::bundle`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bundle: std::collections::BTreeMap<String, Bundle>,
    /// Per-tool binary overrides (`[managed_tools.<name>]`) for the managed-tool
    /// resolver: an explicit `path` (highest-priority tier) + optional `args`,
    /// consulted before PATH lookup and the managed download. See
    /// [`crate::managed_tool`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub managed_tools: std::collections::BTreeMap<String, crate::managed_tool::ToolOverride>,
    /// User-declared MCP servers (`[mcp_servers.<name>]`) the agent consumes,
    /// acquired via the managed-tool resolver and gated by capability grants.
    /// See [`crate::mcp::config`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub mcp_servers: std::collections::BTreeMap<String, crate::mcp::config::McpServerConfig>,
    /// `[secrets.resolvers]` — external secret-resolver commands used to expand
    /// `<scheme>:<ref>` bundle values at launch without persisting the secret.
    #[serde(skip_serializing_if = "SecretsConfig::is_empty")]
    pub secrets: SecretsConfig,
    /// Per-program host-action overlays (`[program_keybinds.<program>]`), keyed
    /// by the focused pane's program name. Consulted before the active mode.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub program_keybinds: std::collections::BTreeMap<String, KeybindConfig>,
    /// Per-program key-injection remaps (`[program_remap.<program>] "Alt j" = "j"`).
    /// When the program is focused and the LHS chord is not claimed as a host
    /// action, the RHS key bytes are written into the pane instead.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub program_remap:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

impl Default for Config {
    fn default() -> Self {
        let home = util::home();
        Config {
            // Under superzej's root (honors SUPERZEJ_DIR) so a dev/test instance
            // gets its own worktrees, isolated from the daily-driver instance.
            worktrees_dir: util::superzej_dir()
                .join("worktrees")
                .to_string_lossy()
                .into_owned(),
            workspaces_dir: home.join("code").to_string_lossy().into_owned(),
            base_branch: "auto".into(),
            window_margin: 0,
            branch_prefix: "sz/".into(),
            picker: Picker::Auto,
            worktree_mode: WorktreeMode::Global,
            name_scheme: NameScheme::Words,
            auto_remove_worktree: false,
            confirm_delete: true,
            repo_roots: Vec::new(),
            repo_scan_depth: 5,
            ui: UiConfig::default(),
            agents: Vec::new(),
            tools: Vec::new(),
            accounts: Vec::new(),
            pins: Vec::new(),
            tasks: Vec::new(),
            worktree_templates: Vec::new(),
            git_commands: Vec::new(),
            plugins: Vec::new(),
            git: GitConfig::default(),
            theme: ThemeConfig::default(),
            monitor: MonitorConfig::default(),
            stats: StatsConfig::default(),
            metrics: MetricsConfig::default(),
            apps: AppsConfig::default(),
            bars: BarsConfig::default(),
            pr: PrConfig::default(),
            issues: IssuesConfig::default(),
            ci: CiConfig::default(),
            watch: WatchConfig::default(),
            log: LogConfig::default(),
            sandbox: SandboxConfig::default(),
            limits: LimitsConfig::default(),
            disk: DiskConfig::default(),
            drawer: DrawerConfig::default(),
            notifications: NotificationsConfig::default(),
            strip: StripConfig::default(),
            panel: PanelConfig::default(),
            search: SearchConfig::default(),
            palette: PaletteConfig::default(),
            lsp: LspConfig::default(),
            llm_proxy: LlmProxyConfig::default(),
            merge_queue: MergeQueueConfig::default(),
            replay: ReplayConfig::default(),
            media: MediaConfig::default(),
            share: ShareConfig::default(),
            forward: ForwardConfig::default(),
            lifecycle: LifecycleConfig::default(),
            keybinds: KeybindConfig::default(),
            actions: Vec::new(),
            profile: String::new(),
            keymap_preset: default_preset(),
            profiles: std::collections::BTreeMap::new(),
            workspace: std::collections::BTreeMap::new(),
            env: std::collections::BTreeMap::new(),
            bundle: std::collections::BTreeMap::new(),
            managed_tools: std::collections::BTreeMap::new(),
            mcp_servers: std::collections::BTreeMap::new(),
            secrets: SecretsConfig::default(),
            program_keybinds: std::collections::BTreeMap::new(),
            program_remap: std::collections::BTreeMap::new(),
        }
    }
}

/// A source of environment variables — abstracted so the layering is testable
/// without touching the real process environment.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub struct ProcessEnv;
impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|s| !s.trim().is_empty())
    }
}

/// An in-memory environment (for tests).
#[cfg(test)]
#[derive(Default, schemars::JsonSchema)]
pub struct MapEnv(pub BTreeMap<String, String>);
#[cfg(test)]
impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned().filter(|s| !s.trim().is_empty())
    }
}

/// An all-`Option` mirror of [`Config`] used for the env and CLI-flag layers.
/// `apply` writes only the set fields onto a base, so each layer overrides the
/// one below it.
#[derive(Debug, Default, Clone, schemars::JsonSchema)]
pub struct ConfigOverlay {
    pub worktrees_dir: Option<String>,
    pub workspaces_dir: Option<String>,
    pub base_branch: Option<String>,
    pub window_margin: Option<usize>,
    pub branch_prefix: Option<String>,
    pub picker: Option<Picker>,
    pub worktree_mode: Option<WorktreeMode>,
    pub name_scheme: Option<NameScheme>,
    pub auto_remove_worktree: Option<bool>,
    pub repo_scan_depth: Option<usize>,
    pub profile: Option<String>,
    pub accent: Option<String>,
    pub focus_border: Option<String>,
    pub frame_border: Option<String>,
    pub theme_color: Option<ColorMode>,
    pub theme_glyphs: Option<GlyphMode>,
    pub pr_ttl_secs: Option<u64>,
    pub watch_pr_interval_secs: Option<u64>,
    pub metrics_interval_secs: Option<f64>,
    pub metrics_timeout_ms: Option<u64>,
    pub metrics_max_body_bytes: Option<usize>,
    pub apps_default_tab: Option<String>,
    pub apps_tab_order: Option<Vec<String>>,
    pub log_level: Option<LogLevel>,
    pub log_file: Option<bool>,
    pub log_dir: Option<String>,
    pub log_rotation_size_mb: Option<u64>,
    pub log_max_files: Option<usize>,
    pub log_format: Option<LogFormat>,
    pub disk_show_sizes: Option<bool>,
    pub disk_warn_threshold_gb: Option<u64>,
    pub disk_scan_interval_secs: Option<u64>,
    pub disk_auto_clean_on_merge: Option<bool>,
    pub disk_clean_on_pr_closed: Option<bool>,
    pub disk_sccache: Option<bool>,
    pub disk_sccache_dir: Option<String>,
    pub disk_shared_target_dir: Option<String>,
    pub sandbox: SandboxOverlay,
}

impl ConfigOverlay {
    fn apply(self, base: &mut Config) {
        macro_rules! set {
            ($field:expr, $val:expr) => {
                if let Some(v) = $val {
                    $field = v;
                }
            };
        }
        set!(base.worktrees_dir, self.worktrees_dir);
        set!(base.workspaces_dir, self.workspaces_dir);
        set!(base.base_branch, self.base_branch);
        set!(base.window_margin, self.window_margin);
        set!(base.branch_prefix, self.branch_prefix);
        set!(base.picker, self.picker);
        set!(base.worktree_mode, self.worktree_mode);
        set!(base.name_scheme, self.name_scheme);
        set!(base.auto_remove_worktree, self.auto_remove_worktree);
        set!(base.repo_scan_depth, self.repo_scan_depth);
        set!(base.profile, self.profile);
        set!(base.theme.accent, self.accent);
        set!(base.theme.focus_border, self.focus_border);
        set!(base.theme.color, self.theme_color);
        set!(base.theme.glyphs, self.theme_glyphs);
        if self.frame_border.is_some() {
            base.theme.colors.border = self.frame_border;
        }
        set!(base.pr.ttl_secs, self.pr_ttl_secs);
        set!(base.watch.pr_interval_secs, self.watch_pr_interval_secs);
        set!(base.metrics.interval_secs, self.metrics_interval_secs);
        set!(base.metrics.timeout_ms, self.metrics_timeout_ms);
        set!(base.metrics.max_body_bytes, self.metrics_max_body_bytes);
        set!(base.apps.default_tab, self.apps_default_tab);
        set!(base.apps.tab_order, self.apps_tab_order);
        set!(base.log.level, self.log_level);
        set!(base.log.file, self.log_file);
        set!(base.log.dir, self.log_dir);
        set!(base.log.rotation_size_mb, self.log_rotation_size_mb);
        set!(base.log.max_files, self.log_max_files);
        set!(base.log.format, self.log_format);
        set!(base.disk.show_sizes, self.disk_show_sizes);
        set!(base.disk.warn_threshold_gb, self.disk_warn_threshold_gb);
        set!(base.disk.scan_interval_secs, self.disk_scan_interval_secs);
        set!(base.disk.auto_clean_on_merge, self.disk_auto_clean_on_merge);
        set!(base.disk.clean_on_pr_closed, self.disk_clean_on_pr_closed);
        set!(base.disk.sccache, self.disk_sccache);
        set!(base.disk.sccache_dir, self.disk_sccache_dir);
        set!(base.disk.shared_target_dir, self.disk_shared_target_dir);
        if !self.sandbox.is_empty() {
            self.sandbox.apply(&mut base.sandbox);
        }
    }
}

/// Read the `SUPERZEJ_<SECTION>_<KEY>` env layer. Each knob is one line here —
/// this is the single place to extend when a new setting becomes env-settable.
/// Deprecated `SZ_*` names are honored as a fallback with a one-time warning.
pub fn env_overlay(env: &dyn EnvSource) -> ConfigOverlay {
    let mut o = ConfigOverlay::default();

    // Helper that warns-and-skips on a malformed number (never blocks).
    let parse_num = |raw: String, key: &str| -> Option<u64> {
        match raw.trim().parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                config_warn(&format!("{key}: not a number ({raw:?}); ignoring"));
                None
            }
        }
    };
    let parse_float = |raw: String, key: &str| -> Option<f64> {
        match raw.trim().parse::<f64>() {
            Ok(n) if n.is_finite() => Some(n),
            _ => {
                config_warn(&format!("{key}: not a finite number ({raw:?}); ignoring"));
                None
            }
        }
    };
    let parse_list = |raw: String| -> Vec<String> {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };

    o.worktrees_dir = env.get("SUPERZEJ_WORKTREES_DIR");
    o.workspaces_dir = env.get("SUPERZEJ_WORKSPACES_DIR");
    o.base_branch = env.get("SUPERZEJ_BASE_BRANCH");
    o.branch_prefix = env.get("SUPERZEJ_BRANCH_PREFIX");
    if let Some(v) = env.get("SUPERZEJ_PICKER") {
        o.picker = Picker::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_WORKTREE_MODE") {
        o.worktree_mode = WorktreeMode::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_NAME_SCHEME") {
        o.name_scheme = NameScheme::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_AUTO_REMOVE_WORKTREE") {
        o.auto_remove_worktree = parse_bool(&v, "SUPERZEJ_AUTO_REMOVE_WORKTREE");
    }
    if let Some(v) = env.get("SUPERZEJ_REPO_SCAN_DEPTH") {
        o.repo_scan_depth = parse_num(v, "SUPERZEJ_REPO_SCAN_DEPTH").map(|n| n as usize);
    }
    o.profile = env.get("SUPERZEJ_PROFILE");
    o.accent = env.get("SUPERZEJ_THEME_ACCENT");
    o.focus_border = env.get("SUPERZEJ_THEME_FOCUS_BORDER");
    o.frame_border = env.get("SUPERZEJ_THEME_BORDER");
    if let Some(v) = env.get("SUPERZEJ_THEME_COLOR") {
        o.theme_color = ColorMode::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_THEME_GLYPHS") {
        o.theme_glyphs = GlyphMode::from_str_validated(v.trim()).ok();
    }

    // [pr] — SUPERZEJ_PR_TTL, with deprecated SZ_PR_TTL fallback.
    if let Some(v) = env.get("SUPERZEJ_PR_TTL") {
        o.pr_ttl_secs = parse_num(v, "SUPERZEJ_PR_TTL");
    } else if let Some(v) = env.get("SZ_PR_TTL") {
        config_warn("SZ_PR_TTL is deprecated; use SUPERZEJ_PR_TTL");
        o.pr_ttl_secs = parse_num(v, "SZ_PR_TTL");
    }
    if let Some(v) = env.get("SUPERZEJ_WATCH_PR_INTERVAL") {
        o.watch_pr_interval_secs = parse_num(v, "SUPERZEJ_WATCH_PR_INTERVAL");
    }

    // [metrics]
    if let Some(v) = env.get("SUPERZEJ_METRICS_INTERVAL_SECS") {
        o.metrics_interval_secs = parse_float(v, "SUPERZEJ_METRICS_INTERVAL_SECS");
    }
    if let Some(v) = env.get("SUPERZEJ_METRICS_TIMEOUT_MS") {
        o.metrics_timeout_ms = parse_num(v, "SUPERZEJ_METRICS_TIMEOUT_MS");
    }
    if let Some(v) = env.get("SUPERZEJ_METRICS_MAX_BODY_BYTES") {
        o.metrics_max_body_bytes =
            parse_num(v, "SUPERZEJ_METRICS_MAX_BODY_BYTES").map(|n| n as usize);
    }

    // [apps]
    o.apps_default_tab = env.get("SUPERZEJ_APPS_DEFAULT_TAB");
    if let Some(v) = env.get("SUPERZEJ_APPS_TAB_ORDER") {
        o.apps_tab_order = Some(parse_list(v));
    }

    // [log]
    if let Some(v) = env.get("SUPERZEJ_LOG_LEVEL") {
        o.log_level = LogLevel::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_LOG_FILE") {
        o.log_file = parse_bool(&v, "SUPERZEJ_LOG_FILE");
    }
    o.log_dir = env.get("SUPERZEJ_LOG_DIR");
    if let Some(v) = env.get("SUPERZEJ_LOG_ROTATION_SIZE_MB") {
        o.log_rotation_size_mb = parse_num(v, "SUPERZEJ_LOG_ROTATION_SIZE_MB");
    }
    if let Some(v) = env.get("SUPERZEJ_LOG_MAX_FILES") {
        o.log_max_files = parse_num(v, "SUPERZEJ_LOG_MAX_FILES").map(|n| n as usize);
    }
    if let Some(v) = env.get("SUPERZEJ_LOG_FORMAT") {
        o.log_format = LogFormat::from_str_validated(v.trim()).ok();
    }

    // [disk]
    if let Some(v) = env.get("SUPERZEJ_DISK_SHOW_SIZES") {
        o.disk_show_sizes = parse_bool(&v, "SUPERZEJ_DISK_SHOW_SIZES");
    }
    if let Some(v) = env.get("SUPERZEJ_DISK_WARN_THRESHOLD_GB") {
        o.disk_warn_threshold_gb = parse_num(v, "SUPERZEJ_DISK_WARN_THRESHOLD_GB");
    }
    if let Some(v) = env.get("SUPERZEJ_DISK_SCAN_INTERVAL_SECS") {
        o.disk_scan_interval_secs = parse_num(v, "SUPERZEJ_DISK_SCAN_INTERVAL_SECS");
    }
    if let Some(v) = env.get("SUPERZEJ_DISK_AUTO_CLEAN_ON_MERGE") {
        o.disk_auto_clean_on_merge = parse_bool(&v, "SUPERZEJ_DISK_AUTO_CLEAN_ON_MERGE");
    }
    if let Some(v) = env.get("SUPERZEJ_DISK_CLEAN_ON_PR_CLOSED") {
        o.disk_clean_on_pr_closed = parse_bool(&v, "SUPERZEJ_DISK_CLEAN_ON_PR_CLOSED");
    }
    if let Some(v) = env.get("SUPERZEJ_DISK_SCCACHE") {
        o.disk_sccache = parse_bool(&v, "SUPERZEJ_DISK_SCCACHE");
    }
    o.disk_sccache_dir = env.get("SUPERZEJ_DISK_SCCACHE_DIR");
    o.disk_shared_target_dir = env.get("SUPERZEJ_DISK_SHARED_TARGET_DIR");

    // [sandbox]
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_BACKEND") {
        o.sandbox.backend = SandboxBackend::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_NETWORK") {
        o.sandbox.network = Network::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_PROFILE") {
        o.sandbox.profile = SandboxProfile::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_AGENT_PROFILE") {
        o.sandbox.agent_profile = SandboxProfile::from_str_validated(v.trim()).ok();
    }
    o.sandbox.image = env.get("SUPERZEJ_SANDBOX_IMAGE");
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_ON_MISSING") {
        o.sandbox.on_missing = OnMissing::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_ENABLED") {
        o.sandbox.enabled = parse_bool(&v, "SUPERZEJ_SANDBOX_ENABLED");
    }
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_INJECT_DEVSHELL") {
        o.sandbox.inject_devshell = parse_bool(&v, "SUPERZEJ_SANDBOX_INJECT_DEVSHELL");
    }
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_NIX_DAEMON") {
        o.sandbox.nix_daemon = parse_bool(&v, "SUPERZEJ_SANDBOX_NIX_DAEMON");
    }
    if let Some(v) = env.get("SUPERZEJ_SANDBOX_WARM_DIRENV") {
        o.sandbox.warm_direnv = WarmDirenv::from_str_validated(v.trim()).ok();
    }
    if let Some(host) = env.get("SUPERZEJ_SANDBOX_REMOTE_HOST") {
        o.sandbox.remote = Some(RemoteOverlay {
            host: Some(host),
            ..Default::default()
        });
    }
    o
}

/// Recursively merge `overlay` into `base` (both JSON): objects merge key-wise
/// (recursing), any other value replaces. The primitive behind config profile
/// overlays — a key the overlay omits keeps the base value.
fn deep_merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                deep_merge_json(b.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (b, o) => *b = o,
    }
}

fn parse_bool(raw: &str, key: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        other => {
            config_warn(&format!("{key}: not a boolean ({other:?}); ignoring"));
            None
        }
    }
}

impl Config {
    /// The default config path (overridable with `--config`).
    pub fn path() -> PathBuf {
        util::xdg_config_home().join("superzej/config.toml")
    }

    /// Load with all layers: defaults < file (`path` or the default) < env < flags.
    pub fn try_load_layered(
        env: &dyn EnvSource,
        cli_overrides: &[String],
        path: Option<PathBuf>,
    ) -> Result<Self, String> {
        let _span = tracing::info_span!("config_load_layered").entered();
        let file = path.unwrap_or_else(Self::path);
        let s = std::fs::read_to_string(&file).unwrap_or_else(|_| "".into());
        let mut cfg: Config = toml::from_str(&s).map_err(|e| format!("{e}"))?;

        // Profile overlay (H): a named profile's own `config.toml` (a full
        // Config-shaped overlay) merges over the shared base, from the REAL
        // config home — `XDG_CONFIG_HOME` is deliberately NOT rerooted, so the
        // shared base still loads while the profile refines it. Below env/`--set`.
        if let Some(pfile) = Self::profile_overlay_path(env)
            && let Ok(ps) = std::fs::read_to_string(&pfile)
            && let Err(e) = Self::apply_toml_overlay(&mut cfg, &ps)
        {
            config_warn(&format!("profile config {}: {e}", pfile.display()));
        }

        env_overlay(env).apply(&mut cfg);

        // Apply dot-notation overrides
        for ov in cli_overrides {
            if let Some((key, val)) = ov.split_once('=')
                && let Err(e) = Self::apply_override_str(&mut cfg, key, val)
            {
                config_warn(&format!("--set {key}={val} failed: {e}"));
            }
        }

        cfg.post_process();
        Ok(cfg)
    }

    /// Load with all layers: defaults < file (`path` or the default) < env < flags.
    pub fn load_layered(
        env: &dyn EnvSource,
        cli_overrides: &[String],
        path: Option<PathBuf>,
    ) -> Self {
        match Self::try_load_layered(env, cli_overrides, path) {
            Ok(cfg) => cfg,
            Err(e) => {
                config_warn(&format!("parse error: {e}; using defaults"));
                let mut cfg = Config::default();
                env_overlay(env).apply(&mut cfg);
                for ov in cli_overrides {
                    if let Some((key, val)) = ov.split_once('=') {
                        let _ = Self::apply_override_str(&mut cfg, key, val);
                    }
                }
                cfg.post_process();
                cfg
            }
        }
    }

    /// The active named profile's config-overlay file
    /// (`<real XDG_CONFIG_HOME>/superzej/profiles/<name>/config.toml`), or `None`
    /// for the default profile. Uses `xdg_config_home()` directly (the shared
    /// config home is never rerooted).
    fn profile_overlay_path(env: &dyn EnvSource) -> Option<PathBuf> {
        let name = crate::profile::normalize_name(&env.get("SUPERZEJ_PROFILE").unwrap_or_default());
        (name != "default").then(|| {
            util::xdg_config_home()
                .join("superzej")
                .join("profiles")
                .join(name)
                .join("config.toml")
        })
    }

    /// Deep-merge a TOML overlay string over `cfg` (overlay wins per-key; base
    /// keys the overlay omits are preserved). The mechanism behind the profile
    /// (and later subprofile) full overlays. Pure + unit-tested.
    pub fn apply_toml_overlay(cfg: &mut Config, toml_str: &str) -> Result<(), String> {
        let overlay: serde_json::Value = toml::from_str(toml_str).map_err(|e| format!("{e}"))?;
        let mut base = serde_json::to_value(&*cfg).map_err(|e| e.to_string())?;
        deep_merge_json(&mut base, overlay);
        *cfg = serde_json::from_value(base).map_err(|e| format!("{e}"))?;
        Ok(())
    }

    fn apply_override_str(cfg: &mut Config, key: &str, val: &str) -> Result<(), String> {
        if key == "apps.tab_order" {
            cfg.apps.tab_order = val
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            return Ok(());
        }

        let mut tree = serde_json::to_value(&cfg).map_err(|e| e.to_string())?;

        let parts: Vec<&str> = key.split('.').collect();
        let mut current = &mut tree;
        for (i, part) in parts.iter().enumerate() {
            if i == parts.len() - 1 {
                if !current.is_object() {
                    return Err(format!("Invalid path: {}", key));
                }
                if let Ok(b) = val.parse::<bool>() {
                    current[*part] = serde_json::Value::Bool(b);
                } else if let Ok(n) = val.parse::<u64>() {
                    current[*part] = serde_json::Value::Number(n.into());
                } else {
                    current[*part] = serde_json::Value::String(val.to_string());
                }
            } else {
                if !current.is_object() {
                    return Err(format!("Invalid path: {}", key));
                }
                let next = current.get_mut(*part);
                match next {
                    Some(val) => current = val,
                    None => return Err(format!("Invalid path: {}", key)),
                }
            }
        }

        let new_cfg: Config =
            serde_json::from_value(tree).map_err(|e| format!("Type error on {}: {}", key, e))?;
        *cfg = new_cfg;
        Ok(())
    }

    fn post_process(&mut self) {
        if self.agents.is_empty() {
            self.agents = vec![
                NamedCommand {
                    name: "claude".into(),
                    command: "claude".into(),
                    hints: vec![],
                    provider: None,
                },
                NamedCommand {
                    name: "termite".into(),
                    command: "termite tui".into(),
                    hints: vec![],
                    provider: None,
                },
                NamedCommand {
                    name: "shell".into(),
                    command: "__shell__".into(),
                    hints: vec![],
                    provider: None,
                },
            ];
        }
        if self.tools.is_empty() {
            self.tools = vec![
                NamedCommand {
                    name: "lazygit".into(),
                    command: "lazygit".into(),
                    hints: vec![],
                    provider: None,
                },
                NamedCommand {
                    name: "yazi".into(),
                    command: "yazi".into(),
                    hints: vec![],
                    provider: None,
                },
                NamedCommand {
                    name: "editor".into(),
                    command: "${EDITOR:-vi} .".into(),
                    hints: vec![],
                    provider: None,
                },
                NamedCommand {
                    name: "diff".into(),
                    command: "git diff".into(),
                    hints: vec![],
                    provider: None,
                },
            ];
        }

        for p in &mut self.pins {
            if let Some(cwd) = &p.cwd {
                p.cwd = Some(util::expand_tilde(cwd));
            }
        }
        self.worktrees_dir = util::expand_tilde(&self.worktrees_dir);
        self.workspaces_dir = util::expand_tilde(&self.workspaces_dir);
        if self.repo_roots.is_empty() {
            self.repo_roots = vec![self.workspaces_dir.clone()];
        }
        self.repo_roots = self
            .repo_roots
            .iter()
            .map(|r| util::expand_tilde(r))
            .collect();
        self.metrics.interval_secs = self.metrics.interval_secs.max(1.0);
        self.metrics.timeout_ms = self.metrics.timeout_ms.clamp(100, 30_000);
        self.metrics.max_body_bytes = self.metrics.max_body_bytes.max(1);
    }

    pub fn agent_command(&self, name: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.command.as_str())
    }

    pub fn tool_command(&self, name: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.command.as_str())
    }

    pub fn test_tasks(&self) -> Vec<&Task> {
        self.tasks
            .iter()
            .filter(|t| t.kind == TaskKind::Test)
            .collect()
    }

    /// The pin with the given name.
    pub fn pin(&self, name: &str) -> Option<&Pin> {
        self.pins.iter().find(|p| p.name == name)
    }

    /// The pin at 1-based position `idx` (the `Alt-1..9` mapping).
    pub fn pin_by_index(&self, idx: usize) -> Option<&Pin> {
        idx.checked_sub(1).and_then(|i| self.pins.get(i))
    }

    /// Pins visible in a workspace: global pins + workspace-scoped pins for that workspace.
    /// When `workspace` is `None`, returns only global pins.
    pub fn pins_for_workspace(&self, workspace: Option<&str>) -> Vec<&Pin> {
        self.pins
            .iter()
            .filter(|p| match p.scope {
                PinScope::Global => true,
                PinScope::Workspace => p.workspace.as_deref() == workspace.or(Some("*")),
            })
            .collect()
    }

    /// The resource-monitor command for a stat segment: `cpu`/`mem` → the
    /// system monitor, `gpu` → the GPU monitor. Unknown kinds → `None`.
    pub fn monitor_command(&self, kind: &str) -> Option<&str> {
        match kind {
            "cpu" | "mem" => Some(self.monitor.system.as_str()),
            "gpu" => Some(self.monitor.gpu.as_str()),
            "loc" => None,
            _ => None,
        }
    }

    /// The active profile config, if `profile` names one in `[profiles]`.
    pub fn active_profile(&self) -> Option<&ProfileConfig> {
        if self.profile.is_empty() {
            None
        } else {
            self.profiles.get(&self.profile)
        }
    }

    /// The ordered keybind layer stack for a focused context, lowest precedence
    /// first: profile → global `[keybinds]` → central `[workspace.<slug>]` →
    /// repo-root `.superzej.*` overlay. The host applies each layer in turn so a
    /// more-specific binding wins. `repo_root`/`slug` are `None` outside a
    /// workspace (e.g. the home tab).
    pub fn effective_keybinds(
        &self,
        repo_root: Option<&std::path::Path>,
        slug: Option<&str>,
    ) -> Vec<KeybindConfig> {
        let mut layers = Vec::new();
        if let Some(p) = self.active_profile() {
            layers.push(p.keybinds.clone());
        }
        layers.push(self.keybinds.clone());
        if let Some(slug) = slug
            && let Some(ws) = self.workspace.get(slug)
        {
            layers.push(ws.keybinds.clone());
        }
        if let Some(root) = repo_root
            && let Some(overlay) = load_repo_overlay(root)
            && !overlay.keybinds.is_empty()
        {
            layers.push(overlay.keybinds);
        }
        layers
    }

    /// The effective notification config: the global `[notifications]` with the
    /// active profile's `[profiles.<p>.notifications]` overlay applied on top,
    /// then a repo-root `.superzej.*` overlay when a worktree is in scope. Follows
    /// the same precedence as [`Self::effective_keybinds`] / [`Self::repo_sandbox`].
    /// `repo_root` is `None` outside a workspace (e.g. the home tab).
    pub fn effective_notifications(
        &self,
        repo_root: Option<&std::path::Path>,
    ) -> NotificationsConfig {
        let mut n = self.notifications.clone();
        if let Some(p) = self.active_profile() {
            p.notifications.clone().apply(&mut n);
        }
        if let Some(root) = repo_root
            && let Some(overlay) = load_repo_overlay(root)
            && !overlay.notifications.is_empty()
        {
            overlay.notifications.apply(&mut n);
        }
        n
    }

    /// The effective sandbox config for a worktree's repo: the global `[sandbox]`
    /// with a repo-root `.superzej.{toml,yaml,yml,json}` overlay applied on top.
    /// Tilde-expands path-bearing fields (mounts, remote_dir).
    pub fn repo_sandbox(&self, repo_root: &std::path::Path) -> SandboxConfig {
        let mut sb = self.sandbox.clone();
        // Profile sandbox overlay (network policy isolation per profile).
        if let Some(profile) = self.active_profile() {
            profile.sandbox.clone().apply(&mut sb);
        }
        if let Some(overlay) = load_repo_overlay(repo_root) {
            overlay.sandbox.apply(&mut sb);
        }
        // Per-workspace bind dirs ([workspace.<slug>] sandbox_mounts) extend the
        // global/profile/overlay mounts. Keyed by the repo's base slug — the
        // same slugify(repo_name) used for [workspace.<slug>] keybinds/accounts
        // (sans the DB collision suffix; we avoid a DB write on the launch path).
        if !self.workspace.is_empty() {
            let base = util::slugify(&crate::repo::repo_name(repo_root));
            let slug = if base.is_empty() {
                "repo".to_string()
            } else {
                base
            };
            if let Some(ws) = self.workspace.get(&slug) {
                sb.mounts.extend(ws.sandbox_mounts.iter().cloned());
            }
        }
        sb.mounts = sb
            .mounts
            .iter()
            .map(|m| match m.split_once(':') {
                Some((host, opt)) => format!("{}:{opt}", util::expand_tilde(host)),
                None => util::expand_tilde(m),
            })
            .collect();
        // NB: remote.remote_dir is a *remote* path — its `~` is expanded on the
        // remote host (see new_worktree::create_remote), not against the local HOME.
        sb
    }

    /// The name of the env a repo's `.superzej.*` overlay selects (`env = "…"`),
    /// or empty. The repo-level layer of env selection.
    pub fn repo_env_name(&self, repo_root: &Path) -> String {
        load_repo_overlay(repo_root)
            .map(|r| r.env.trim().to_string())
            .unwrap_or_default()
    }

    /// Resolve the full execution [`Environment`] for a worktree.
    ///
    /// Env-name precedence (most specific wins): `selected` (the DB worktree/
    /// workspace `env_name`, or a launch flag) → the repo `.superzej.*` `env =`
    /// → the global `[sandbox] default_env` → the implicit `"default"`.
    ///
    /// The `"default"` env (and any unknown name) reproduces today's behavior
    /// exactly: the base `[sandbox]` (+ profile + repo overlay + workspace
    /// mounts) with a placement derived from `[sandbox.remote]` + the `GitLoc`.
    /// A named env overlays its `[env.<name>] sandbox` onto that base and builds
    /// its placement from `[env.<name>.{ssh,k8s,provider}]`.
    pub fn resolve_env(
        &self,
        repo_root: &Path,
        loc: &GitLoc,
        worktree: &Path,
        selected: Option<&str>,
    ) -> Environment {
        let base = self.repo_sandbox(repo_root);
        let pick = |s: &str| {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        };
        let name = selected
            .and_then(pick)
            .or_else(|| pick(&self.repo_env_name(repo_root)))
            .or_else(|| pick(&base.default_env))
            .unwrap_or_else(|| "default".to_string());

        let Some(envc) = self.env.get(&name) else {
            // Implicit default env, or a typo'd selection: today's behavior.
            if name != "default" {
                config_warn(&format!(
                    "execution environment {name:?} is not defined under [env.{name}]; using the default"
                ));
            }
            let data = data_mode_from_remote(base.remote.mode);
            return Environment {
                name: "default".into(),
                placement: crate::sandbox::placement_from_loc(&base, loc),
                sandbox: base,
                data,
            };
        };

        // Named env: overlay its isolation onto the base, build its placement.
        let mut sb = base;
        envc.sandbox.clone().apply(&mut sb);
        let placement = crate::envbuild::build_env_placement(envc, &sb, loc, worktree, repo_root);
        Environment {
            name,
            placement,
            sandbox: sb,
            data: envc.data,
        }
    }

    /// Effective failover policy for the environment named `env_name`: the env's
    /// own `[env.<name>] failover` override if set, else the (repo-overlaid)
    /// global `[sandbox] failover`. `true` ⇒ a bring-up failure may fall back
    /// down the chain to the host; `false` (the default) ⇒ halt + warn. The
    /// implicit/unknown "default" env has no override, so it inherits the global.
    pub fn env_failover(&self, repo_root: &Path, env_name: &str) -> bool {
        if let Some(envc) = self.env.get(env_name)
            && let Some(f) = envc.failover
        {
            return f;
        }
        self.repo_sandbox(repo_root).failover
    }

    /// The accent as a truecolor "R;G;B" fragment; invalid hex falls back to
    /// the default teal.
    pub fn accent_rgb(&self) -> String {
        parse_hex_rgb(&self.theme.accent).unwrap_or_else(|| crate::theme::HUE_TEAL.to_string())
    }

    /// The accent as "#rrggbb" (validated; falls back to the default teal).
    pub fn accent_hex(&self) -> String {
        match parse_hex_rgb(&self.theme.accent) {
            Some(_) => self.theme.accent.to_ascii_lowercase(),
            None => "#6ee7d8".into(),
        }
    }

    /// Resolve the full chrome palette: built-in defaults overlaid with any
    /// `[theme]` / `[theme.colors]` overrides. Invalid hex keeps the default.
    pub fn palette(&self) -> crate::theme::Palette {
        self.palette_with_preset(&self.theme.preset)
    }

    /// The palette for a named preset with this config's `[theme.colors]` /
    /// `[theme.hues]` + accent/focus overrides applied — the live theme-cycle
    /// uses this. Extension tokens a legacy preset leaves empty are derived
    /// last, so derivations follow any user-overridden base colors.
    pub fn palette_with_preset(&self, preset: &str) -> crate::theme::Palette {
        let mut p = crate::theme::preset(preset).unwrap_or_default();
        let set = |slot: &mut String, hex: &Option<String>| {
            if let Some(rgb) = hex.as_deref().and_then(parse_hex_rgb) {
                *slot = rgb;
            }
        };
        let c = &self.theme.colors;
        set(&mut p.bg0, &c.bg0);
        set(&mut p.bg1, &c.bg1);
        set(&mut p.panel, &c.panel);
        set(&mut p.panel2, &c.panel2);
        set(&mut p.raise, &c.raise);
        set(&mut p.border, &c.border);
        set(&mut p.text, &c.text);
        set(&mut p.dim, &c.dim);
        set(&mut p.faint, &c.faint);
        set(&mut p.ghost, &c.ghost);
        set(&mut p.ghost2, &c.ghost2);
        set(&mut p.ghost3, &c.ghost3);
        set(&mut p.shadow_bg, &c.shadow_bg);
        set(&mut p.shadow_fg, &c.shadow_fg);
        set(&mut p.chip_fg, &c.chip_fg);
        set(&mut p.activity_active, &c.activity_active);
        set(&mut p.activity_waiting, &c.activity_waiting);
        let h = &self.theme.hues;
        set(&mut p.hues.teal, &h.teal);
        set(&mut p.hues.magenta, &h.magenta);
        set(&mut p.hues.purple, &h.purple);
        set(&mut p.hues.green, &h.green);
        set(&mut p.hues.amber, &h.amber);
        set(&mut p.hues.red, &h.red);
        set(&mut p.hues.blue, &h.blue);
        set(&mut p.hues.orange, &h.orange);
        // Only override the preset's focus/accent when the user actually
        // customized them (a default — current or pre-prism — would clobber
        // presets).
        if !DEFAULTISH_FOCUS.contains(&self.theme.focus_border.as_str()) {
            set(&mut p.focus, &Some(self.theme.focus_border.clone()));
        }
        if !DEFAULTISH_ACCENTS.contains(&self.theme.accent.as_str()) {
            p.accent = self.accent_rgb();
        }
        crate::theme::extend_palette(&mut p);
        p
    }

    /// Look up a dotted config key as a bare string (for `config get` and the
    /// plugin feed). `None` for an unknown key.
    pub fn get_dotted(&self, key: &str) -> Option<String> {
        Some(match key {
            "worktrees_dir" => self.worktrees_dir.clone(),
            "workspaces_dir" => self.workspaces_dir.clone(),
            "base_branch" => self.base_branch.clone(),
            "branch_prefix" => self.branch_prefix.clone(),
            "picker" => self.picker.to_string(),
            "worktree_mode" => self.worktree_mode.to_string(),
            "name_scheme" => self.name_scheme.to_string(),
            "auto_remove_worktree" => self.auto_remove_worktree.to_string(),
            "confirm_delete" => self.confirm_delete.to_string(),
            "repo_scan_depth" => self.repo_scan_depth.to_string(),
            "repo_roots" => self.repo_roots.join("\n"),
            "theme.preset" => self.theme.preset.clone(),
            "theme.accent" => self.theme.accent.clone(),
            "theme.focus_border" => self.theme.focus_border.clone(),
            "theme.pane_padding" => self.theme.pane_padding.to_string(),
            "theme.undercurl" => self.theme.undercurl.to_string(),
            _ if key.starts_with("theme.colors.") => {
                let c = &self.theme.colors;
                let slot = match &key["theme.colors.".len()..] {
                    "bg0" => &c.bg0,
                    "bg1" => &c.bg1,
                    "panel" => &c.panel,
                    "panel2" => &c.panel2,
                    "raise" => &c.raise,
                    "border" => &c.border,
                    "text" => &c.text,
                    "dim" => &c.dim,
                    "faint" => &c.faint,
                    "ghost" => &c.ghost,
                    "ghost2" => &c.ghost2,
                    "ghost3" => &c.ghost3,
                    "shadow_bg" => &c.shadow_bg,
                    "shadow_fg" => &c.shadow_fg,
                    "chip_fg" => &c.chip_fg,
                    _ => return None,
                };
                slot.clone().unwrap_or_default()
            }
            _ if key.starts_with("theme.hues.") => {
                let h = &self.theme.hues;
                let slot = match &key["theme.hues.".len()..] {
                    "teal" => &h.teal,
                    "magenta" => &h.magenta,
                    "purple" => &h.purple,
                    "green" => &h.green,
                    "amber" => &h.amber,
                    "red" => &h.red,
                    "blue" => &h.blue,
                    "orange" => &h.orange,
                    _ => return None,
                };
                slot.clone().unwrap_or_default()
            }
            "pr.ttl_secs" => self.pr.ttl_secs.to_string(),
            "watch.pr_interval_secs" => self.watch.pr_interval_secs.to_string(),
            "metrics.interval_secs" => self.metrics.interval_secs.to_string(),
            "metrics.timeout_ms" => self.metrics.timeout_ms.to_string(),
            "metrics.max_body_bytes" => self.metrics.max_body_bytes.to_string(),
            "log.level" => self.log.level.to_string(),
            "log.file" => self.log.file.to_string(),
            "log.dir" => self.log.dir_path().to_string_lossy().into_owned(),
            "log.rotation_size_mb" => self.log.rotation_size_mb.to_string(),
            "log.max_files" => self.log.max_files.to_string(),
            "log.format" => self.log.format.to_string(),
            "sandbox.enabled" => self.sandbox.enabled.to_string(),
            "sandbox.backend" => self.sandbox.backend.to_string(),
            "sandbox.image" => self.sandbox.image.clone(),
            "sandbox.network" => self.sandbox.network.to_string(),
            "sandbox.on_missing" => self.sandbox.on_missing.to_string(),
            "sandbox.remote.host" => self.sandbox.remote.host.clone(),
            "sandbox.remote.transport" => self.sandbox.remote.transport.to_string(),
            "sandbox.remote.mode" => self.sandbox.remote.mode.to_string(),
            _ => return None,
        })
    }
}

/// Strictly validate a raw `config.toml` body, collecting human-readable errors
/// for `config validate` (the only place a bad value is treated as an error
/// rather than warned-and-defaulted). Returns the list of problems (empty = ok).
pub fn validate_str(body: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let val: toml::Value = match body.parse() {
        Ok(v) => v,
        Err(e) => return vec![format!("TOML syntax error: {e}")],
    };
    fn check(
        errs: &mut Vec<String>,
        path: &str,
        opt: Option<&toml::Value>,
        f: fn(&str) -> Result<(), String>,
    ) {
        if let Some(toml::Value::String(s)) = opt
            && let Err(e) = f(s)
        {
            errs.push(format!("{path}: {e}"));
        }
    }
    let Some(t) = val.as_table() else {
        return errs;
    };
    check(&mut errs, "picker", t.get("picker"), |s| {
        Picker::from_str_validated(s).map(|_| ())
    });
    check(&mut errs, "worktree_mode", t.get("worktree_mode"), |s| {
        WorktreeMode::from_str_validated(s).map(|_| ())
    });
    check(&mut errs, "name_scheme", t.get("name_scheme"), |s| {
        NameScheme::from_str_validated(s).map(|_| ())
    });
    if let Some(sb) = t.get("sandbox").and_then(|v| v.as_table()) {
        check(&mut errs, "sandbox.backend", sb.get("backend"), |s| {
            SandboxBackend::from_str_validated(s).map(|_| ())
        });
        check(&mut errs, "sandbox.network", sb.get("network"), |s| {
            Network::from_str_validated(s).map(|_| ())
        });
        check(&mut errs, "sandbox.profile", sb.get("profile"), |s| {
            SandboxProfile::from_str_validated(s).map(|_| ())
        });
        check(
            &mut errs,
            "sandbox.agent_profile",
            sb.get("agent_profile"),
            |s| SandboxProfile::from_str_validated(s).map(|_| ()),
        );
        check(&mut errs, "sandbox.on_missing", sb.get("on_missing"), |s| {
            OnMissing::from_str_validated(s).map(|_| ())
        });
        if let Some(rm) = sb.get("remote").and_then(|v| v.as_table()) {
            check(
                &mut errs,
                "sandbox.remote.transport",
                rm.get("transport"),
                |s| RemoteTransport::from_str_validated(s).map(|_| ()),
            );
            check(&mut errs, "sandbox.remote.mode", rm.get("mode"), |s| {
                RemoteMode::from_str_validated(s).map(|_| ())
            });
        }
    }
    if let Some(lg) = t.get("log").and_then(|v| v.as_table()) {
        check(&mut errs, "log.level", lg.get("level"), |s| {
            LogLevel::from_str_validated(s).map(|_| ())
        });
        check(&mut errs, "log.format", lg.get("format"), |s| {
            LogFormat::from_str_validated(s).map(|_| ())
        });
    }
    if let Some(pins) = t.get("pins").and_then(|v| v.as_array()) {
        for (i, pin) in pins.iter().enumerate() {
            if let Some(pin) = pin.as_table() {
                check(
                    &mut errs,
                    &format!("pins[{i}].location"),
                    pin.get("location"),
                    |s| PinLocation::from_str_validated(s).map(|_| ()),
                );
            }
        }
    }
    errs
}

/// Load and parse a repo-root `.superzej.*` overlay, if present. Tries TOML,
/// YAML, then JSON (first existing file wins); parse errors warn and are ignored
/// so a malformed repo file never blocks opening a worktree.
fn load_repo_overlay(repo_root: &std::path::Path) -> Option<RepoConfigFile> {
    for (ext, kind) in [
        ("toml", "toml"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("json", "json"),
    ] {
        let path = repo_root.join(format!(".superzej.{ext}"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed: Result<RepoConfigFile, String> = match kind {
            "toml" => toml::from_str(&text).map_err(|e| e.to_string()),
            "yaml" => serde_yaml::from_str(&text).map_err(|e| e.to_string()),
            _ => serde_json::from_str(&text).map_err(|e| e.to_string()),
        };
        return match parsed {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                config_warn(&format!("{}: parse error: {e}; ignoring", path.display()));
                None
            }
        };
    }
    None
}

/// A repo-root `.superzej.*` overlay that EXISTS but failed to parse. Returned by
/// [`repo_overlay_parse_error`] so the host can refuse to silently ignore it: a
/// dropped overlay can change placement (e.g. a malformed file that was selecting
/// `env = "sprites"` → falls back to local/host), which is exactly the silent
/// degradation a failover-off env forbids.
#[derive(Debug, Clone)]
pub struct RepoOverlayParseError {
    pub path: PathBuf,
    pub error: String,
    /// Best-effort lenient read of the `env = "…"` selector (empty if absent), so
    /// a parse failure elsewhere in the file doesn't hide which env it requested.
    pub selected_env: String,
}

/// If a repo-root `.superzej.*` file exists but fails to parse, return the error
/// (+ a lenient `env =` read). `None` when there's no file or it parses cleanly.
/// Mirrors `load_repo_overlay`'s file precedence; the difference is intent —
/// `load_repo_overlay` swallows the error to keep opening the worktree, this lets
/// the caller surface it (a visible halt/warning) so a dropped overlay that
/// changes placement is never silent.
pub fn repo_overlay_parse_error(repo_root: &Path) -> Option<RepoOverlayParseError> {
    for (ext, kind) in [
        ("toml", "toml"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("json", "json"),
    ] {
        let path = repo_root.join(format!(".superzej.{ext}"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let err = match kind {
            "toml" => toml::from_str::<RepoConfigFile>(&text)
                .err()
                .map(|e| e.to_string()),
            "yaml" => serde_yaml::from_str::<RepoConfigFile>(&text)
                .err()
                .map(|e| e.to_string()),
            _ => serde_json::from_str::<RepoConfigFile>(&text)
                .err()
                .map(|e| e.to_string()),
        };
        return err.map(|error| RepoOverlayParseError {
            path,
            error,
            selected_env: lenient_env_selector(&text),
        });
    }
    None
}

/// Best-effort extraction of a top-level `env = "VALUE"` (TOML/JSON-ish) or
/// `env: VALUE` (YAML) selector from a repo overlay's raw text, so a parse failure
/// elsewhere doesn't hide which env it was selecting. Empty when absent.
fn lenient_env_selector(text: &str) -> String {
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("env") else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix('=').or_else(|| rest.strip_prefix(':')) else {
            continue; // `environment = …`, `env_name = …`, `[env.x]` — not the selector
        };
        let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
        if !v.is_empty() && !v.starts_with('{') && !v.starts_with('[') {
            return v.to_string();
        }
    }
    String::new()
}

/// Map the legacy `[sandbox.remote] mode` onto the env [`DataMode`], so the
/// default env honours an existing `mode = "sshfs"`/`"local_exec"` config.
fn data_mode_from_remote(mode: RemoteMode) -> DataMode {
    match mode {
        RemoteMode::Remote => DataMode::InEnv,
        RemoteMode::LocalExec => DataMode::LocalExec,
        RemoteMode::Sshfs => DataMode::Sshfs,
    }
}

/// "#rrggbb" / "#rgb" -> "R;G;B".
fn parse_hex_rgb(hex: &str) -> Option<String> {
    let h = hex.trim().strip_prefix('#')?;
    let h = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => h.to_string(),
        _ => return None,
    };
    let n = u32::from_str_radix(&h, 16).ok()?;
    Some(format!(
        "{};{};{}",
        (n >> 16) & 255,
        (n >> 8) & 255,
        n & 255
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_defaults_are_budget_safe() {
        let l = LifecycleConfig::default();
        assert!(
            l.enabled,
            "policy on by default (defaults only reduce cost)"
        );
        assert_eq!(l.max_warm, 2);
        assert_eq!(l.idle_ttl_secs, 300);
        assert_eq!(l.eager, EagerScope::ActiveWorktreePlusNew);
        assert!(l.keep_active_warm && l.keep_busy_warm && l.serve_cached_glyphs);
        assert_eq!(l.cost_ceiling_per_hour, 0.0, "no ceiling by default");
        assert_eq!(l.pool.size, 0, "pool disabled by default");
    }

    #[test]
    fn nix_parallel_clamps_and_gates_on_zero() {
        let mut pc = EnvProviderConfig::default();
        assert_eq!(pc.nix_parallel(), None, "0 ⇒ leave nix defaults");
        assert!(pc.is_default(), "speedup fields default to inert");
        pc.nix_parallel_downloads = 100;
        assert_eq!(pc.nix_parallel(), Some(100));
        pc.nix_parallel_downloads = 9999;
        assert_eq!(pc.nix_parallel(), Some(256), "clamped to 256");
        assert!(!pc.is_default());
    }

    #[test]
    fn eager_scope_and_nix_installer_parse() {
        assert_eq!(
            EagerScope::from_str_validated("focus"),
            Ok(EagerScope::ActiveWorktreePlusNew)
        );
        assert_eq!(
            EagerScope::from_str_validated("workspace"),
            Ok(EagerScope::ActiveWorkspace)
        );
        assert_eq!(EagerScope::from_str_validated("off"), Ok(EagerScope::Off));
        assert!(EagerScope::from_str_validated("bogus").is_err());
        assert_eq!(
            NixInstaller::from_str_validated("ds"),
            Ok(NixInstaller::Determinate)
        );
        assert_eq!(NixInstaller::default(), NixInstaller::Official);
    }

    #[test]
    fn short_hash_is_stable_and_distinct() {
        // Stable across calls (the property the sandbox name lifecycle relies on).
        assert_eq!(util::short_hash("/a/b/c", 6), util::short_hash("/a/b/c", 6));
        assert_eq!(util::short_hash("/a/b/c", 6).len(), 6);
        assert_ne!(util::short_hash("/a/b/c", 6), util::short_hash("/a/b/d", 6));
        // base36 charset only.
        assert!(
            util::short_hash("/x/y/z", 6)
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn provider_exec_mode_parses_and_defaults_to_auto() {
        assert_eq!(ProviderExecMode::default(), ProviderExecMode::Auto);
        assert_eq!(
            ProviderExecMode::from_str_validated("api"),
            Ok(ProviderExecMode::Api)
        );
        assert_eq!(
            ProviderExecMode::from_str_validated("CLI"),
            Ok(ProviderExecMode::Cli)
        );
        assert_eq!(ProviderExecMode::Auto.as_str(), "auto");
        assert!(ProviderExecMode::from_str_validated("nope").is_err());
        // A fresh provider block is "default" (so it round-trips as absent) and
        // its exec mode is Auto.
        let pc = EnvProviderConfig::default();
        assert!(pc.is_default());
        assert_eq!(pc.exec, ProviderExecMode::Auto);
    }

    #[test]
    fn app_tab_config_defaults_to_work_first_and_default() {
        let cfg = Config::default();
        assert_eq!(cfg.apps.default_tab, "work");
        assert_eq!(cfg.apps.effective_tab_order(), vec!["work"]);
    }

    #[test]
    fn app_tab_config_honors_file_env_and_cli_order() {
        let mut env = MapEnv::default();
        // Only `work` is a built-in id today; every other requested id is
        // filtered out.
        env.0.insert(
            "SUPERZEJ_APPS_TAB_ORDER".into(),
            "comms,work,dashboard".into(),
        );
        env.0
            .insert("SUPERZEJ_APPS_DEFAULT_TAB".into(), "work".into());
        let flags = vec!["apps.default_tab=work".to_string()];
        let cfg = Config::load_layered(&env, &flags, None);

        assert_eq!(cfg.apps.default_tab, "work");
        assert_eq!(cfg.apps.effective_tab_order(), vec!["work"]);
    }

    #[test]
    fn disk_config_defaults_and_env_override() {
        let cfg = Config::default();
        assert!(cfg.disk.show_sizes);
        assert_eq!(cfg.disk.warn_threshold_gb, 100);
        assert_eq!(cfg.disk.scan_interval_secs, 45);
        assert!(cfg.disk.auto_clean_on_merge);
        assert!(!cfg.disk.clean_on_pr_closed);
        assert!(!cfg.disk.sccache);
        assert!(cfg.disk.sccache_dir.is_empty());
        assert!(cfg.disk.shared_target_dir.is_empty());

        let mut env = MapEnv::default();
        env.0.insert("SUPERZEJ_DISK_SCCACHE".into(), "true".into());
        env.0
            .insert("SUPERZEJ_DISK_WARN_THRESHOLD_GB".into(), "250".into());
        env.0.insert(
            "SUPERZEJ_DISK_SHARED_TARGET_DIR".into(),
            "/tmp/shared".into(),
        );
        let cfg = Config::load_layered(&env, &[], None);
        assert!(cfg.disk.sccache);
        assert_eq!(cfg.disk.warn_threshold_gb, 250);
        assert_eq!(cfg.disk.shared_target_dir, "/tmp/shared");
    }

    #[test]
    fn try_load_layered_handles_overrides_and_invalid_overrides() {
        let env = MapEnv::default();
        let cli_overrides = vec![
            "theme.accent=#abcdef".to_string(),
            "invalid.path=123".to_string(),
            "sandbox.enabled=false".to_string(),
            "sandbox.remote.host=user@box".to_string(),
        ];

        let cfg = Config::try_load_layered(&env, &cli_overrides, None).unwrap();
        assert_eq!(cfg.theme.accent, "#abcdef");
        assert!(!cfg.sandbox.enabled);
        assert_eq!(cfg.sandbox.remote.host, "user@box");
    }

    #[test]
    fn override_str_parses_types_correctly_and_handles_bad_paths() {
        let mut cfg = Config::default();
        // Number
        assert!(Config::apply_override_str(&mut cfg, "repo_scan_depth", "99").is_ok());
        assert_eq!(cfg.repo_scan_depth, 99);
        // Bool
        assert!(Config::apply_override_str(&mut cfg, "sandbox.enabled", "false").is_ok());
        assert!(!cfg.sandbox.enabled);
        // String
        assert!(Config::apply_override_str(&mut cfg, "theme.accent", "#123456").is_ok());
        assert_eq!(cfg.theme.accent, "#123456");
        // Deep error: parent is not an object
        assert!(Config::apply_override_str(&mut cfg, "repo_scan_depth.invalid", "value").is_err());
        // Deep error: parent is missing/null
        assert!(Config::apply_override_str(&mut cfg, "does.not.exist", "value").is_err());
        // Type error: setting a number field to a string that doesn't parse to a number
        assert!(Config::apply_override_str(&mut cfg, "repo_scan_depth", "not_a_number").is_err());

        // Edge cases
        assert!(Config::apply_override_str(&mut cfg, "theme", "value").is_err());
        assert!(Config::apply_override_str(&mut cfg, "drawer.height", "\"30%\"").is_ok());

        // Null test
        assert!(Config::apply_override_str(&mut cfg, "sandbox.remote", "value").is_err());
    }

    #[test]
    fn plugin_manifest_config_projection_parses() {
        let cfg: Config = toml::from_str(
            r#"
[[plugins]]
id = "todoist"
name = "Todoist"
version = "1.0.0"
api = "0.1.0"
capabilities = ["surface:statusbar"]

[[plugins.contributions]]
id = "todoist.count"
extension_point = "StatusBarSegment"
label = "Todoist"
surface = "todoist.status"
"#,
        )
        .unwrap();

        assert_eq!(cfg.plugins.len(), 1);
        assert_eq!(cfg.plugins[0].id.as_str(), "todoist");
        assert_eq!(
            cfg.plugins[0].contributions[0].extension_point,
            crate::plugin_api::ExtensionPoint::StatusBarSegment
        );
    }

    #[test]
    fn worktree_templates_parse_with_defaults() {
        let cfg: Config = toml::from_str(
            r#"
[[worktree_templates]]
name = "rust-feature"
base = "main"
branch_prefix = "feat/"
sandbox = "podman"
agent = "claude"
pins = ["logs", "test-watch"]
commands = ["nvim", "", "cargo watch -x test"]

[[worktree_templates]]
name = "minimal"
"#,
        )
        .unwrap();
        assert_eq!(cfg.worktree_templates.len(), 2);
        let t = &cfg.worktree_templates[0];
        assert_eq!(t.name, "rust-feature");
        assert_eq!(t.base.as_deref(), Some("main"));
        assert_eq!(t.branch_prefix.as_deref(), Some("feat/"));
        assert_eq!(t.sandbox.as_deref(), Some("podman"));
        assert_eq!(t.agent.as_deref(), Some("claude"));
        assert_eq!(t.pins, vec!["logs", "test-watch"]);
        assert_eq!(t.commands.len(), 3);
        // A bare template defaults every optional field.
        let m = &cfg.worktree_templates[1];
        assert_eq!(m.name, "minimal");
        assert!(m.base.is_none() && m.agent.is_none() && m.layout.is_none());
        assert!(m.pins.is_empty() && m.commands.is_empty());
        // Default config has no templates.
        assert!(Config::default().worktree_templates.is_empty());
    }

    #[test]
    fn monitor_defaults() {
        let m = MonitorConfig::default();
        assert_eq!(m.system, "btm");
        assert_eq!(m.gpu, "nvtop");
    }

    #[test]
    fn stats_defaults() {
        let s = StatsConfig::default();
        assert_eq!(s.refresh_secs, 2.0);
        // Nerd Font glyphs by default; overridable to plain text. All must be
        // single-width PUA glyphs (U+E000–U+F8FF) so the icon sits flush with
        // its value — plane-15 MDI glyphs (U+F0000+) double-advance and leave a
        // gap. See StatsConfig::default.
        for (name, icon) in [
            ("cpu", &s.cpu_icon),
            ("mem", &s.mem_icon),
            ("net", &s.net_icon),
            ("gpu", &s.gpu_icon),
            ("temp", &s.temp_icon),
            ("swap", &s.swap_icon),
            ("freq", &s.freq_icon),
            ("load", &s.load_icon),
            ("uptime", &s.uptime_icon),
            ("disk", &s.disk_icon),
            ("battery", &s.battery_icon),
            ("battery_charging", &s.battery_charging_icon),
        ] {
            let cp = icon.chars().next().unwrap() as u32;
            assert!(
                (0xE000..=0xF8FF).contains(&cp),
                "{name} icon U+{cp:04X} must be single-width PUA (U+E000–U+F8FF)"
            );
        }
        assert_eq!(s.cpu_icon, "\u{f4bc}");
        assert_eq!(s.mem_icon, "\u{efc5}");
        assert_eq!(s.net_icon, "\u{f1eb}"); // nf-fa-wifi
        assert_eq!(s.gpu_icon, "\u{f2db}"); // nf-fa-microchip
        assert_eq!(s.battery_icon, "\u{f240}"); // nf-fa-battery_full
        // nf-fa-bolt — lightning bolt shown while charging.
        assert_eq!(s.battery_charging_icon, "\u{f0e7}");
        assert_eq!(s.battery_warn, 25);
        assert_eq!(s.refresh_rates, vec![1.0, 2.0, 5.0, 10.0]);
    }

    #[test]
    fn monitor_command_maps_kinds() {
        let cfg = Config::default();
        assert_eq!(cfg.monitor_command("cpu"), Some("btm"));
        assert_eq!(cfg.monitor_command("mem"), Some("btm"));
        assert_eq!(cfg.monitor_command("gpu"), Some("nvtop"));
        assert_eq!(cfg.monitor_command("disk"), None);
        assert_eq!(cfg.monitor_command(""), None);
    }

    #[test]
    fn monitor_command_honors_overrides() {
        let cfg = Config {
            monitor: MonitorConfig {
                system: "htop".into(),
                gpu: "nvitop".into(),
            },
            ..Config::default()
        };
        assert_eq!(cfg.monitor_command("cpu"), Some("htop"));
        assert_eq!(cfg.monitor_command("gpu"), Some("nvitop"));
    }

    #[test]
    fn missing_monitor_table_uses_defaults() {
        // A config.toml without a [monitor] table parses with serde defaults.
        let cfg: Config = toml::from_str("base_branch = \"main\"").unwrap();
        assert_eq!(cfg.monitor.system, "btm");
        assert_eq!(cfg.monitor.gpu, "nvtop");
    }

    #[test]
    fn parses_monitor_table() {
        let cfg: Config =
            toml::from_str("[monitor]\nsystem = \"htop\"\ngpu = \"nvtop\"\n").unwrap();
        assert_eq!(cfg.monitor.system, "htop");
        assert_eq!(cfg.monitor.gpu, "nvtop");
    }

    #[test]
    fn partial_monitor_table_keeps_serde_defaults() {
        // Only one key set — the other falls back to its default.
        let cfg: Config = toml::from_str("[monitor]\ngpu = \"nvitop\"\n").unwrap();
        assert_eq!(cfg.monitor.system, "btm");
        assert_eq!(cfg.monitor.gpu, "nvitop");
    }

    #[test]
    fn parse_hex_rgb_accepts_3_and_6_digit_and_rejects_junk() {
        assert_eq!(parse_hex_rgb("#76eede").as_deref(), Some("118;238;222"));
        assert_eq!(parse_hex_rgb("#fff").as_deref(), Some("255;255;255"));
        assert_eq!(parse_hex_rgb("#000").as_deref(), Some("0;0;0"));
        assert_eq!(parse_hex_rgb("76eede"), None); // requires a leading '#'
        assert_eq!(parse_hex_rgb("#12g456"), None);
        assert_eq!(parse_hex_rgb("#1234"), None);
        assert_eq!(parse_hex_rgb(""), None);
    }

    #[test]
    fn accent_helpers_fall_back_to_teal_on_bad_hex() {
        let good = Config {
            theme: ThemeConfig {
                accent: "#FFffFF".into(),
                ..ThemeConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(good.accent_rgb(), "255;255;255");
        assert_eq!(good.accent_hex(), "#ffffff"); // normalized to lowercase
        let bad = Config {
            theme: ThemeConfig {
                accent: "not-a-color".into(),
                ..ThemeConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(bad.accent_hex(), "#6ee7d8");
        assert_eq!(bad.accent_rgb(), crate::theme::HUE_TEAL);
    }

    #[test]
    fn palette_defaults_match_builtins() {
        let p = Config::default().palette();
        assert_eq!(p, crate::theme::Palette::default());
        assert_eq!(p.focus, crate::theme::HUE_TEAL);
        assert_eq!(p.border, crate::theme::P_GHOST);
        assert_eq!(p.accent, crate::theme::HUE_TEAL);
    }

    #[test]
    fn legacy_presets_come_back_fully_extended() {
        let cfg = Config::default();
        for name in crate::theme::PRESETS {
            let p = cfg.palette_with_preset(name);
            assert!(!p.ghost2.is_empty(), "{name}: ghost2");
            assert!(!p.shadow_bg.is_empty(), "{name}: shadow_bg");
            assert!(!p.hues.orange.is_empty(), "{name}: hues");
            assert!(p.heat.iter().all(|h| !h.is_empty()), "{name}: heat");
        }
    }

    #[test]
    fn activity_dot_colors_resolve_and_honor_overrides() {
        let cfg = Config::default();
        for name in crate::theme::PRESETS {
            let p = cfg.palette_with_preset(name);
            // Default: active borrows the text tone, waiting borrows red.
            assert_eq!(p.activity_active, p.text, "{name}: activity_active");
            assert_eq!(p.activity_waiting, p.hues.red, "{name}: activity_waiting");
        }
        // Explicit `[theme.colors]` overrides win over the derived defaults.
        let mut cfg = Config::default();
        cfg.theme.colors.activity_active = Some("#010203".into());
        cfg.theme.colors.activity_waiting = Some("#0a0b0c".into());
        let p = cfg.palette();
        assert_eq!(p.activity_active, "1;2;3");
        assert_eq!(p.activity_waiting, "10;11;12");
    }

    #[test]
    fn derived_tokens_follow_overridden_bases_and_hue_overrides_apply() {
        let mut cfg = Config::default();
        cfg.theme.preset = "storm".into();
        cfg.theme.colors.ghost = Some("#808080".into());
        cfg.theme.hues.red = Some("#ff0000".into());
        let p = cfg.palette();
        // ghost2 derives from the *overridden* ghost, not storm's.
        assert_eq!(
            p.ghost2,
            crate::theme::blend_over("128;128;128", &p.bg0, 0.62)
        );
        assert_eq!(p.hues.red, "255;0;0");
        // Explicit extension override beats derivation.
        cfg.theme.colors.ghost2 = Some("#010203".into());
        assert_eq!(cfg.palette().ghost2, "1;2;3");
    }

    #[test]
    fn old_default_accent_still_reads_as_uncustomized() {
        // A config that pinned the pre-prism default accent keeps preset
        // accents when cycling (treated as "not customized").
        let mut cfg = Config::default();
        cfg.theme.accent = "#76eede".into();
        cfg.theme.focus_border = "#9bd1ff".into();
        let p = cfg.palette_with_preset("ember");
        assert_eq!(p.accent, "255;122;89"); // ember's own accent survives
        assert_eq!(p.focus, "255;176;102"); // ember's own focus survives
    }

    #[test]
    fn palette_applies_overrides_and_skips_bad_hex() {
        let mut cfg = Config::default();
        cfg.theme.focus_border = "#102030".into();
        cfg.theme.colors.bg0 = Some("#000000".into());
        cfg.theme.colors.border = Some("#fff".into()); // short form
        cfg.theme.colors.text = Some("nope".into()); // invalid -> default
        let p = cfg.palette();
        assert_eq!(p.focus, "16;32;48");
        assert_eq!(p.bg0, "0;0;0");
        assert_eq!(p.border, "255;255;255");
        assert_eq!(p.text, crate::theme::P_TEXT);
    }

    #[test]
    fn theme_keys_via_get_set_and_env() {
        let mut cfg = Config::default();
        assert!(Config::apply_override_str(&mut cfg, "theme.focus_border", "#abcdef").is_ok());
        assert!(Config::apply_override_str(&mut cfg, "theme.colors.bg1", "#111111").is_ok());
        assert_eq!(cfg.get_dotted("theme.focus_border").unwrap(), "#abcdef");
        assert_eq!(cfg.get_dotted("theme.colors.bg1").unwrap(), "#111111");
        assert_eq!(cfg.get_dotted("theme.colors.bg0").unwrap(), "");
        assert_eq!(cfg.get_dotted("theme.colors.bogus"), None);

        let env = map_env(&[
            ("SUPERZEJ_THEME_FOCUS_BORDER", "#010203"),
            ("SUPERZEJ_THEME_BORDER", "#040506"),
        ]);
        let mut base = Config::default();
        env_overlay(&env).apply(&mut base);
        assert_eq!(base.theme.focus_border, "#010203");
        assert_eq!(base.theme.colors.border.as_deref(), Some("#040506"));
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sz-cfg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn map_env(pairs: &[(&str, &str)]) -> MapEnv {
        MapEnv(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn sandbox_profile_defaults_and_env_overlay() {
        // Safe-by-default: the worktree shell is hardened, the embedded agent
        // gets its own sealed container.
        let c = SandboxConfig::default();
        assert_eq!(c.profile, SandboxProfile::Hardened);
        assert_eq!(c.agent_profile, SandboxProfile::Sealed);

        let o = env_overlay(&map_env(&[
            ("SUPERZEJ_SANDBOX_PROFILE", "open"),
            ("SUPERZEJ_SANDBOX_AGENT_PROFILE", "hardened"),
        ]));
        assert_eq!(o.sandbox.profile, Some(SandboxProfile::Open));
        assert_eq!(o.sandbox.agent_profile, Some(SandboxProfile::Hardened));

        // Overlay precedence: a present key overrides the global default.
        let mut base = SandboxConfig::default();
        o.sandbox.apply(&mut base);
        assert_eq!(base.profile, SandboxProfile::Open);
        assert_eq!(base.agent_profile, SandboxProfile::Hardened);
    }

    // The same overlay expressed in each format must produce identical results,
    // and only the present keys override the global defaults.
    #[test]
    fn repo_overlay_all_three_formats_agree() {
        let cfg = Config::default();
        let cases = [
            (
                "toml",
                ".superzej.toml",
                "[sandbox]\nimage = \"img:1\"\ninit_script = \"echo hi\"\n[sandbox.remote]\nhost = \"user@box\"\n",
            ),
            (
                "yaml",
                ".superzej.yaml",
                "sandbox:\n  image: img:1\n  init_script: echo hi\n  remote:\n    host: user@box\n",
            ),
            (
                "json",
                ".superzej.json",
                "{\"sandbox\":{\"image\":\"img:1\",\"init_script\":\"echo hi\",\"remote\":{\"host\":\"user@box\"}}}",
            ),
        ];
        for (tag, file, body) in cases {
            let dir = tmpdir(tag);
            std::fs::write(dir.join(file), body).unwrap();
            let sb = cfg.repo_sandbox(&dir);
            assert_eq!(sb.image, "img:1", "{tag}: image overridden");
            assert_eq!(sb.init_script, "echo hi", "{tag}: init overridden");
            assert_eq!(sb.remote.host, "user@box", "{tag}: remote host overridden");
            // Untouched keys keep their defaults.
            assert!(sb.enabled, "{tag}: enabled keeps default");
            assert_eq!(
                sb.backend,
                SandboxBackend::Auto,
                "{tag}: backend keeps default"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn no_repo_file_yields_global() {
        let cfg = Config::default();
        let dir = tmpdir("none");
        let sb = cfg.repo_sandbox(&dir);
        assert_eq!(sb.image, ""); // global default (host-toolchain)
        assert!(!sb.remote.is_remote());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_sandbox_mounts_extend_global() {
        let dir = tmpdir("ws-binds");
        // Same base slug repo_sandbox derives (slugify(repo_name); no DB).
        let base = util::slugify(&crate::repo::repo_name(&dir));
        let slug = if base.is_empty() {
            "repo".to_string()
        } else {
            base
        };
        let mut cfg = Config::default();
        cfg.sandbox.mounts = vec!["/srv/global".into()];
        cfg.workspace.insert(
            slug,
            WorkspaceConfig {
                sandbox_mounts: vec!["~/datasets:ro".into()],
                ..Default::default()
            },
        );
        let sb = cfg.repo_sandbox(&dir);
        // Global mount survives, workspace mount is appended and tilde-expanded.
        assert!(sb.mounts.iter().any(|m| m == "/srv/global"));
        assert!(
            sb.mounts
                .iter()
                .any(|m| m.ends_with("/datasets:ro") && !m.starts_with('~')),
            "workspace mount appended + tilde-expanded: {:?}",
            sb.mounts
        );
        // A workspace with no entry for this slug adds nothing.
        let mut other = Config::default();
        other.workspace.insert(
            "some-other-repo".into(),
            WorkspaceConfig {
                sandbox_mounts: vec!["/should/not/appear".into()],
                ..Default::default()
            },
        );
        let sb2 = other.repo_sandbox(&dir);
        assert!(!sb2.mounts.iter().any(|m| m == "/should/not/appear"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drawer_defaults() {
        let d = DrawerConfig::default();
        assert_eq!(d.command, "");
        assert_eq!(d.config_home, ""); // empty = private default
        assert_eq!(d.height, "35%");
        assert_eq!(d.width, "full");
        assert!(!d.image_previews);
        assert!(d.git_status);
        assert!(d.contain);
        assert_eq!(d.memory_max, "2G");
        assert_eq!(d.memory_swap_max, "512M");
        assert_eq!(d.cpu_quota, "200%");
        assert_eq!(d.pool_limit, 1);
        assert!(!d.prewarm);
    }

    #[test]
    fn config_without_drawer_section_uses_defaults() {
        let cfg: Config = toml::from_str("base_branch = \"main\"").unwrap();
        assert_eq!(cfg.drawer.height, "35%");
        assert_eq!(cfg.drawer.width, "full");
        assert_eq!(cfg.drawer.command, "");
        assert!(!cfg.drawer.image_previews);
        assert!(cfg.drawer.contain);
        assert_eq!(cfg.drawer.memory_max, "2G");
        assert_eq!(cfg.drawer.memory_swap_max, "512M");
        assert_eq!(cfg.drawer.cpu_quota, "200%");
        assert_eq!(cfg.drawer.pool_limit, 1);
        assert!(!cfg.drawer.prewarm);
    }

    #[test]
    fn drawer_section_overrides_parse() {
        let cfg: Config = toml::from_str(
            "[drawer]\ncommand = \"ranger\"\nconfig_home = \"system\"\nheight = \"50%\"\nwidth = \"center\"\nimage_previews = true\ncontain = false\nmemory_max = \"4G\"\nmemory_swap_max = \"0\"\ncpu_quota = \"50%\"\npool_limit = 0\nprewarm = true\n",
        )
        .unwrap();
        assert_eq!(cfg.drawer.command, "ranger");
        assert_eq!(cfg.drawer.config_home, "system");
        assert_eq!(cfg.drawer.height, "50%");
        assert_eq!(cfg.drawer.width, "center");
        assert!(cfg.drawer.image_previews);
        assert!(!cfg.drawer.contain);
        assert_eq!(cfg.drawer.memory_max, "4G");
        assert_eq!(cfg.drawer.memory_swap_max, "0");
        assert_eq!(cfg.drawer.cpu_quota, "50%");
        assert_eq!(cfg.drawer.pool_limit, 0);
        assert!(cfg.drawer.prewarm);
    }

    #[test]
    fn drawer_partial_section_keeps_other_defaults() {
        // Only height set; the rest fall back to defaults via #[serde(default)].
        let cfg: Config = toml::from_str("[drawer]\nheight = \"20%\"\n").unwrap();
        assert_eq!(cfg.drawer.height, "20%");
        assert_eq!(cfg.drawer.width, "full");
        assert_eq!(cfg.drawer.command, "");
        assert!(!cfg.drawer.image_previews);
        assert!(cfg.drawer.contain);
        assert_eq!(cfg.drawer.pool_limit, 1);
        assert!(!cfg.drawer.prewarm);
    }

    #[test]
    fn git_section_and_custom_commands_parse() {
        let cfg: Config = toml::from_str(
            r#"
[git]
override_gpg = true

[[git_commands]]
key = "p"
context = "branches"
command = "git push {{.SelectedBranch.Name | quote}}"
output = "terminal"
description = "push selected branch"
prompts = [{ type = "input", title = "Remote", key = "Remote" }]

[[git_commands]]
key = "n"
command = "git notes add {{.SelectedCommit.Sha}}"
"#,
        )
        .unwrap();
        assert!(cfg.git.override_gpg);
        assert_eq!(cfg.git_commands.len(), 2);
        let c = &cfg.git_commands[0];
        assert_eq!(c.key, "p");
        assert_eq!(c.context, "branches");
        assert_eq!(c.output, GitCmdOutput::Terminal);
        assert_eq!(c.description.as_deref(), Some("push selected branch"));
        assert_eq!(c.prompts.len(), 1);
        assert_eq!(c.prompts[0].key, "Remote");
        assert_eq!(c.prompts[0].kind, "input");
        assert_eq!(c.prompts[0].title.as_deref(), Some("Remote"));
        // Defaults: context global, popup output, no prompts.
        let c = &cfg.git_commands[1];
        assert_eq!(c.context, "global");
        assert_eq!(c.output, GitCmdOutput::Popup);
        assert!(c.prompts.is_empty());

        // Absent section → defaults.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(!cfg.git.override_gpg);
        assert!(cfg.git_commands.is_empty());
    }

    #[test]
    fn panel_sections_parse_and_default_empty() {
        let cfg: Config =
            toml::from_str("[panel]\nsections = [\"pr\", \"changes\", \"telemetry\"]\n").unwrap();
        assert_eq!(cfg.panel.sections, vec!["pr", "changes", "telemetry"]);
        // Absent table → empty list (the host shows every section).
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.panel.sections.is_empty());
    }

    #[test]
    fn panel_collapse_on_escape_defaults_true_and_parses() {
        // Default (both the Rust `Default` and the absent-table serde path).
        assert!(PanelConfig::default().collapse_on_escape);
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.panel.collapse_on_escape);
        // Explicit opt-out.
        let cfg: Config = toml::from_str("[panel]\ncollapse_on_escape = false\n").unwrap();
        assert!(!cfg.panel.collapse_on_escape);
    }

    #[test]
    fn config_parses_mode_specific_keybinds() {
        let cfg: Config = toml::from_str(
            "[keybinds]\nnew-worktree = \"Alt w\"\n[keybinds.vim_normal]\nfocus-down = \"j\"\n[keybinds.emacs]\nquit = \"Ctrl x Ctrl c\"\n",
        )
        .unwrap();
        assert_eq!(
            cfg.keybinds.get("new-worktree").map(String::as_str),
            Some("Alt w")
        );
        assert_eq!(
            cfg.keybinds
                .vim_normal
                .get("focus-down")
                .map(String::as_str),
            Some("j")
        );
        assert_eq!(
            cfg.keybinds.emacs.get("quit").map(String::as_str),
            Some("Ctrl x Ctrl c")
        );
    }

    #[test]
    fn keybind_config_serializes_nested_mode_tables() {
        let mut cfg = Config::default();
        cfg.keybinds.insert("new-worktree".into(), "Ctrl w".into());
        cfg.keybinds
            .vim_normal
            .insert("focus-down".into(), "j".into());
        let s = toml::to_string_pretty(&cfg).unwrap();
        assert!(s.contains("[keybinds]"));
        assert!(s.contains("new-worktree = \"Ctrl w\""));
        assert!(s.contains("[keybinds.vim_normal]"));
        assert!(s.contains("focus-down = \"j\""));
    }

    #[test]
    fn config_parses_profiles_and_active_profile() {
        let cfg: Config = toml::from_str(
            "profile = \"vim\"\n[profiles.vim]\ndefault_mode = \"vim-normal\"\n[profiles.vim.keybinds]\nfocus-down = \"j\"\n",
        )
        .unwrap();
        let p = cfg.active_profile().expect("active profile resolves");
        assert_eq!(p.default_mode, "vim-normal");
        assert_eq!(p.keybinds.get("focus-down").map(String::as_str), Some("j"));
    }

    #[test]
    fn unknown_profile_has_no_active_profile() {
        let cfg: Config = toml::from_str("profile = \"nope\"\n").unwrap();
        assert!(cfg.active_profile().is_none());
    }

    #[test]
    fn effective_keybinds_layers_profile_then_global() {
        let cfg: Config = toml::from_str(
            "profile = \"vim\"\n[keybinds]\nfocus-down = \"Ctrl j\"\n[profiles.vim.keybinds]\nfocus-down = \"j\"\n",
        )
        .unwrap();
        let layers = cfg.effective_keybinds(None, None);
        // profile layer first (lowest precedence), then global.
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].get("focus-down").map(String::as_str), Some("j"));
        assert_eq!(
            layers[1].get("focus-down").map(String::as_str),
            Some("Ctrl j")
        );
    }

    #[test]
    fn effective_keybinds_adds_central_workspace_layer_for_slug() {
        let cfg: Config = toml::from_str(
            "[keybinds]\nfocus-down = \"Ctrl j\"\n[workspace.myrepo.keybinds]\nfocus-down = \"Alt j\"\n",
        )
        .unwrap();
        let none = cfg.effective_keybinds(None, None);
        assert_eq!(none.len(), 1); // global only
        let with = cfg.effective_keybinds(None, Some("myrepo"));
        assert_eq!(with.len(), 2);
        assert_eq!(with[1].get("focus-down").map(String::as_str), Some("Alt j"));
    }

    // G2: Profile sandbox overlay applied by repo_sandbox().
    #[test]
    fn profile_sandbox_overlay_applies_network_block() {
        let cfg: Config = toml::from_str(
            "profile = \"work\"\n\
             [profiles.work.sandbox]\n\
             network_block = [\"social.example.com\"]\n",
        )
        .unwrap();
        // repo_sandbox on any path should now inherit the profile block-list.
        let sb = cfg.repo_sandbox(std::path::Path::new("/nonexistent"));
        assert!(
            sb.network_block.contains(&"social.example.com".to_string()),
            "profile network_block should flow into repo_sandbox: {:?}",
            sb.network_block
        );
    }

    #[test]
    fn profile_sandbox_overlay_does_not_apply_when_inactive() {
        let cfg: Config = toml::from_str(
            "profile = \"\"\n\
             [profiles.work.sandbox]\n\
             network_block = [\"social.example.com\"]\n",
        )
        .unwrap();
        let sb = cfg.repo_sandbox(std::path::Path::new("/nonexistent"));
        assert!(
            sb.network_block.is_empty(),
            "inactive profile must not inject block list: {:?}",
            sb.network_block
        );
    }

    #[test]
    fn repo_overlay_keybinds_are_the_most_specific_layer() {
        let dir = tmpdir("repo-kb");
        std::fs::write(
            dir.join(".superzej.toml"),
            "[keybinds]\nfocus-down = \"Alt n\"\n",
        )
        .unwrap();
        let cfg: Config = toml::from_str(
            "[keybinds]\nfocus-down = \"Ctrl j\"\n[workspace.myrepo.keybinds]\nfocus-down = \"Alt j\"\n",
        )
        .unwrap();
        let layers = cfg.effective_keybinds(Some(&dir), Some("myrepo"));
        // global, central-workspace, repo-root overlay (last = highest precedence).
        assert_eq!(layers.len(), 3);
        assert_eq!(
            layers.last().unwrap().get("focus-down").map(String::as_str),
            Some("Alt n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn profile_selectable_via_env() {
        let env = MapEnv(BTreeMap::from([(
            "SUPERZEJ_PROFILE".to_string(),
            "emacs".to_string(),
        )]));
        let cfg = Config::load_layered(&env, &[], None);
        assert_eq!(cfg.profile, "emacs");
    }

    // defaults < file < env < flag, for a scalar and a validated enum.
    #[test]
    fn precedence_default_file_env_flag() {
        let dir = tmpdir("prec");
        let file = dir.join("config.toml");
        std::fs::write(&file, "branch_prefix = \"file/\"\npicker = \"gum\"\n").unwrap();

        // file only
        let c = Config::load_layered(&MapEnv::default(), &[], Some(file.clone()));
        assert_eq!(c.branch_prefix, "file/");
        assert_eq!(c.picker, Picker::Gum);

        // env overrides file
        let env = map_env(&[
            ("SUPERZEJ_BRANCH_PREFIX", "env/"),
            ("SUPERZEJ_PICKER", "fzf"),
        ]);
        let c = Config::load_layered(&env, &[], Some(file.clone()));
        assert_eq!(c.branch_prefix, "env/");
        assert_eq!(c.picker, Picker::Fzf);

        let flags = vec![
            "branch_prefix=flag/".to_string(),
            "picker=select".to_string(),
        ];
        let c = Config::load_layered(&env, &flags, Some(file));
        assert_eq!(c.picker, Picker::Select);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_enum_warns_and_defaults() {
        // A junk picker in the file deserializes to the default, never errors.
        let c: Config = toml::from_str("picker = \"nope\"\n").unwrap();
        assert_eq!(c.picker, Picker::Auto);
        // strict validate flags it
        let errs = validate_str("picker = \"nope\"\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("picker"));
    }

    #[test]
    fn pin_location_defaults_to_tab() {
        let cfg: Config = toml::from_str("[[pins]]\nname = 'x'\ncommand = 'echo x'\n").unwrap();
        assert_eq!(cfg.pins[0].location, PinLocation::Tab);
    }

    #[test]
    fn pin_location_parses_layout() {
        let cfg: Config =
            toml::from_str("[[pins]]\nname = 'x'\ncommand = 'echo x'\nlocation = 'layout'\n")
                .unwrap();
        assert_eq!(cfg.pins[0].location, PinLocation::Layout);
        assert_eq!(PinLocation::Layout.as_str(), "layout");
    }

    #[test]
    fn pin_location_bad_value_defaults_but_validate_flags_it() {
        let body = "[[pins]]\nname = 'x'\ncommand = 'echo x'\nlocation = 'bogus'\n";
        let cfg: Config = toml::from_str(body).unwrap();
        assert_eq!(cfg.pins[0].location, PinLocation::Tab);
        let errs = validate_str(body);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("pins[0].location"), "{errs:?}");
    }

    #[test]
    fn pin_location_parses_strip_and_float_with_aliases() {
        let strip: Config =
            toml::from_str("[[pins]]\nname='x'\ncommand='c'\nlocation='top-strip'\n").unwrap();
        assert_eq!(strip.pins[0].location, PinLocation::Strip);
        assert_eq!(PinLocation::Strip.as_str(), "strip");
        let float: Config =
            toml::from_str("[[pins]]\nname='x'\ncommand='c'\nlocation='scratch'\n").unwrap();
        assert_eq!(float.pins[0].location, PinLocation::Float);
        assert_eq!(PinLocation::Float.as_str(), "float");
    }

    #[test]
    fn pin_extended_fields_parse() {
        let body = "[[pins]]\nname='logs'\ncommand='journalctl'\nargs=['-f']\n\
                    label='syslog'\nratio=2.5\n[pins.env]\nRUST_LOG='info'\n";
        let cfg: Config = toml::from_str(body).unwrap();
        let p = &cfg.pins[0];
        assert_eq!(p.args, vec!["-f"]);
        assert_eq!(p.display_label(), "syslog");
        assert_eq!(p.strip_weight(), 2.5);
        assert_eq!(p.env.get("RUST_LOG").map(String::as_str), Some("info"));
    }

    #[test]
    fn pin_helpers_fall_back_sensibly() {
        let cfg: Config = toml::from_str("[[pins]]\nname='bare'\ncommand='c'\n").unwrap();
        let p = &cfg.pins[0];
        // No label → name; no/zero ratio → 1.0.
        assert_eq!(p.display_label(), "bare");
        assert_eq!(p.strip_weight(), 1.0);
        let mut neg = p.clone();
        neg.ratio = Some(-3.0);
        assert_eq!(neg.strip_weight(), 1.0);
    }

    #[test]
    fn strip_config_defaults_and_clamps() {
        let def = StripConfig::default();
        assert_eq!(def.ratio, 0.2);
        assert!(def.visible);
        let lo = StripConfig {
            ratio: 0.001,
            visible: true,
        };
        assert_eq!(lo.clamped_ratio(), 0.05);
        let hi = StripConfig {
            ratio: 5.0,
            visible: false,
        };
        assert_eq!(hi.clamped_ratio(), 0.9);
    }

    #[test]
    fn pins_for_workspace_filters_by_scope() {
        let body = "[[pins]]\nname='g'\ncommand='c'\nscope='global'\n\
                    [[pins]]\nname='w'\ncommand='c'\nscope='workspace'\nworkspace='repoA'\n";
        let cfg: Config = toml::from_str(body).unwrap();
        let a: Vec<_> = cfg
            .pins_for_workspace(Some("repoA"))
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(a, vec!["g", "w"]);
        let b: Vec<_> = cfg
            .pins_for_workspace(Some("repoB"))
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(b, vec!["g"]);
    }

    #[test]
    fn deprecated_sz_pr_ttl_still_read() {
        let env = map_env(&[("SZ_PR_TTL", "7")]);
        let o = env_overlay(&env);
        assert_eq!(o.pr_ttl_secs, Some(7));
        // canonical wins when both set
        let env = map_env(&[("SZ_PR_TTL", "7"), ("SUPERZEJ_PR_TTL", "9")]);
        assert_eq!(env_overlay(&env).pr_ttl_secs, Some(9));
    }

    #[test]
    fn enum_roundtrip() {
        for (s, p) in [
            ("auto", Picker::Auto),
            ("gum", Picker::Gum),
            ("fzf", Picker::Fzf),
            ("select", Picker::Select),
        ] {
            assert_eq!(Picker::from_str_validated(s).unwrap(), p);
            assert_eq!(p.as_str(), s);
        }
        assert!(Picker::from_str_validated("bogus").is_err());
        // aliases
        assert_eq!(
            SandboxBackend::from_str_validated("bubblewrap").unwrap(),
            SandboxBackend::Bwrap
        );
        assert_eq!(
            SandboxBackend::from_str_validated("host").unwrap(),
            SandboxBackend::None
        );
    }

    #[test]
    fn get_dotted_reads_values() {
        let c = Config::default();
        assert_eq!(c.get_dotted("picker").as_deref(), Some("auto"));
        assert_eq!(c.get_dotted("pr.ttl_secs").as_deref(), Some("30"));
        assert_eq!(c.get_dotted("sandbox.backend").as_deref(), Some("auto"));
        assert!(c.get_dotted("nope.nope").is_none());
    }

    #[test]
    fn effective_config_serializes_to_toml() {
        // `config show` round-trips the effective config back to parseable TOML.
        let c = Config::default();
        let s = toml::to_string_pretty(&c).expect("serialize");
        let back: Config = toml::from_str(&s).expect("reparse");
        assert_eq!(back.picker, c.picker);
        assert_eq!(back.sandbox.backend, c.sandbox.backend);
    }

    #[test]
    fn metrics_config_defaults_and_toml_parse() {
        let default = MetricsConfig::default();
        assert_eq!(default.interval_secs, 5.0);
        assert_eq!(default.timeout_ms, 500);
        assert_eq!(default.max_body_bytes, 1_048_576);
        assert!(default.targets.is_empty());

        let cfg: Config = toml::from_str(
            r#"
            [metrics]
            interval_secs = 2.5
            timeout_ms = 250
            max_body_bytes = 4096

            [[metrics.targets]]
            name = "model-proxy"
            url = "http://127.0.0.1:9091/metrics"
            metrics = ["http_requests_total", "process_resident_memory_bytes"]
            labels = { instance = "local" }
            "#,
        )
        .unwrap();
        assert_eq!(cfg.metrics.interval_secs, 2.5);
        assert_eq!(cfg.metrics.timeout_ms, 250);
        assert_eq!(cfg.metrics.max_body_bytes, 4096);
        assert_eq!(cfg.metrics.targets.len(), 1);
        let target = &cfg.metrics.targets[0];
        assert_eq!(target.name, "model-proxy");
        assert_eq!(target.url, "http://127.0.0.1:9091/metrics");
        assert_eq!(target.metrics[0], "http_requests_total");
        assert_eq!(
            target.labels.get("instance").map(String::as_str),
            Some("local")
        );
    }

    #[test]
    fn metrics_env_overlay_clamps_runtime_bounds() {
        let env = map_env(&[
            ("SUPERZEJ_METRICS_INTERVAL_SECS", "0.2"),
            ("SUPERZEJ_METRICS_TIMEOUT_MS", "10"),
            ("SUPERZEJ_METRICS_MAX_BODY_BYTES", "0"),
        ]);
        let c = Config::load_layered(&env, &[], None);
        assert_eq!(c.metrics.interval_secs, 1.0);
        assert_eq!(c.metrics.timeout_ms, 100);
        assert_eq!(c.metrics.max_body_bytes, 1);
    }

    // Exercise every env knob (and the canonical/deprecated/bad-value paths) so
    // the layering is covered, not just spot-checked.
    #[test]
    fn env_overlay_covers_every_knob() {
        let env = map_env(&[
            ("SUPERZEJ_WORKTREES_DIR", "/wt"),
            ("SUPERZEJ_WORKSPACES_DIR", "/ws"),
            ("SUPERZEJ_BASE_BRANCH", "develop"),
            ("SUPERZEJ_BRANCH_PREFIX", "x/"),
            ("SUPERZEJ_PICKER", "fzf"),
            ("SUPERZEJ_WORKTREE_MODE", "in_repo"),
            ("SUPERZEJ_NAME_SCHEME", "numbered"),
            ("SUPERZEJ_AUTO_REMOVE_WORKTREE", "yes"),
            ("SUPERZEJ_REPO_SCAN_DEPTH", "9"),
            ("SUPERZEJ_PROFILE", "vim"),
            ("SUPERZEJ_THEME_ACCENT", "#abcdef"),
            ("SUPERZEJ_PR_TTL", "11"),
            ("SUPERZEJ_WATCH_PR_INTERVAL", "13"),
            ("SUPERZEJ_METRICS_INTERVAL_SECS", "3.5"),
            ("SUPERZEJ_METRICS_TIMEOUT_MS", "750"),
            ("SUPERZEJ_METRICS_MAX_BODY_BYTES", "2048"),
            ("SUPERZEJ_LOG_LEVEL", "debug"),
            ("SUPERZEJ_LOG_FILE", "true"),
            ("SUPERZEJ_LOG_DIR", "/logs"),
            ("SUPERZEJ_LOG_ROTATION_SIZE_MB", "8"),
            ("SUPERZEJ_LOG_MAX_FILES", "4"),
            ("SUPERZEJ_LOG_FORMAT", "json"),
            ("SUPERZEJ_SANDBOX_BACKEND", "docker"),
            ("SUPERZEJ_SANDBOX_NETWORK", "host"),
            ("SUPERZEJ_SANDBOX_IMAGE", "img:9"),
            ("SUPERZEJ_SANDBOX_ON_MISSING", "fail"),
            ("SUPERZEJ_SANDBOX_ENABLED", "off"),
            ("SUPERZEJ_SANDBOX_REMOTE_HOST", "user@box"),
        ]);
        let c = Config::load_layered(&env, &[], None);
        assert_eq!(c.worktrees_dir, "/wt");
        assert_eq!(c.workspaces_dir, "/ws");
        assert_eq!(c.base_branch, "develop");
        assert_eq!(c.branch_prefix, "x/");
        assert_eq!(c.picker, Picker::Fzf);
        assert_eq!(c.worktree_mode, WorktreeMode::InRepo);
        assert_eq!(c.name_scheme, NameScheme::Numbered);
        assert!(c.auto_remove_worktree);
        assert_eq!(c.repo_scan_depth, 9);
        assert_eq!(c.profile, "vim");
        assert_eq!(c.theme.accent, "#abcdef");
        assert_eq!(c.pr.ttl_secs, 11);
        assert_eq!(c.watch.pr_interval_secs, 13);
        assert_eq!(c.metrics.interval_secs, 3.5);
        assert_eq!(c.metrics.timeout_ms, 750);
        assert_eq!(c.metrics.max_body_bytes, 2048);
        assert_eq!(c.log.level, LogLevel::Debug);
        assert!(c.log.file);
        assert_eq!(c.log.dir, "/logs");
        assert_eq!(c.log.rotation_size_mb, 8);
        assert_eq!(c.log.max_files, 4);
        assert_eq!(c.log.format, LogFormat::Json);
        assert_eq!(c.sandbox.backend, SandboxBackend::Docker);
        assert_eq!(c.sandbox.network, Network::Host);
        assert_eq!(c.sandbox.image, "img:9");
        assert_eq!(c.sandbox.on_missing, OnMissing::Fail);
        assert!(!c.sandbox.enabled);
        assert_eq!(c.sandbox.remote.host, "user@box");
    }

    #[test]
    fn env_bad_values_warn_and_skip() {
        // Malformed number / bool / enum values are ignored (defaults survive).
        let env = map_env(&[
            ("SUPERZEJ_PR_TTL", "lots"),
            ("SUPERZEJ_AUTO_REMOVE_WORKTREE", "maybe"),
            ("SUPERZEJ_PICKER", "telescope"),
            ("SUPERZEJ_REPO_SCAN_DEPTH", "deep"),
        ]);
        let o = env_overlay(&env);
        assert_eq!(o.pr_ttl_secs, None);
        assert_eq!(o.auto_remove_worktree, None);
        assert_eq!(o.picker, None);
        assert_eq!(o.repo_scan_depth, None);
        // parse_bool accepts the documented spellings.
        assert_eq!(parse_bool("on", "k"), Some(true));
        assert_eq!(parse_bool("0", "k"), Some(false));
        assert_eq!(parse_bool("huh", "k"), None);
    }

    #[test]
    fn get_dotted_covers_all_keys() {
        let c = Config::default();
        for key in [
            "worktrees_dir",
            "workspaces_dir",
            "base_branch",
            "branch_prefix",
            "picker",
            "worktree_mode",
            "name_scheme",
            "auto_remove_worktree",
            "repo_scan_depth",
            "repo_roots",
            "theme.accent",
            "pr.ttl_secs",
            "watch.pr_interval_secs",
            "metrics.interval_secs",
            "metrics.timeout_ms",
            "metrics.max_body_bytes",
            "log.level",
            "log.file",
            "log.dir",
            "log.rotation_size_mb",
            "log.max_files",
            "log.format",
            "sandbox.enabled",
            "sandbox.backend",
            "sandbox.image",
            "sandbox.network",
            "sandbox.on_missing",
            "sandbox.remote.host",
            "sandbox.remote.transport",
            "sandbox.remote.mode",
        ] {
            assert!(c.get_dotted(key).is_some(), "missing dotted key: {key}");
        }
    }

    #[test]
    fn validate_str_flags_every_section() {
        assert!(
            validate_str("not = valid = toml")
                .iter()
                .any(|e| e.contains("syntax"))
        );
        let body = "\
picker = \"x\"
worktree_mode = \"y\"
name_scheme = \"z\"
[sandbox]
backend = \"bad\"
network = \"bad\"
on_missing = \"bad\"
[sandbox.remote]
transport = \"bad\"
mode = \"bad\"
[log]
level = \"bad\"
format = \"bad\"
";
        let errs = validate_str(body);
        assert_eq!(errs.len(), 10, "{errs:?}");
        assert!(validate_str("picker = \"auto\"\n").is_empty());
        // a non-table top-level is tolerated (no panic).
        assert!(validate_str("").is_empty());
    }

    #[test]
    fn accent_and_log_dir_helpers() {
        let mut c = Config::default();
        assert_eq!(c.accent_hex(), "#6ee7d8");
        assert!(c.accent_rgb().contains(';'));
        c.theme.accent = "#fff".into();
        assert_eq!(c.accent_rgb(), "255;255;255"); // 3-digit hex expands
        c.theme.accent = "garbage".into();
        assert_eq!(c.accent_hex(), "#6ee7d8"); // invalid falls back
        assert!(c.accent_rgb().len() > 3);
        // log dir: default vs explicit.
        assert!(c.log.dir_path().ends_with("superzej/logs"));
        c.log.dir = "~/x".into();
        assert!(!c.log.dir_path().to_string_lossy().contains('~'));
        assert!(!c.sandbox.remote.is_remote());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn non_default_enums_roundtrip() {
        // Exercises Serialize (as_str) for the non-default variants.
        let mut c = Config::default();
        c.picker = Picker::Select;
        c.worktree_mode = WorktreeMode::InRepo;
        c.name_scheme = NameScheme::Numbered;
        c.sandbox.backend = SandboxBackend::Podman;
        c.sandbox.network = Network::None;
        c.sandbox.on_missing = OnMissing::Prompt;
        c.sandbox.remote.transport = RemoteTransport::Ssh;
        c.sandbox.remote.mode = RemoteMode::Sshfs;
        c.log.level = LogLevel::Trace;
        c.log.format = LogFormat::Json;
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.sandbox.remote.transport, RemoteTransport::Ssh);
        assert_eq!(back.sandbox.remote.mode, RemoteMode::Sshfs);
        assert_eq!(back.log.level, LogLevel::Trace);
        assert_eq!(back.log.format, LogFormat::Json);
        assert_eq!(back.sandbox.on_missing, OnMissing::Prompt);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults() {
        let dir = tmpdir("bad");
        let f = dir.join("c.toml");
        std::fs::write(&f, "this is = = not toml\n").unwrap();
        let c = Config::load_layered(&MapEnv::default(), &[], Some(f));
        assert_eq!(c.picker, Picker::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_is_under_xdg_config() {
        assert!(Config::path().ends_with("superzej/config.toml"));
    }

    #[test]
    fn repo_sandbox_expands_mount_tildes() {
        let cfg = Config::default();
        let dir = tmpdir("mounts");
        let sb = cfg.repo_sandbox(&dir);
        // default mount "~/.gitconfig:ro" → tilde expanded, :ro preserved.
        assert!(
            sb.mounts
                .iter()
                .any(|m| m.ends_with("/.gitconfig:ro") && !m.starts_with('~'))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // A repo overlay that sets *every* sandbox + remote field exercises all the
    // overlay `apply` branches.
    #[test]
    fn agent_command() {
        let mut cfg = Config::default();
        cfg.agents.push(crate::config::NamedCommand {
            name: "test".into(),
            command: "echo test".into(),
            hints: vec![],
            provider: None,
        });
        assert_eq!(cfg.agent_command("test"), Some("echo test"));
        assert_eq!(cfg.agent_command("missing"), None);
    }

    #[test]
    fn tool_command() {
        let mut cfg = Config::default();
        cfg.tools.push(crate::config::NamedCommand {
            name: "test".into(),
            command: "echo test".into(),
            hints: vec![],
            provider: None,
        });
        assert_eq!(cfg.tool_command("test"), Some("echo test"));
        assert_eq!(cfg.tool_command("missing"), None);
    }

    #[test]
    fn tasks_parse_and_filter_tests() {
        let cfg: Config = toml::from_str(
            r#"
            [[tasks]]
            name = "unit"
            command = "cargo"
            args = ["test"]
            kind = "test"
            matcher = "cargo-test"

            [[tasks]]
            name = "serve"
            command = "npm run dev"
            kind = "run"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.tasks.len(), 2);
        let tests = cfg.test_tasks();
        assert_eq!(tests.len(), 1);
        assert_eq!(tests[0].name, "unit");
        assert_eq!(tests[0].matcher.as_deref(), Some("cargo-test"));
    }

    #[test]
    fn pin_and_pin_by_index() {
        let mut cfg = Config::default();
        cfg.pins.push(crate::config::Pin {
            name: "test".into(),
            command: "echo test".into(),
            scope: crate::config::PinScope::Global,
            workspace: None,
            cwd: None,
            start: crate::config::PinStart::Lazy,
            restart: crate::config::PinRestart::Never,
            singleton: false,
            location: crate::config::PinLocation::Tab,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            label: None,
            ratio: None,
            corner: crate::config::PinCorner::BottomRight,
            corner_width: None,
            corner_height: None,
        });
        assert_eq!(cfg.pin("test").unwrap().name, "test");
        assert!(cfg.pin("missing").is_none());
        assert_eq!(cfg.pin_by_index(1).unwrap().name, "test");
        assert!(cfg.pin_by_index(0).is_none());
        assert!(cfg.pin_by_index(2).is_none());
    }

    #[test]
    fn pins_for_workspace() {
        let mut cfg = Config::default();
        cfg.pins.push(crate::config::Pin {
            name: "global".into(),
            command: "echo test".into(),
            scope: crate::config::PinScope::Global,
            workspace: None,
            cwd: None,
            start: crate::config::PinStart::Lazy,
            restart: crate::config::PinRestart::Never,
            singleton: false,
            location: crate::config::PinLocation::Tab,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            label: None,
            ratio: None,
            corner: crate::config::PinCorner::BottomRight,
            corner_width: None,
            corner_height: None,
        });
        cfg.pins.push(crate::config::Pin {
            name: "local".into(),
            command: "echo test".into(),
            scope: crate::config::PinScope::Workspace,
            workspace: Some("repo".into()),
            cwd: None,
            start: crate::config::PinStart::Lazy,
            restart: crate::config::PinRestart::Never,
            singleton: false,
            location: crate::config::PinLocation::Tab,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            label: None,
            ratio: None,
            corner: crate::config::PinCorner::BottomRight,
            corner_width: None,
            corner_height: None,
        });
        cfg.pins.push(crate::config::Pin {
            name: "local_any".into(),
            command: "echo test".into(),
            scope: crate::config::PinScope::Workspace,
            workspace: None,
            cwd: None,
            start: crate::config::PinStart::Lazy,
            restart: crate::config::PinRestart::Never,
            singleton: false,
            location: crate::config::PinLocation::Tab,
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            label: None,
            ratio: None,
            corner: crate::config::PinCorner::BottomRight,
            corner_width: None,
            corner_height: None,
        });
        let none_pins = cfg.pins_for_workspace(None);
        assert_eq!(none_pins.len(), 1); // just global
        assert!(none_pins.iter().any(|p| p.name == "global"));
        let some_pins = cfg.pins_for_workspace(Some("repo"));
        assert_eq!(some_pins.len(), 2); // global, local
    }

    #[test]
    fn full_repo_overlay_applies_every_field() {
        let cfg = Config::default();
        let dir = tmpdir("full");
        std::fs::write(
            dir.join(".superzej.toml"),
            "\
[sandbox]
enabled = false
backend = \"docker\"
backend_chain = [\"docker\", \"none\"]
image = \"img:2\"
network = \"none\"
env_passthrough = [\"FOO\"]
mounts = [\"/a:/b\"]
init_script = \"echo go\"
devenv = true
on_missing = \"fail\"
[sandbox.remote]
host = \"u@h\"
port = 2200
transport = \"ssh\"
mode = \"sshfs\"
remote_dir = \"/srv/wt\"
forward_agent = false
",
        )
        .unwrap();
        let sb = cfg.repo_sandbox(&dir);
        assert!(!sb.enabled);
        assert_eq!(sb.backend, SandboxBackend::Docker);
        assert_eq!(sb.backend_chain, vec!["docker", "none"]);
        assert_eq!(sb.image, "img:2");
        assert_eq!(sb.network, Network::None);
        assert_eq!(sb.env_passthrough, vec!["FOO"]);
        assert!(sb.devenv);
        assert_eq!(sb.on_missing, OnMissing::Fail);
        assert_eq!(sb.remote.host, "u@h");
        assert_eq!(sb.remote.port, 2200);
        assert_eq!(sb.remote.transport, RemoteTransport::Ssh);
        assert_eq!(sb.remote.mode, RemoteMode::Sshfs);
        assert_eq!(sb.remote.remote_dir, "/srv/wt");
        assert!(!sb.remote.forward_agent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- named execution environments (`[env.<name>]`) -----------------------

    #[test]
    fn default_env_reproduces_legacy_behavior() {
        // No [env.*] defined → the implicit "default" env: base [sandbox] +
        // a placement derived from [sandbox.remote] + the GitLoc (today's path).
        let cfg = Config::default();
        let dir = tmpdir("env-default");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, &dir, None);
        assert_eq!(env.name, "default");
        assert!(env.placement.is_local());
        assert!(!env.is_remote());
        // The resolved sandbox equals repo_sandbox (modulo identical content).
        assert_eq!(env.sandbox.backend, cfg.repo_sandbox(&dir).backend);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn named_env_overlays_isolation_and_local_placement() {
        let cfg: Config = toml::from_str(
            "\
[env.local-containers]
placement = \"local\"
[env.local-containers.sandbox]
backend = \"podman\"
image = \"registry.example.com/dev:latest\"
profile = \"sealed\"
",
        )
        .unwrap();
        let dir = tmpdir("env-local");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, &dir, Some("local-containers"));
        assert_eq!(env.name, "local-containers");
        assert!(env.placement.is_local());
        assert_eq!(env.sandbox.backend, SandboxBackend::Podman);
        assert_eq!(env.sandbox.image, "registry.example.com/dev:latest");
        assert_eq!(env.sandbox.profile, SandboxProfile::Sealed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_connect_parse_and_default() {
        assert_eq!(ProviderConnect::default(), ProviderConnect::Exec);
        assert_eq!(
            ProviderConnect::from_str_validated("ssh"),
            Ok(ProviderConnect::Ssh)
        );
        assert_eq!(
            ProviderConnect::from_str_validated("exec"),
            Ok(ProviderConnect::Exec)
        );
        assert!(ProviderConnect::from_str_validated("nope").is_err());
        assert_eq!(ProviderConnect::Ssh.as_str(), "ssh");
        // Parses from an env provider table.
        let cfg: Config = toml::from_str(
            "[env.x]\nplacement = \"provider\"\n[env.x.provider]\nprovider = \"sprites\"\nconnect = \"ssh\"\n",
        )
        .unwrap();
        assert_eq!(cfg.env["x"].provider.connect, ProviderConnect::Ssh);
    }

    #[test]
    fn home_config_default_is_portable_and_safe() {
        let h = HomeConfig::default();
        assert_eq!(h.strategy, ShellStrategy::Portable);
        assert!(
            h.portable_dotfiles_only,
            "safe default: drop non-portable rc"
        );
        assert!(!h.is_enabled(), "strategy alone is not 'enabled'");
    }

    #[test]
    fn sandbox_overlay_merges_home_strategy_per_env() {
        // Global portable; one env asks for host-parity, another for clean.
        let cfg: Config = toml::from_str(
            "\
[sandbox.home]
strategy = \"portable\"
tools = [\"fd\", \"fzf\"]
[env.bigbox]
placement = \"local\"
[env.bigbox.sandbox.home]
strategy = \"host-parity\"
[env.sprite]
placement = \"local\"
[env.sprite.sandbox.home]
strategy = \"clean\"
",
        )
        .unwrap();
        let dir = tmpdir("env-home");
        let loc = GitLoc::Local(dir.clone());
        let big = cfg.resolve_env(&dir, &loc, &dir, Some("bigbox"));
        let sprite = cfg.resolve_env(&dir, &loc, &dir, Some("sprite"));
        let dflt = cfg.resolve_env(&dir, &loc, &dir, Some("nope-default"));
        assert_eq!(big.sandbox.home.strategy, ShellStrategy::HostParity);
        assert_eq!(sprite.sandbox.home.strategy, ShellStrategy::Clean);
        assert_eq!(dflt.sandbox.home.strategy, ShellStrategy::Portable);
        // Field-merge: the override only set `strategy`, so global `tools` inherit.
        assert_eq!(
            big.sandbox.home.tools,
            vec!["fd".to_string(), "fzf".to_string()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn home_overlay_apply_field_merges_present_only() {
        let mut base = HomeConfig {
            tools: vec!["fd".into()],
            strategy: ShellStrategy::Portable,
            portable_dotfiles_only: true,
            ..HomeConfig::default()
        };
        let ov = HomeOverlay {
            strategy: Some(ShellStrategy::Clean),
            ..HomeOverlay::default()
        };
        ov.apply(&mut base);
        assert_eq!(base.strategy, ShellStrategy::Clean); // present → replaced
        assert_eq!(base.tools, vec!["fd".to_string()]); // absent → inherited
        assert!(base.portable_dotfiles_only); // absent → inherited
    }

    #[test]
    fn home_overlay_merges_atuin_and_is_enabled_counts_it() {
        // Per-env opt-in: present overrides, absent inherits.
        let mut base = HomeConfig::default();
        assert!(!base.atuin && !base.is_enabled(), "off by default");
        HomeOverlay {
            atuin: Some(true),
            ..HomeOverlay::default()
        }
        .apply(&mut base);
        assert!(base.atuin, "overlay turns atuin on");
        assert!(base.is_enabled(), "atuin alone enables the personal layer");
        // An overlay without `atuin` leaves the base value untouched.
        let mut on = HomeConfig {
            atuin: true,
            ..HomeConfig::default()
        };
        HomeOverlay::default().apply(&mut on);
        assert!(on.atuin, "absent overlay key inherits the base");
        assert!(HomeOverlay::default().atuin.is_none());
    }

    #[test]
    fn sandbox_overlay_is_empty_accounts_for_home() {
        let empty = SandboxOverlay::default();
        assert!(empty.is_empty());
        let with_home = SandboxOverlay {
            home: Some(HomeOverlay {
                strategy: Some(ShellStrategy::Clean),
                ..HomeOverlay::default()
            }),
            ..SandboxOverlay::default()
        };
        assert!(
            !with_home.is_empty(),
            "a home strategy override makes it non-empty"
        );
        // An all-None HomeOverlay does NOT make the overlay non-empty.
        let blank_home = SandboxOverlay {
            home: Some(HomeOverlay::default()),
            ..SandboxOverlay::default()
        };
        assert!(blank_home.is_empty());
    }

    #[test]
    fn k8s_env_builds_kubectl_placement() {
        let cfg: Config = toml::from_str(
            "\
[env.company-k8s]
placement = \"k8s\"
[env.company-k8s.sandbox]
backend = \"none\"
[env.company-k8s.k8s]
context = \"company-prod\"
namespace = \"dev-blake\"
pod = \"sz-dev\"
",
        )
        .unwrap();
        let dir = tmpdir("env-k8s");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, &dir, Some("company-k8s"));
        assert!(env.is_remote());
        assert_eq!(env.placement.label(), "k8s:dev-blake/sz-dev");
        // The kubectl exec argv carries the configured context/namespace/pod.
        let argv = env.placement.interactive_argv(&["true".into()]);
        assert_eq!(argv[0], "kubectl");
        assert!(argv.windows(2).any(|w| w == ["--context", "company-prod"]));
        assert!(argv.windows(2).any(|w| w == ["--namespace", "dev-blake"]));
        assert!(argv.contains(&"sz-dev".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_env_substitutes_id_into_exec_template() {
        let cfg: Config = toml::from_str(
            "\
[env.daytona]
placement = \"provider\"
[env.daytona.provider]
provider = \"daytona\"
id = \"sb-42\"
exec_command = [\"daytona\", \"ssh\", \"{id}\", \"--\"]
",
        )
        .unwrap();
        let dir = tmpdir("env-prov");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, &dir, Some("daytona"));
        assert!(env.is_remote());
        let argv = env.placement.interactive_argv(&["ls".into()]);
        assert_eq!(&argv[..4], &["daytona", "ssh", "sb-42", "--"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_env_lifecycle_commands_substitute_id() {
        let cfg: Config = toml::from_str(
            "\
[env.daytona]
placement = \"provider\"
[env.daytona.provider]
provider = \"daytona\"
id = \"sb-7\"
exec_command = [\"daytona\", \"ssh\", \"{id}\", \"--\"]
up_command = [\"daytona\", \"create\", \"--id\", \"{id}\"]
down_command = [\"daytona\", \"delete\", \"{id}\"]
",
        )
        .unwrap();
        let dir = tmpdir("env-prov-life");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, &dir, Some("daytona"));
        match env.placement {
            crate::placement::Placement::Provider(p) => {
                assert_eq!(p.up_command, vec!["daytona", "create", "--id", "sb-7"]);
                assert_eq!(p.down_command, vec!["daytona", "delete", "sb-7"]);
            }
            other => panic!("expected provider placement, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn env_selection_precedence_repo_over_global_default() {
        // Global default_env picks "g"; a repo .superzej.toml `env = "r"` wins;
        // an explicit `selected` beats both.
        let cfg: Config = toml::from_str(
            "\
[sandbox]
default_env = \"g\"
[env.g]
[env.g.sandbox]
backend = \"bwrap\"
[env.r]
[env.r.sandbox]
backend = \"docker\"
[env.x]
[env.x.sandbox]
backend = \"podman\"
",
        )
        .unwrap();
        let dir = tmpdir("env-prec");
        std::fs::write(dir.join(".superzej.toml"), "env = \"r\"\n").unwrap();
        let loc = GitLoc::Local(dir.clone());
        // No explicit selection → repo overlay "r" wins over global default "g".
        assert_eq!(cfg.resolve_env(&dir, &loc, &dir, None).name, "r");
        assert_eq!(
            cfg.resolve_env(&dir, &loc, &dir, None).sandbox.backend,
            SandboxBackend::Docker
        );
        // Explicit selection beats the repo overlay.
        assert_eq!(cfg.resolve_env(&dir, &loc, &dir, Some("x")).name, "x");
        // Empty/whitespace selection is ignored (falls through to repo).
        assert_eq!(cfg.resolve_env(&dir, &loc, &dir, Some("  ")).name, "r");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_env_name_falls_back_to_default() {
        let cfg = Config::default();
        let dir = tmpdir("env-unknown");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, &dir, Some("does-not-exist"));
        assert_eq!(env.name, "default");
        assert!(env.placement.is_local());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ssh_env_falls_back_to_global_remote_host() {
        // [env.*.ssh] with no host inherits [sandbox.remote] host.
        let cfg: Config = toml::from_str(
            "\
[sandbox.remote]
host = \"u@devbox\"
port = 2200
[env.remote-dev]
placement = \"ssh\"
[env.remote-dev.ssh]
transport = \"ssh\"
",
        )
        .unwrap();
        let dir = tmpdir("env-ssh");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, &dir, Some("remote-dev"));
        assert!(env.is_remote());
        assert_eq!(env.placement.label(), "ssh:u@devbox");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn process_env_reads_real_vars() {
        // SAFETY: single-threaded test using uniquely-named vars.
        unsafe { std::env::set_var("SUPERZEJ_TEST_PENV_xyz", "v1") };
        assert_eq!(
            ProcessEnv.get("SUPERZEJ_TEST_PENV_xyz").as_deref(),
            Some("v1")
        );
        assert!(ProcessEnv.get("SUPERZEJ_TEST_PENV_absent_qqq").is_none());
        unsafe { std::env::remove_var("SUPERZEJ_TEST_PENV_xyz") };
        // blank values are treated as unset.
        unsafe { std::env::set_var("SUPERZEJ_TEST_PENV_blank", "   ") };
        assert!(ProcessEnv.get("SUPERZEJ_TEST_PENV_blank").is_none());
        unsafe { std::env::remove_var("SUPERZEJ_TEST_PENV_blank") };
    }

    #[test]
    fn config_parses_all_mode_specific_keybind_tables() {
        let toml = r#"
            [keybinds]
            new-worktree = "Ctrl w"

            [keybinds.vim_normal]
            focus-down = "j"

            [keybinds.vim_insert]
            mode-vim-normal = "Esc"

            [keybinds.emacs]
            focus-left = "Ctrl b"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.keybinds.get("new-worktree").unwrap(), "Ctrl w");
        assert_eq!(cfg.keybinds.vim_normal.get("focus-down").unwrap(), "j");
        assert_eq!(
            cfg.keybinds.vim_insert.get("mode-vim-normal").unwrap(),
            "Esc"
        );
        assert_eq!(cfg.keybinds.emacs.get("focus-left").unwrap(), "Ctrl b");
    }

    #[test]
    fn agent_and_tool_command_lookup_by_name() {
        let cfg: Config = toml::from_str(
            "[[agents]]\nname = 'claude'\ncommand = 'claude --foo'\n\
             [[tools]]\nname = 'lazygit'\ncommand = 'lazygit'\n",
        )
        .unwrap();
        assert_eq!(cfg.agent_command("claude"), Some("claude --foo"));
        assert_eq!(cfg.agent_command("nope"), None);
        assert_eq!(cfg.tool_command("lazygit"), Some("lazygit"));
        assert_eq!(cfg.tool_command("nope"), None);
    }

    #[test]
    fn pin_lookup_by_name_and_index_and_workspace_scope() {
        let cfg: Config = toml::from_str(
            "[[pins]]\nname = 'aerc'\ncommand = 'aerc'\n\
             [[pins]]\nname = 'logs'\ncommand = 'journalctl -f'\nscope = 'workspace'\nworkspace = '/repo'\n",
        )
        .unwrap();
        // By name.
        assert_eq!(cfg.pin("aerc").map(|p| p.name.as_str()), Some("aerc"));
        assert!(cfg.pin("missing").is_none());
        // By 1-based index (0 and out-of-range miss).
        assert_eq!(cfg.pin_by_index(1).map(|p| p.name.as_str()), Some("aerc"));
        assert_eq!(cfg.pin_by_index(2).map(|p| p.name.as_str()), Some("logs"));
        assert!(cfg.pin_by_index(0).is_none());
        assert!(cfg.pin_by_index(3).is_none());
        // Workspace scoping: global pin always shows; workspace pin only for its repo.
        let global_only = cfg.pins_for_workspace(None);
        assert_eq!(global_only.len(), 1);
        assert_eq!(global_only[0].name, "aerc");
        let in_repo = cfg.pins_for_workspace(Some("/repo"));
        assert_eq!(in_repo.len(), 2);
    }

    #[test]
    fn expand_env_ref_resolves_env_prefix() {
        unsafe { std::env::set_var("SUPERZEJ_TEST_EXPAND_TOKEN", "secret") };
        assert_eq!(
            expand_env_ref("env:SUPERZEJ_TEST_EXPAND_TOKEN"),
            Some("secret".into())
        );
        unsafe { std::env::remove_var("SUPERZEJ_TEST_EXPAND_TOKEN") };
        // Missing var returns None.
        assert_eq!(expand_env_ref("env:SUPERZEJ_TEST_EXPAND_TOKEN"), None);
    }

    #[test]
    fn expand_env_ref_returns_literal_for_plain_value() {
        assert_eq!(expand_env_ref("lin_abc123"), Some("lin_abc123".into()));
    }

    #[test]
    fn profile_toml_overlay_merges_over_base_and_preserves_untouched() {
        let mut cfg = Config {
            branch_prefix: "sz/".into(),
            ..Config::default()
        };
        let base_accent = cfg.theme.accent.clone();
        // A profile overlay changes branch_prefix + a nested sandbox field, and
        // leaves theme.accent untouched.
        Config::apply_toml_overlay(
            &mut cfg,
            "branch_prefix = \"work/\"\n[sandbox]\nnetwork = \"none\"\n",
        )
        .unwrap();
        assert_eq!(cfg.branch_prefix, "work/", "overlay wins");
        assert_eq!(cfg.sandbox.network, Network::None, "nested overlay applies");
        assert_eq!(
            cfg.theme.accent, base_accent,
            "untouched base key preserved"
        );
    }

    #[test]
    fn profile_overlay_path_none_for_default_some_for_named() {
        struct FakeEnv(Option<String>);
        impl EnvSource for FakeEnv {
            fn get(&self, k: &str) -> Option<String> {
                (k == "SUPERZEJ_PROFILE").then(|| self.0.clone()).flatten()
            }
        }
        assert!(Config::profile_overlay_path(&FakeEnv(None)).is_none());
        assert!(Config::profile_overlay_path(&FakeEnv(Some("default".into()))).is_none());
        let p = Config::profile_overlay_path(&FakeEnv(Some("work".into()))).unwrap();
        assert!(p.ends_with("superzej/profiles/work/config.toml"));
    }

    #[test]
    fn expand_env_ref_returns_none_for_empty() {
        assert_eq!(expand_env_ref(""), None);
        assert_eq!(expand_env_ref("   "), None);
    }

    #[test]
    fn issue_provider_kind_infallible_deserialize() {
        let k: IssueProviderKind = serde_json::from_str(r#""linear""#).unwrap();
        assert_eq!(k, IssueProviderKind::Linear);
        // Unknown value falls back to default (None) without panic.
        let k: IssueProviderKind = serde_json::from_str(r#""unknown_provider""#).unwrap();
        assert_eq!(k, IssueProviderKind::None);
    }

    #[test]
    fn issues_config_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.issues.provider, IssueProviderKind::None);
        assert_eq!(cfg.issues.ttl_secs, 60);
        assert_eq!(cfg.issues.max_issues, 100);
        assert!(cfg.issues.filter_assignee_me);
        assert_eq!(cfg.issues.linear.api_key, "env:LINEAR_API_KEY");
        assert_eq!(cfg.issues.jira.api_token, "env:JIRA_API_TOKEN");
        assert!(cfg.issues.providers.is_empty());
        assert!(cfg.issues.active_providers().is_empty());
    }

    #[test]
    fn active_providers_back_compat_single() {
        // Legacy single `provider` is honored when `providers` is empty.
        let mut cfg = IssuesConfig {
            provider: IssueProviderKind::Linear,
            ..Default::default()
        };
        assert_eq!(cfg.active_providers(), vec![IssueProviderKind::Linear]);
        // `none` yields an empty set, not a [None] entry.
        cfg.provider = IssueProviderKind::None;
        assert!(cfg.active_providers().is_empty());
    }

    #[test]
    fn active_providers_multi_wins_and_dedups() {
        // Single provider is overridden once the plural list is set.
        let cfg = IssuesConfig {
            provider: IssueProviderKind::Github,
            providers: vec![
                IssueProviderKind::Linear,
                IssueProviderKind::Jira,
                IssueProviderKind::Linear, // duplicate collapses
                IssueProviderKind::None,   // None filtered out
            ],
            ..Default::default()
        };
        assert_eq!(
            cfg.active_providers(),
            vec![IssueProviderKind::Linear, IssueProviderKind::Jira]
        );
    }

    #[test]
    fn issues_multi_provider_table_parses() {
        let toml = r#"
            [issues]
            providers = ["linear", "jira"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.issues.providers,
            vec![IssueProviderKind::Linear, IssueProviderKind::Jira]
        );
        assert_eq!(
            cfg.issues.active_providers(),
            vec![IssueProviderKind::Linear, IssueProviderKind::Jira]
        );
    }

    #[test]
    fn notification_priority_defaults_and_overrides() {
        use crate::notification::{NotificationKind, Priority};
        let mut cfg = NotificationsConfig::default();

        // Defaults: failures alert, lifecycle info, the rest notice.
        assert_eq!(
            cfg.priority_of(NotificationKind::TestFailed),
            Priority::Alert
        );
        assert_eq!(
            cfg.priority_of(NotificationKind::WorktreeCreated),
            Priority::Info
        );
        assert_eq!(
            cfg.priority_of(NotificationKind::AgentDone),
            Priority::Notice
        );

        // Alert set is the failures + the agent-attention request; unread excludes Info.
        let alerts = cfg.alert_kind_names();
        assert_eq!(alerts.len(), 5);
        for k in [
            "agent_failed",
            "agent_attention",
            "test_failed",
            "log_error",
            "process_failed",
        ] {
            assert!(alerts.contains(&k), "missing {k}");
        }
        let counted = cfg.counted_unread_kind_names();
        assert!(!counted.contains(&"worktree_created"));
        assert!(!counted.contains(&"process_exited"));
        assert!(counted.contains(&"test_failed") && counted.contains(&"agent_done"));

        // Override: garbage falls back to default; a demotion reclassifies live.
        cfg.priority.insert("test_failed".into(), "garbage".into());
        assert_eq!(
            cfg.priority_of(NotificationKind::TestFailed),
            Priority::Alert
        );
        cfg.priority.insert("test_failed".into(), "notice".into());
        assert_eq!(
            cfg.priority_of(NotificationKind::TestFailed),
            Priority::Notice
        );
        assert!(!cfg.alert_kind_names().contains(&"test_failed"));
        assert!(cfg.counted_unread_kind_names().contains(&"test_failed"));
        // Promote an info kind to alert.
        cfg.priority
            .insert("worktree_created".into(), "alert".into());
        assert!(cfg.alert_kind_names().contains(&"worktree_created"));
    }

    #[test]
    fn notification_routing_config_parses() {
        let toml = r#"
[notifications]
active_mode = "focus"

[notifications.dnd]
enabled = true
windows = ["22:00-08:00", "sat,sun 00:00-24:00"]
allow_priority = "notice"

[notifications.sound]
mode = "command"
min_priority = "notice"
command = "paplay alert.oga"

[notifications.sound.per_priority]
alert = "paplay crit.oga"

[notifications.modes.focus]
label = "Heads down"

[[notifications.rules]]
name = "mute noisy"
worktree = "*/scratch"
mute = true

[[notifications.rules]]
kind = "agent_done"
set_priority = "alert"
stop = true
"#;
        let cfg: Config = toml::from_str(toml).expect("parses");
        let n = &cfg.notifications;
        assert_eq!(n.active_mode, "focus");
        assert!(n.dnd.enabled);
        assert_eq!(n.dnd.windows.len(), 2);
        assert_eq!(n.dnd.allow_priority, "notice");
        assert_eq!(n.sound.mode, SoundMode::Command);
        assert_eq!(n.sound.command, "paplay alert.oga");
        assert_eq!(
            n.sound.per_priority.get("alert").unwrap(),
            "paplay crit.oga"
        );
        assert!(n.modes.contains_key("focus"));
        assert_eq!(n.rules.len(), 2);
        assert_eq!(n.rules[0].worktree.as_deref(), Some("*/scratch"));
        assert!(n.rules[0].mute);
        assert_eq!(n.rules[1].kind.as_deref(), Some("agent_done"));
        assert!(n.rules[1].stop);
    }

    #[test]
    fn notification_bad_sound_mode_warns_and_defaults() {
        let cfg: Config = toml::from_str(
            r#"
[notifications.sound]
mode = "bogus"
"#,
        )
        .expect("parses");
        // Unknown enum value warns and falls back to the default (Bell).
        assert_eq!(cfg.notifications.sound.mode, SoundMode::Bell);
    }

    #[test]
    fn profile_notifications_overlay_layers() {
        let toml = r#"
profile = "work"

[notifications]
desktop = true
active_mode = "all"

[notifications.sound]
mode = "bell"

[profiles.work.notifications]
active_mode = "focus"

[profiles.work.notifications.sound]
mode = "off"
min_priority = "alert"
"#;
        let cfg: Config = toml::from_str(toml).expect("parses");
        // Global untouched.
        assert_eq!(cfg.notifications.active_mode, "all");
        assert_eq!(cfg.notifications.sound.mode, SoundMode::Bell);
        // Effective (no repo root) applies the active profile overlay.
        let eff = cfg.effective_notifications(None);
        assert_eq!(eff.active_mode, "focus");
        assert_eq!(eff.sound.mode, SoundMode::Off);
        // A field the overlay didn't set inherits the global value.
        assert!(eff.desktop);
    }

    #[test]
    fn effective_notifications_no_profile_is_identity() {
        let cfg = Config::default();
        let eff = cfg.effective_notifications(None);
        assert_eq!(eff.active_mode, cfg.notifications.active_mode);
        assert_eq!(eff.sound.mode, cfg.notifications.sound.mode);
    }

    #[test]
    fn notifications_overlay_apply_covers_every_field() {
        // is_empty on the default overlay.
        assert!(NotificationsOverlay::default().is_empty());

        let mut base = NotificationsConfig::default();
        let mut priority = std::collections::BTreeMap::new();
        priority.insert("agent_done".to_string(), "alert".to_string());
        let mut modes = std::collections::BTreeMap::new();
        modes.insert("focus".to_string(), NotificationMode::default());
        let overlay = NotificationsOverlay {
            desktop: Some(false),
            desktop_min_urgency: Some("critical".into()),
            process_exit: Some("all".into()),
            priority: Some(priority),
            rules: Some(vec![NotificationRule {
                drop: true,
                ..Default::default()
            }]),
            dnd: Some(DndConfig {
                enabled: true,
                windows: vec!["22:00-08:00".into()],
                allow_priority: "notice".into(),
            }),
            sound: Some(SoundConfig {
                mode: SoundMode::Off,
                ..Default::default()
            }),
            modes: Some(modes),
            active_mode: Some("focus".into()),
        };
        assert!(!overlay.is_empty());
        overlay.apply(&mut base);
        assert!(!base.desktop);
        assert_eq!(base.desktop_min_urgency, "critical");
        assert_eq!(base.process_exit, "all");
        assert_eq!(base.priority.get("agent_done").unwrap(), "alert");
        assert!(base.has_rules());
        assert!(base.dnd.enabled);
        assert_eq!(base.sound.mode, SoundMode::Off);
        assert!(base.modes.contains_key("focus"));
        assert_eq!(base.active_mode, "focus");
    }

    #[test]
    fn llm_proxy_disabled_by_default_no_launch() {
        let cfg = Config::default();
        assert!(!cfg.llm_proxy.enabled);
        assert_eq!(cfg.llm_proxy.listen, "127.0.0.1:8383");
        assert_eq!(cfg.llm_proxy.routing, RoutingStrategy::Sequential);
        assert!(cfg.llm_proxy.launch_spec().is_none());
    }

    #[test]
    fn llm_proxy_launch_spec_when_enabled() {
        let mut cfg = LlmProxyConfig {
            enabled: true,
            config_path: "/etc/szproxy.json".into(),
            ..Default::default()
        };
        let (prog, args, env) = cfg.launch_spec().unwrap();
        assert_eq!(prog, "szproxy");
        assert!(args.is_empty());
        assert_eq!(env.get("SZPROXY_LISTEN").unwrap(), "127.0.0.1:8383");
        assert_eq!(env.get("SZPROXY_CONFIG").unwrap(), "/etc/szproxy.json");
        // No config path → no SZPROXY_CONFIG env entry.
        cfg.config_path = String::new();
        let (_, _, env) = cfg.launch_spec().unwrap();
        assert!(!env.contains_key("SZPROXY_CONFIG"));
    }

    #[test]
    fn routing_strategy_aliases_and_fallback() {
        assert_eq!(
            RoutingStrategy::from_str_validated("failover").unwrap(),
            RoutingStrategy::Sequential
        );
        assert_eq!(
            RoutingStrategy::from_str_validated("cascade").unwrap(),
            RoutingStrategy::Speculative
        );
        // Unknown deserializes to the default without panic.
        let s: RoutingStrategy = serde_json::from_str(r#""nonsense""#).unwrap();
        assert_eq!(s, RoutingStrategy::Sequential);
    }

    // ---- config_enum! Default + Display + from_str_validated round-trips ----

    #[test]
    fn config_enum_defaults_and_displays() {
        // Default variant matches the macro `default =` clause for every enum.
        assert_eq!(Picker::default(), Picker::Auto);
        assert_eq!(UndercurlMode::default(), UndercurlMode::Auto);
        assert_eq!(WorktreeMode::default(), WorktreeMode::Global);
        assert_eq!(NameScheme::default(), NameScheme::Words);
        assert_eq!(SandboxBackend::default(), SandboxBackend::Auto);
        assert_eq!(Network::default(), Network::Nat);
        assert_eq!(OnMissing::default(), OnMissing::Warn);
        assert_eq!(RemoteTransport::default(), RemoteTransport::Mosh);
        assert_eq!(RemoteMode::default(), RemoteMode::Remote);
        assert_eq!(LogLevel::default(), LogLevel::Info);
        assert_eq!(LogFormat::default(), LogFormat::Text);
        assert_eq!(PinLocation::default(), PinLocation::Tab);
        assert_eq!(PinScope::default(), PinScope::Global);
        assert_eq!(RoutingStrategy::default(), RoutingStrategy::Sequential);
        assert_eq!(CompressionLevel::default(), CompressionLevel::Conservative);
        assert_eq!(GitCmdOutput::default(), GitCmdOutput::Popup);
        assert_eq!(IssueProviderKind::default(), IssueProviderKind::None);

        // Display delegates to as_str (canonical form).
        assert_eq!(UndercurlMode::On.to_string(), "on");
        assert_eq!(UndercurlMode::Off.to_string(), "off");
        assert_eq!(WorktreeMode::InRepo.to_string(), "in_repo");
        assert_eq!(NameScheme::Numbered.to_string(), "numbered");
        assert_eq!(LogLevel::Trace.to_string(), "trace");
        assert_eq!(GitCmdOutput::Terminal.to_string(), "terminal");
        assert_eq!(GitCmdOutput::None.to_string(), "none");
        assert_eq!(CompressionLevel::Aggressive.to_string(), "aggressive");
        assert_eq!(IssueProviderKind::Jira.to_string(), "jira");
    }

    #[test]
    fn config_enum_every_variant_roundtrips_canon_and_aliases() {
        // SandboxBackend: each canonical + alias parses to its variant; as_str
        // emits the canonical string.
        for (s, v) in [
            ("auto", SandboxBackend::Auto),
            ("podman", SandboxBackend::Podman),
            ("podman-rootless", SandboxBackend::Podman),
            ("rootless-podman", SandboxBackend::Podman),
            ("podman-rootful", SandboxBackend::PodmanRootful),
            ("rootful-podman", SandboxBackend::PodmanRootful),
            ("docker", SandboxBackend::Docker),
            ("bwrap", SandboxBackend::Bwrap),
            ("bubblewrap", SandboxBackend::Bwrap),
            ("systemd", SandboxBackend::Systemd),
            ("systemd-run", SandboxBackend::Systemd),
            ("apple", SandboxBackend::Apple),
            ("container", SandboxBackend::Apple),
            ("wsl", SandboxBackend::Wsl),
            ("none", SandboxBackend::None),
            ("host", SandboxBackend::None),
        ] {
            assert_eq!(SandboxBackend::from_str_validated(s).unwrap(), v, "{s}");
        }
        assert_eq!(SandboxBackend::Systemd.as_str(), "systemd");
        assert_eq!(SandboxBackend::Apple.as_str(), "apple");
        assert_eq!(SandboxBackend::Wsl.as_str(), "wsl");
        assert_eq!(SandboxBackend::PodmanRootful.as_str(), "podman-rootful");

        // Network / OnMissing / RemoteTransport / RemoteMode.
        for (s, v) in [
            ("nat", Network::Nat),
            ("host", Network::Host),
            ("none", Network::None),
        ] {
            assert_eq!(Network::from_str_validated(s).unwrap(), v);
            assert_eq!(v.as_str(), s);
        }
        for (s, v) in [
            ("warn", OnMissing::Warn),
            ("prompt", OnMissing::Prompt),
            ("fail", OnMissing::Fail),
        ] {
            assert_eq!(OnMissing::from_str_validated(s).unwrap(), v);
            assert_eq!(v.as_str(), s);
        }
        assert_eq!(
            RemoteTransport::from_str_validated("ssh").unwrap(),
            RemoteTransport::Ssh
        );
        assert_eq!(RemoteTransport::Mosh.as_str(), "mosh");
        for (s, v) in [
            ("remote", RemoteMode::Remote),
            ("local_exec", RemoteMode::LocalExec),
            ("sshfs", RemoteMode::Sshfs),
        ] {
            assert_eq!(RemoteMode::from_str_validated(s).unwrap(), v);
            assert_eq!(v.as_str(), s);
        }

        // LogLevel / LogFormat full sets.
        for (s, v) in [
            ("error", LogLevel::Error),
            ("warn", LogLevel::Warn),
            ("info", LogLevel::Info),
            ("debug", LogLevel::Debug),
            ("trace", LogLevel::Trace),
        ] {
            assert_eq!(LogLevel::from_str_validated(s).unwrap(), v);
            assert_eq!(v.as_str(), s);
        }
        assert_eq!(
            LogFormat::from_str_validated("json").unwrap(),
            LogFormat::Json
        );
        assert_eq!(LogFormat::Text.as_str(), "text");

        // UndercurlMode, WorktreeMode, NameScheme, GitCmdOutput, IssueProviderKind.
        for (s, v) in [
            ("auto", UndercurlMode::Auto),
            ("on", UndercurlMode::On),
            ("off", UndercurlMode::Off),
        ] {
            assert_eq!(UndercurlMode::from_str_validated(s).unwrap(), v);
            assert_eq!(v.as_str(), s);
        }
        assert_eq!(
            WorktreeMode::from_str_validated("global").unwrap(),
            WorktreeMode::Global
        );
        assert_eq!(
            NameScheme::from_str_validated("words").unwrap(),
            NameScheme::Words
        );
        for (s, v) in [
            ("none", GitCmdOutput::None),
            ("popup", GitCmdOutput::Popup),
            ("terminal", GitCmdOutput::Terminal),
        ] {
            assert_eq!(GitCmdOutput::from_str_validated(s).unwrap(), v);
            assert_eq!(v.as_str(), s);
        }
        for (s, v) in [
            ("none", IssueProviderKind::None),
            ("linear", IssueProviderKind::Linear),
            ("github", IssueProviderKind::Github),
            ("jira", IssueProviderKind::Jira),
        ] {
            assert_eq!(IssueProviderKind::from_str_validated(s).unwrap(), v);
            assert_eq!(v.as_str(), s);
        }

        // Error messages mention the kind label and the bad value.
        let e = Network::from_str_validated("bogus").unwrap_err();
        assert!(e.contains("sandbox network") && e.contains("bogus"), "{e}");
    }

    #[test]
    fn config_enum_parsing_is_case_and_whitespace_insensitive() {
        assert_eq!(Picker::from_str_validated("  GUM ").unwrap(), Picker::Gum);
        assert_eq!(
            SandboxBackend::from_str_validated("DOCKER").unwrap(),
            SandboxBackend::Docker
        );
    }

    #[test]
    fn pin_scope_aliases_parse() {
        for (s, v) in [
            ("global", PinScope::Global),
            ("everywhere", PinScope::Global),
            ("all", PinScope::Global),
            ("workspace", PinScope::Workspace),
            ("local", PinScope::Workspace),
        ] {
            assert_eq!(PinScope::from_str_validated(s).unwrap(), v, "{s}");
        }
        assert_eq!(PinScope::Global.as_str(), "global");
        assert_eq!(PinScope::Workspace.as_str(), "workspace");
    }

    #[test]
    fn pin_location_aliases_parse() {
        for (s, v) in [
            ("tab", PinLocation::Tab),
            ("layout", PinLocation::Layout),
            ("pane", PinLocation::Layout),
            ("active_layout", PinLocation::Layout),
            ("active-layout", PinLocation::Layout),
            ("strip", PinLocation::Strip),
            ("top", PinLocation::Strip),
            ("top-strip", PinLocation::Strip),
            ("top_strip", PinLocation::Strip),
            ("float", PinLocation::Float),
            ("floating", PinLocation::Float),
            ("scratch", PinLocation::Float),
        ] {
            assert_eq!(PinLocation::from_str_validated(s).unwrap(), v, "{s}");
        }
    }

    #[test]
    fn compression_level_aliases_and_serde() {
        assert_eq!(
            CompressionLevel::from_str_validated("none").unwrap(),
            CompressionLevel::Off
        );
        assert_eq!(CompressionLevel::Off.as_str(), "off");
        assert_eq!(
            CompressionLevel::from_str_validated("balanced").unwrap(),
            CompressionLevel::Balanced
        );
        // Unknown deserializes to default (Conservative) without panic.
        let c: CompressionLevel = serde_json::from_str(r#""nonsense""#).unwrap();
        assert_eq!(c, CompressionLevel::Conservative);
        // Serialize round-trips to canonical.
        assert_eq!(
            serde_json::to_string(&CompressionLevel::Aggressive).unwrap(),
            r#""aggressive""#
        );
    }

    // ---- Default impls (non-trivial fields) ----

    #[test]
    fn section_defaults_match_documented_values() {
        assert_eq!(PrConfig::default().ttl_secs, 30);
        assert_eq!(WatchConfig::default().pr_interval_secs, 20);

        let a = AppsConfig::default();
        assert_eq!(a.default_tab, "work");
        assert_eq!(a.tab_order, vec!["work"]);

        let n = NotificationsConfig::default();
        assert!(n.desktop);
        assert_eq!(n.desktop_min_urgency, "normal");
        assert_eq!(n.process_exit, "failures_and_tasks");

        let s = SearchConfig::default();
        assert_eq!(s.history_lines, 10_000);
        assert_eq!(s.max_results, 1_000);

        let l = LspConfig::default();
        assert!(l.enabled);
        assert!(l.hover);
        assert!(l.servers.is_empty());

        let p = PaletteConfig::default();
        assert_eq!(p.content_max_results, 500);
        assert_eq!(p.file_max_results, 200);
        assert_eq!(p.symbol_max_results, 100);
        assert!(!p.content_search_hidden);

        assert!(PanelConfig::default().sections.is_empty());
        assert!(!GitConfig::default().override_gpg);
    }

    #[test]
    fn media_config_defaults_and_enums() {
        let m = MediaConfig::default();
        assert!(
            m.enabled,
            "media defaults ON (additive; inert without a backend)"
        );
        assert_eq!(
            m.backend,
            MediaBackendKind::Auto,
            "media defaults to per-OS auto resolution"
        );
        assert!(m.players_priority.is_empty());
        assert_eq!(m.default_action, MediaDefaultAction::PlayPause);
        assert_eq!(m.volume_step, 0.05);
        assert_eq!(m.poll_interval_secs, 3);
        assert_eq!(m.mpv.socket, "/tmp/mpvsocket");

        // Aliases parse; an unknown value falls back to the default (infallible).
        assert_eq!(
            MediaBackendKind::from_str_validated("dbus").unwrap(),
            MediaBackendKind::Mpris
        );
        assert_eq!(
            MediaBackendKind::from_str_validated("off").unwrap(),
            MediaBackendKind::None
        );
        assert!(MediaBackendKind::from_str_validated("winamp").is_err());
        assert_eq!(MediaBackendKind::Mpris.as_str(), "mpris");
        // New cross-platform backends parse (canon + aliases).
        assert_eq!(
            MediaBackendKind::from_str_validated("auto").unwrap(),
            MediaBackendKind::Auto
        );
        assert_eq!(
            MediaBackendKind::from_str_validated("windows").unwrap(),
            MediaBackendKind::Smtc
        );
        assert_eq!(
            MediaBackendKind::from_str_validated("osascript").unwrap(),
            MediaBackendKind::AppleScript
        );

        // Default Config keeps media enabled and round-trips through TOML.
        assert!(Config::default().media.enabled);
        let toml = toml::to_string(&MediaConfig::default()).unwrap();
        let back: MediaConfig = toml::from_str(&toml).unwrap();
        assert_eq!(back.backend, MediaBackendKind::Auto);
    }

    #[test]
    fn bars_config_defaults() {
        let b = BarsConfig::default();
        assert_eq!(b.top_left, vec!["brand"]);
        assert_eq!(
            b.top_right,
            vec![
                "cpu", "mem", "disk", "gpu", "temp", "net", "battery", "date", "clock"
            ]
        );
        assert_eq!(b.bottom_left, vec!["keyhints"]);
        assert_eq!(b.bottom_right, vec!["pr", "tests", "loc", "disk", "status"]);
        assert_eq!(b.date_format, "%a %b %-d");
        assert_eq!(b.clock_format, "%H:%M");
    }

    #[test]
    fn limits_config_defaults() {
        let l = LimitsConfig::default();
        assert_eq!(l.tool_mem_max, "6G");
        assert_eq!(l.tool_mem_swap_max, "1G");
        assert_eq!(l.test_cpu_quota, "150%");
        assert_eq!(l.test_mem_max, "4G");
        assert_eq!(l.test_nice, 10);
        assert_eq!(l.test_max_parallel, 1);
        assert_eq!(l.test_timeout_secs, 1800);
        assert_eq!(l.discover_timeout_secs, 45);
        assert!(l.isolated_target_dir);
    }

    #[test]
    fn log_config_default_and_theme_config_default() {
        let l = LogConfig::default();
        assert_eq!(l.level, LogLevel::Info);
        assert!(!l.file);
        assert_eq!(l.dir, "");
        assert_eq!(l.rotation_size_mb, 5);
        assert_eq!(l.max_files, 5);
        assert_eq!(l.format, LogFormat::Text);

        let t = ThemeConfig::default();
        assert_eq!(t.preset, "prism");
        assert_eq!(t.accent, "#6ee7d8");
        assert_eq!(t.focus_border, "#6ee7d8");
        assert_eq!(t.pane_padding, 0);
        assert_eq!(t.undercurl, UndercurlMode::Auto);
        assert_eq!(t.color, ColorMode::Auto);
        assert_eq!(t.glyphs, GlyphMode::Auto);
    }

    #[test]
    fn color_and_glyph_modes_parse_with_aliases() {
        assert_eq!(
            ColorMode::from_str_validated("auto").unwrap(),
            ColorMode::Auto
        );
        assert_eq!(
            ColorMode::from_str_validated("24bit").unwrap(),
            ColorMode::Truecolor
        );
        assert_eq!(
            ColorMode::from_str_validated("256").unwrap(),
            ColorMode::Ansi256
        );
        assert_eq!(
            ColorMode::from_str_validated("MONO").unwrap(),
            ColorMode::None
        );
        assert!(ColorMode::from_str_validated("16bit").is_err());

        assert_eq!(
            GlyphMode::from_str_validated("ascii").unwrap(),
            GlyphMode::Ascii
        );
        assert_eq!(
            GlyphMode::from_str_validated("unicode").unwrap(),
            GlyphMode::Unicode
        );
        assert!(GlyphMode::from_str_validated("nerd").is_err());
    }

    #[test]
    fn theme_color_glyph_env_overrides_apply() {
        let mut env = MapEnv::default();
        env.0
            .insert("SUPERZEJ_THEME_COLOR".to_string(), "16".to_string());
        env.0
            .insert("SUPERZEJ_THEME_GLYPHS".to_string(), "ascii".to_string());
        let o = env_overlay(&env);
        assert_eq!(o.theme_color, Some(ColorMode::Ansi16));
        assert_eq!(o.theme_glyphs, Some(GlyphMode::Ascii));
        let mut cfg = Config::default();
        o.apply(&mut cfg);
        assert_eq!(cfg.theme.color, ColorMode::Ansi16);
        assert_eq!(cfg.theme.glyphs, GlyphMode::Ascii);
    }

    #[test]
    fn issue_provider_subconfig_defaults() {
        let lin = LinearConfig::default();
        assert_eq!(lin.api_key, "env:LINEAR_API_KEY");
        assert_eq!(lin.team_id, "");
        assert_eq!(lin.workspace_slug, "");
        let jira = JiraConfig::default();
        assert_eq!(jira.api_token, "env:JIRA_API_TOKEN");
        assert_eq!(jira.base_url, "");
        assert_eq!(jira.email, "");
        assert_eq!(jira.project_key, "");
        assert!(GitHubIssuesConfig::default().extra_flags.is_empty());
    }

    #[test]
    fn remote_config_default_and_is_remote() {
        let r = RemoteConfig::default();
        assert_eq!(r.host, "");
        assert_eq!(r.port, 22);
        assert_eq!(r.transport, RemoteTransport::Mosh);
        assert_eq!(r.mode, RemoteMode::Remote);
        assert_eq!(r.remote_dir, "~/superzej-worktrees");
        assert!(r.forward_agent);
        assert!(!r.is_remote());
        let r2 = RemoteConfig {
            host: "  user@box ".into(),
            ..RemoteConfig::default()
        };
        assert!(r2.is_remote());
        let blank = RemoteConfig {
            host: "   ".into(),
            ..RemoteConfig::default()
        };
        assert!(!blank.is_remote());
    }

    #[test]
    fn sandbox_config_default_collections() {
        let s = SandboxConfig::default();
        assert!(s.enabled);
        assert_eq!(s.backend, SandboxBackend::Auto);
        assert_eq!(s.default_backend, SandboxBackend::Auto);
        assert_eq!(
            s.backend_chain,
            vec![
                "podman-rootless",
                "podman-rootful",
                "docker",
                "bwrap",
                "host"
            ]
        );
        assert!(s.image.is_empty());
        assert!(s.env_passthrough.contains(&"SSH_AUTH_SOCK".to_string()));
        assert!(s.env_passthrough.contains(&"GH_TOKEN".to_string()));
        assert!(s.auto_caches);
        assert!(s.mounts.contains(&"~/.gitconfig:ro".to_string()));
        assert!(!s.devenv);
        assert_eq!(s.on_missing, OnMissing::Warn);
        assert_eq!(s.file_access, FileAccess::WorktreePlusCaches);
        assert!(s.network_allow.is_empty());
        assert!(!s.network_audit);
    }

    #[test]
    fn file_access_default_and_serde() {
        assert_eq!(FileAccess::default(), FileAccess::WorktreePlusCaches);
        // snake_case rename: the default variant serializes to that string.
        assert_eq!(
            serde_json::to_string(&FileAccess::WorktreePlusCaches).unwrap(),
            r#""worktree_plus_caches""#
        );
        let f: FileAccess = serde_json::from_str(r#""host""#).unwrap();
        assert_eq!(f, FileAccess::Host);
    }

    #[test]
    fn sandbox_limits_default_and_parse() {
        let d = SandboxLimits::default();
        assert!(d.cpu.is_none() && d.memory.is_none());
        let cfg: Config =
            toml::from_str("[sandbox.limits]\ncpu = \"2\"\nmemory = \"4G\"\n").unwrap();
        assert_eq!(cfg.sandbox.limits.cpu.as_deref(), Some("2"));
        assert_eq!(cfg.sandbox.limits.memory.as_deref(), Some("4G"));
    }

    #[test]
    fn sandbox_warm_direnv_and_prepare_parse() {
        // Default: warm on, no prepare hooks.
        let d = SandboxConfig::default();
        assert_eq!(d.warm_direnv, WarmDirenv::Auto);
        assert!(d.prepare.is_empty());
        // Round-trips from a `[sandbox]` table, and the overlay layers them.
        let cfg: Config = toml::from_str(
            "[sandbox]\nwarm_direnv = \"allowed-only\"\nprepare = [\"mise install\", \"echo hi\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.sandbox.warm_direnv, WarmDirenv::AllowedOnly);
        assert_eq!(cfg.sandbox.prepare, vec!["mise install", "echo hi"]);
        // Unknown value warns and falls back to the default (infallible enum).
        let cfg2: Config = toml::from_str("[sandbox]\nwarm_direnv = \"bogus\"\n").unwrap();
        assert_eq!(cfg2.sandbox.warm_direnv, WarmDirenv::Auto);
        // `off` aliases.
        assert_eq!(
            WarmDirenv::from_str_validated("off").unwrap(),
            WarmDirenv::Off
        );
        assert_eq!(
            WarmDirenv::from_str_validated("false").unwrap(),
            WarmDirenv::Off
        );
    }

    // ---- launch_spec full env coverage ----

    #[test]
    fn llm_proxy_launch_spec_sets_all_stream_env() {
        let cfg = LlmProxyConfig {
            enabled: true,
            listen: "0.0.0.0:9000".into(),
            config_path: String::new(),
            routing: RoutingStrategy::Speculative,
            first_byte_timeout_secs: 7,
            idle_timeout_secs: 99,
            heartbeat_secs: 3,
            token_reduction: true,
            token_reduction_level: CompressionLevel::Aggressive,
            ..Default::default()
        };
        let (prog, _args, env) = cfg.launch_spec().unwrap();
        assert_eq!(prog, "szproxy");
        assert_eq!(env.get("SZPROXY_LISTEN").unwrap(), "0.0.0.0:9000");
        assert_eq!(env.get("SZPROXY_FIRST_BYTE_TIMEOUT").unwrap(), "7");
        assert_eq!(env.get("SZPROXY_STREAM_IDLE_TIMEOUT").unwrap(), "99");
        assert_eq!(env.get("SZPROXY_STREAM_HEARTBEAT_INTERVAL").unwrap(), "3");
        assert_eq!(env.get("SZPROXY_COMPRESS").unwrap(), "1");
        assert_eq!(env.get("SZPROXY_COMPRESS_LEVEL").unwrap(), "aggressive");
        assert_eq!(env.get("SZPROXY_ROUTING").unwrap(), "speculative");
        // token_reduction off → SZPROXY_COMPRESS = "0".
        let off = LlmProxyConfig {
            enabled: true,
            token_reduction: false,
            ..Default::default()
        };
        let (_, _, env) = off.launch_spec().unwrap();
        assert_eq!(env.get("SZPROXY_COMPRESS").unwrap(), "0");
    }

    // ---- AppsConfig::effective_tab_order / normalized_default_tab edges ----

    #[test]
    fn effective_tab_order_dedups_and_appends_missing() {
        let a = AppsConfig {
            // duplicates, unknown ids, and a whitespace-padded built-in.
            default_tab: "work".into(),
            tab_order: vec![
                "bogus".into(),
                "comms".into(),
                " work ".into(),
                "work".into(),
            ],
        };
        // unknown ids dropped, trimmed, deduped; the only built-in is `work`.
        assert_eq!(a.effective_tab_order(), vec!["work"]);
    }

    #[test]
    fn effective_tab_order_empty_falls_back_to_builtins() {
        let a = AppsConfig {
            default_tab: "work".into(),
            tab_order: Vec::new(),
        };
        assert_eq!(a.effective_tab_order(), vec!["work"]);
    }

    #[test]
    fn normalized_default_tab_present_and_falls_back_to_first() {
        let present = AppsConfig {
            default_tab: " work ".into(),
            tab_order: vec!["work".into()],
        };
        assert_eq!(present.normalized_default_tab(), "work");
        // Unknown default → first of the effective order (`work`).
        let bad = AppsConfig {
            default_tab: "nonexistent".into(),
            tab_order: vec!["comms".into(), "work".into()],
        };
        assert_eq!(bad.normalized_default_tab(), "work");
    }

    // ---- ConfigOverlay::apply field-by-field ----

    #[test]
    fn config_overlay_apply_sets_every_field() {
        let overlay = ConfigOverlay {
            worktrees_dir: Some("/wt".into()),
            workspaces_dir: Some("/ws".into()),
            base_branch: Some("main".into()),
            window_margin: Some(1),
            branch_prefix: Some("pfx/".into()),
            picker: Some(Picker::Fzf),
            worktree_mode: Some(WorktreeMode::InRepo),
            name_scheme: Some(NameScheme::Numbered),
            auto_remove_worktree: Some(true),
            repo_scan_depth: Some(7),
            profile: Some("vim".into()),
            accent: Some("#111111".into()),
            focus_border: Some("#222222".into()),
            frame_border: Some("#333333".into()),
            theme_color: Some(ColorMode::Ansi256),
            theme_glyphs: Some(GlyphMode::Unicode),
            pr_ttl_secs: Some(99),
            watch_pr_interval_secs: Some(43),
            metrics_interval_secs: Some(11.0),
            metrics_timeout_ms: Some(1234),
            metrics_max_body_bytes: Some(4096),
            apps_default_tab: Some("chat".into()),
            apps_tab_order: Some(vec!["chat".into(), "work".into()]),
            log_level: Some(LogLevel::Debug),
            log_file: Some(true),
            log_dir: Some("/logs".into()),
            log_rotation_size_mb: Some(12),
            log_max_files: Some(3),
            log_format: Some(LogFormat::Json),
            disk_show_sizes: Some(false),
            disk_warn_threshold_gb: Some(250),
            disk_scan_interval_secs: Some(90),
            disk_auto_clean_on_merge: Some(false),
            disk_clean_on_pr_closed: Some(true),
            disk_sccache: Some(true),
            disk_sccache_dir: Some("/cache/sccache".into()),
            disk_shared_target_dir: Some("/cache/target".into()),
            sandbox: SandboxOverlay {
                enabled: Some(false),
                ..Default::default()
            },
        };
        let mut cfg = Config::default();
        overlay.apply(&mut cfg);
        assert_eq!(cfg.worktrees_dir, "/wt");
        assert_eq!(cfg.workspaces_dir, "/ws");
        assert_eq!(cfg.base_branch, "main");
        assert_eq!(cfg.window_margin, 1);
        assert_eq!(cfg.branch_prefix, "pfx/");
        assert_eq!(cfg.picker, Picker::Fzf);
        assert_eq!(cfg.worktree_mode, WorktreeMode::InRepo);
        assert_eq!(cfg.name_scheme, NameScheme::Numbered);
        assert!(cfg.auto_remove_worktree);
        assert_eq!(cfg.repo_scan_depth, 7);
        assert_eq!(cfg.profile, "vim");
        assert_eq!(cfg.theme.accent, "#111111");
        assert_eq!(cfg.theme.focus_border, "#222222");
        assert_eq!(cfg.theme.colors.border.as_deref(), Some("#333333"));
        assert_eq!(cfg.pr.ttl_secs, 99);
        assert_eq!(cfg.watch.pr_interval_secs, 43);
        assert_eq!(cfg.metrics.interval_secs, 11.0);
        assert_eq!(cfg.metrics.timeout_ms, 1234);
        assert_eq!(cfg.metrics.max_body_bytes, 4096);
        assert_eq!(cfg.apps.default_tab, "chat");
        assert_eq!(cfg.apps.tab_order, vec!["chat", "work"]);
        assert_eq!(cfg.log.level, LogLevel::Debug);
        assert!(cfg.log.file);
        assert_eq!(cfg.log.dir, "/logs");
        assert_eq!(cfg.log.rotation_size_mb, 12);
        assert_eq!(cfg.log.max_files, 3);
        assert_eq!(cfg.log.format, LogFormat::Json);
        assert!(!cfg.disk.show_sizes);
        assert_eq!(cfg.disk.warn_threshold_gb, 250);
        assert_eq!(cfg.disk.scan_interval_secs, 90);
        assert!(!cfg.disk.auto_clean_on_merge);
        assert!(cfg.disk.clean_on_pr_closed);
        assert!(cfg.disk.sccache);
        assert_eq!(cfg.disk.sccache_dir, "/cache/sccache");
        assert_eq!(cfg.disk.shared_target_dir, "/cache/target");
        assert!(!cfg.sandbox.enabled);
    }

    #[test]
    fn config_overlay_empty_leaves_base_untouched() {
        let mut cfg = Config::default();
        let before = cfg.clone();
        ConfigOverlay::default().apply(&mut cfg);
        // Spot-check a few fields are unchanged.
        assert_eq!(cfg.branch_prefix, before.branch_prefix);
        assert_eq!(cfg.picker, before.picker);
        assert_eq!(cfg.sandbox.enabled, before.sandbox.enabled);
        // An empty sandbox overlay must not be applied.
        assert!(ConfigOverlay::default().sandbox.is_empty());
    }

    // ---- SandboxOverlay::apply remaining branches + is_empty ----

    #[test]
    fn sandbox_overlay_apply_covers_remaining_fields() {
        let mut base = SandboxConfig::default();
        let overlay = SandboxOverlay {
            default_backend: Some(SandboxBackend::Docker),
            file_access: Some(FileAccess::All),
            ports: Some(vec!["8080:8080".into()]),
            gpu: Some("all".into()),
            limits: Some(SandboxLimits {
                cpu: Some("2".into()),
                memory: Some("8G".into()),
            }),
            volumes: Some(std::collections::HashMap::from([(
                "vol".to_string(),
                "/data".to_string(),
            )])),
            compose: Some("docker-compose.yml".into()),
            auto_caches: Some(false),
            shell: Some("zsh".into()),
            network_audit: Some(true),
            ..Default::default()
        };
        overlay.apply(&mut base);
        assert_eq!(base.default_backend, SandboxBackend::Docker);
        assert_eq!(base.file_access, FileAccess::All);
        assert_eq!(base.ports, vec!["8080:8080"]);
        assert_eq!(base.gpu.as_deref(), Some("all"));
        assert_eq!(base.limits.cpu.as_deref(), Some("2"));
        assert_eq!(base.limits.memory.as_deref(), Some("8G"));
        assert_eq!(base.volumes.get("vol").map(String::as_str), Some("/data"));
        assert_eq!(base.compose.as_deref(), Some("docker-compose.yml"));
        assert!(!base.auto_caches);
        assert_eq!(base.shell, "zsh");
        assert!(base.network_audit);
    }

    #[test]
    fn sandbox_overlay_is_empty_detects_any_set_field() {
        assert!(SandboxOverlay::default().is_empty());
        // Each is_empty()-tracked field flips it to non-empty.
        let with_allow = SandboxOverlay {
            network_allow: Some(vec!["x".into()]),
            ..Default::default()
        };
        assert!(!with_allow.is_empty());
        let with_remote = SandboxOverlay {
            remote: Some(RemoteOverlay::default()),
            ..Default::default()
        };
        assert!(!with_remote.is_empty());
        let with_backend = SandboxOverlay {
            backend: Some(SandboxBackend::Docker),
            ..Default::default()
        };
        assert!(!with_backend.is_empty());
    }

    #[test]
    fn remote_overlay_apply_sets_each_field() {
        let mut base = RemoteConfig::default();
        RemoteOverlay {
            host: Some("h".into()),
            port: Some(2022),
            transport: Some(RemoteTransport::Ssh),
            mode: Some(RemoteMode::LocalExec),
            remote_dir: Some("/srv".into()),
            forward_agent: Some(false),
        }
        .apply(&mut base);
        assert_eq!(base.host, "h");
        assert_eq!(base.port, 2022);
        assert_eq!(base.transport, RemoteTransport::Ssh);
        assert_eq!(base.mode, RemoteMode::LocalExec);
        assert_eq!(base.remote_dir, "/srv");
        assert!(!base.forward_agent);
    }

    // ---- env_overlay: remaining branches ----

    #[test]
    fn env_overlay_apps_tab_order_parses_csv() {
        let env = map_env(&[("SUPERZEJ_APPS_TAB_ORDER", " work , foo ,, bar ")]);
        let o = env_overlay(&env);
        // parse_list trims and drops empties (validity filtering happens later
        // in effective_tab_order).
        assert_eq!(
            o.apps_tab_order,
            Some(vec![
                "work".to_string(),
                "foo".to_string(),
                "bar".to_string()
            ])
        );
    }

    #[test]
    fn env_overlay_metrics_rejects_non_finite_floats() {
        let env = map_env(&[("SUPERZEJ_METRICS_INTERVAL_SECS", "inf")]);
        assert_eq!(env_overlay(&env).metrics_interval_secs, None);
        let env = map_env(&[("SUPERZEJ_METRICS_INTERVAL_SECS", "abc")]);
        assert_eq!(env_overlay(&env).metrics_interval_secs, None);
    }

    #[test]
    fn env_overlay_bad_enum_values_yield_none() {
        let env = map_env(&[
            ("SUPERZEJ_WORKTREE_MODE", "bogus"),
            ("SUPERZEJ_NAME_SCHEME", "bogus"),
            ("SUPERZEJ_LOG_LEVEL", "bogus"),
            ("SUPERZEJ_LOG_FORMAT", "bogus"),
            ("SUPERZEJ_SANDBOX_BACKEND", "bogus"),
            ("SUPERZEJ_SANDBOX_NETWORK", "bogus"),
            ("SUPERZEJ_SANDBOX_ON_MISSING", "bogus"),
        ]);
        let o = env_overlay(&env);
        assert_eq!(o.worktree_mode, None);
        assert_eq!(o.name_scheme, None);
        assert_eq!(o.log_level, None);
        assert_eq!(o.log_format, None);
        assert_eq!(o.sandbox.backend, None);
        assert_eq!(o.sandbox.network, None);
        assert_eq!(o.sandbox.on_missing, None);
    }

    #[test]
    fn env_overlay_log_and_metrics_bad_numbers_skip() {
        let env = map_env(&[
            ("SUPERZEJ_LOG_ROTATION_SIZE_MB", "huge"),
            ("SUPERZEJ_LOG_MAX_FILES", "lots"),
            ("SUPERZEJ_METRICS_TIMEOUT_MS", "soon"),
            ("SUPERZEJ_METRICS_MAX_BODY_BYTES", "big"),
            ("SUPERZEJ_WATCH_PR_INTERVAL", "later"),
        ]);
        let o = env_overlay(&env);
        assert_eq!(o.log_rotation_size_mb, None);
        assert_eq!(o.log_max_files, None);
        assert_eq!(o.metrics_timeout_ms, None);
        assert_eq!(o.metrics_max_body_bytes, None);
        assert_eq!(o.watch_pr_interval_secs, None);
    }

    // ---- get_dotted: theme.preset/pane_padding/undercurl/hues + log.dir ----

    #[test]
    fn get_dotted_theme_preset_padding_undercurl_and_hues() {
        let mut c = Config::default();
        c.theme.preset = "storm".into();
        c.theme.pane_padding = 3;
        c.theme.undercurl = UndercurlMode::On;
        c.theme.hues.teal = Some("#0a0b0c".into());
        assert_eq!(c.get_dotted("theme.preset").as_deref(), Some("storm"));
        assert_eq!(c.get_dotted("theme.pane_padding").as_deref(), Some("3"));
        assert_eq!(c.get_dotted("theme.undercurl").as_deref(), Some("on"));
        assert_eq!(c.get_dotted("theme.hues.teal").as_deref(), Some("#0a0b0c"));
        // Unset hue → empty string; unknown hue → None.
        assert_eq!(c.get_dotted("theme.hues.red").as_deref(), Some(""));
        assert_eq!(c.get_dotted("theme.hues.bogus"), None);
        // confirm_delete + remote sub-keys.
        assert_eq!(c.get_dotted("confirm_delete").as_deref(), Some("true"));
        assert_eq!(
            c.get_dotted("sandbox.remote.transport").as_deref(),
            Some("mosh")
        );
        assert_eq!(
            c.get_dotted("sandbox.remote.mode").as_deref(),
            Some("remote")
        );
        // log.dir resolves to the default path (no tilde).
        let dir = c.get_dotted("log.dir").unwrap();
        assert!(dir.ends_with("superzej/logs"), "{dir}");
    }

    // ---- post_process behavior ----

    #[test]
    fn post_process_populates_default_agents_and_tools() {
        // Point at a non-existent file so the file layer is empty and defaults
        // (not the host's real ~/.config) drive post_process.
        let dir = tmpdir("ppdefaults");
        let f = dir.join("missing.toml");
        let c = Config::load_layered(&MapEnv::default(), &[], Some(f));
        assert!(c.agents.iter().any(|a| a.name == "claude"));
        assert!(c.agents.iter().any(|a| a.name == "shell"));
        assert!(c.tools.iter().any(|t| t.name == "lazygit"));
        assert!(c.tools.iter().any(|t| t.name == "editor"));
        // repo_roots defaults to [workspaces_dir] when unset.
        assert_eq!(c.repo_roots, vec![c.workspaces_dir.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_process_expands_pin_cwd_tilde() {
        let dir = tmpdir("ppcwd");
        let f = dir.join("c.toml");
        std::fs::write(&f, "[[pins]]\nname='x'\ncommand='c'\ncwd='~/sub'\n").unwrap();
        let c = Config::load_layered(&MapEnv::default(), &[], Some(f));
        let cwd = c.pins[0].cwd.as_deref().unwrap();
        assert!(!cwd.starts_with('~'), "{cwd}");
        assert!(cwd.ends_with("/sub"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- apply_override_str: apps.tab_order shortcut + load_layered error path ----

    #[test]
    fn apply_override_str_apps_tab_order_splits_csv() {
        let mut cfg = Config::default();
        Config::apply_override_str(&mut cfg, "apps.tab_order", " work , foo ,, bar ").unwrap();
        assert_eq!(cfg.apps.tab_order, vec!["work", "foo", "bar"]);
    }

    #[test]
    fn load_layered_recovers_on_parse_error_and_still_applies_layers() {
        // A malformed file forces the load_layered error branch, which rebuilds
        // from defaults and re-applies env + flags.
        let dir = tmpdir("recover");
        let f = dir.join("c.toml");
        std::fs::write(&f, "= = broken\n").unwrap();
        let env = map_env(&[("SUPERZEJ_BRANCH_PREFIX", "env/")]);
        let flags = vec!["picker=fzf".to_string()];
        let c = Config::load_layered(&env, &flags, Some(f));
        assert_eq!(c.branch_prefix, "env/");
        assert_eq!(c.picker, Picker::Fzf);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remote_agent_env_routes_through_proxy_when_configured() {
        // Off by default (no route_agent → no injection).
        assert!(LlmProxyConfig::default().remote_agent_env(None).is_empty());
        // `route_agent` alone is the single switch: an empty remote_base_url resolves
        // to the auto reverse-tunnel loopback, so the pi (`SUPERZEJ_PROXY_*`) env IS
        // injected and the tunnel port is signalled — but NOT `ANTHROPIC_BASE_URL`
        // (claude talks to Anthropic directly unless `route_claude`).
        let only_route = LlmProxyConfig {
            route_agent: true,
            ..Default::default()
        };
        let oenv = only_route.remote_agent_env(None);
        assert!(
            oenv.iter()
                .any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL" && v == "http://127.0.0.1:8383"),
            "route_agent alone → pi proxy vars injected at the auto loopback"
        );
        assert!(
            !oenv.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"),
            "claude is NOT routed by default (route_claude off)"
        );
        assert_eq!(only_route.remote_tunnel_port(), Some(8383));
        // Configured, route_claude ON → additionally inject the ANTHROPIC_* vars.
        let lp = LlmProxyConfig {
            route_agent: true,
            route_claude: true,
            remote_base_url: "https://proxy.example".into(),
            ..Default::default()
        };
        let env = lp.remote_agent_env(Some("vk-1"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "https://proxy.example"),
            "route_claude → claude code / SDK honor ANTHROPIC_BASE_URL"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v == "vk-1")
        );
        assert!(env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_BASE_URL"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "SUPERZEJ_PROXY_KEY" && v == "vk-1"),
            "pi always gets the virtual key regardless of route_claude"
        );
        assert_eq!(
            lp.remote_tunnel_port(),
            None,
            "explicit URL needs no tunnel"
        );
        // route_claude OFF with a virtual key: pi key present, ANTHROPIC_* absent.
        let keyed_no_claude = LlmProxyConfig {
            route_agent: true,
            remote_base_url: "https://proxy.example".into(),
            ..Default::default()
        };
        let kenv = keyed_no_claude.remote_agent_env(Some("vk-2"));
        assert!(
            kenv.iter()
                .any(|(k, v)| k == "SUPERZEJ_PROXY_KEY" && v == "vk-2")
        );
        assert!(!kenv.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
        assert!(!kenv.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));

        // "auto" → derive the in-sandbox tunnel URL from the proxy port + signal
        // the host to stand a reverse tunnel up on that port (pi still needs it).
        let auto = LlmProxyConfig {
            route_agent: true,
            route_claude: true,
            remote_base_url: "auto".into(),
            listen: "127.0.0.1:9999".into(),
            ..Default::default()
        };
        assert_eq!(
            auto.remote_base_url().as_deref(),
            Some("http://127.0.0.1:9999")
        );
        assert_eq!(auto.remote_tunnel_port(), Some(9999));
        let aenv = auto.remote_agent_env(None);
        assert!(
            aenv.iter()
                .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == "http://127.0.0.1:9999")
        );
    }

    #[test]
    fn local_agent_env_targets_host_loopback_regardless_of_remote_base_url() {
        // Off by default.
        assert!(LlmProxyConfig::default().local_agent_env().is_empty());
        // `route_agent` on: always the LOCAL listen loopback, even when
        // `remote_base_url` points at an external endpoint for remote sandboxes.
        let lp = LlmProxyConfig {
            route_agent: true,
            route_claude: true,
            remote_base_url: "https://proxy.example.ts.net".into(),
            listen: "127.0.0.1:8383".into(),
            ..Default::default()
        };
        let env = lp.local_agent_env();
        let url = "http://127.0.0.1:8383";
        assert!(
            env.iter()
                .any(|(k, v)| k == "ANTHROPIC_BASE_URL" && v == url),
            "route_claude → host agent uses the local loopback, not the external remote URL"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL" && v == url),
            "the pi extension's base URL is the local proxy"
        );
        assert!(env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_API"));
        assert!(env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_MODEL"));
        // Keyless (like the sprite path) — the pi extension falls back to default.
        assert!(!env.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_KEY"));
        assert!(!env.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
        // Default (route_claude off): pi vars only, claude talks upstream directly.
        let no_claude = LlmProxyConfig {
            route_agent: true,
            listen: "127.0.0.1:8383".into(),
            ..Default::default()
        };
        let nenv = no_claude.local_agent_env();
        assert!(nenv.iter().any(|(k, _)| k == "SUPERZEJ_PROXY_BASE_URL"));
        assert!(
            !nenv.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"),
            "claude not routed on the host by default"
        );
        // Honors a custom listen port.
        let custom = LlmProxyConfig {
            route_agent: true,
            listen: "127.0.0.1:7000".into(),
            ..Default::default()
        };
        assert!(
            custom
                .local_agent_env()
                .iter()
                .any(|(k, v)| k == "SUPERZEJ_PROXY_BASE_URL" && v == "http://127.0.0.1:7000")
        );
    }

    #[test]
    fn passthrough_env_remote_drops_host_local_socket_vars() {
        let sb = SandboxConfig {
            env_passthrough: vec!["SZ_TEST_TOK_42".into(), "SSH_AUTH_SOCK".into()],
            ..Default::default()
        };
        // SAFETY: test-local env mutation with a unique key; restored below.
        unsafe {
            std::env::set_var("SZ_TEST_TOK_42", "secret");
            std::env::set_var("SSH_AUTH_SOCK", "/tmp/agent.sock");
        }
        let remote = sb.passthrough_env_remote();
        assert!(
            remote.iter().any(|(k, _)| k == "SZ_TEST_TOK_42"),
            "value secrets pass to a remote placement"
        );
        assert!(
            !remote.iter().any(|(k, _)| k == "SSH_AUTH_SOCK"),
            "host-local socket vars are dropped for a remote placement"
        );
        // The unfiltered passthrough still carries it (OCI bind-mount case).
        assert!(
            sb.passthrough_env()
                .iter()
                .any(|(k, _)| k == "SSH_AUTH_SOCK")
        );
        unsafe {
            std::env::remove_var("SZ_TEST_TOK_42");
            std::env::remove_var("SSH_AUTH_SOCK");
        }
    }

    #[test]
    fn remote_safe_term_downgrades_exotic_terminals() {
        // Exotic types the remote won't have terminfo for → xterm-256color.
        assert_eq!(remote_safe_term("xterm-ghostty"), "xterm-256color");
        assert_eq!(remote_safe_term("xterm-kitty"), "xterm-256color");
        assert_eq!(remote_safe_term("alacritty"), "xterm-256color");
        assert_eq!(remote_safe_term(""), "xterm-256color");
        // Universally-shipped types pass through unchanged.
        assert_eq!(remote_safe_term("xterm-256color"), "xterm-256color");
        assert_eq!(remote_safe_term("screen-256color"), "screen-256color");
        assert_eq!(remote_safe_term("vt100"), "vt100");
    }

    #[test]
    fn passthrough_env_remote_injects_devshell_selector() {
        // Unset → no SUPERZEJ_DEVSHELL (host default shell unchanged).
        let plain = SandboxConfig::default();
        assert!(
            !plain
                .passthrough_env_remote()
                .iter()
                .any(|(k, _)| k == "SUPERZEJ_DEVSHELL"),
            "no selector when [sandbox] devshell is unset"
        );
        // Set → exported so the sandbox `.envrc` enters that attr.
        let sb = SandboxConfig {
            devshell: "sandbox".into(),
            ..Default::default()
        };
        assert!(
            sb.passthrough_env_remote()
                .iter()
                .any(|(k, v)| k == "SUPERZEJ_DEVSHELL" && v == "sandbox"),
            "devshell attr exported as SUPERZEJ_DEVSHELL"
        );
    }

    #[test]
    fn passthrough_env_remote_normalizes_term() {
        let sb = SandboxConfig {
            env_passthrough: vec!["TERM".into()],
            ..Default::default()
        };
        // SAFETY: test-local env mutation; restored below.
        unsafe {
            std::env::set_var("TERM", "xterm-ghostty");
        }
        let remote = sb.passthrough_env_remote();
        assert!(
            remote
                .iter()
                .any(|(k, v)| k == "TERM" && v == "xterm-256color"),
            "exotic host TERM normalized for the remote: {remote:?}"
        );
        unsafe {
            std::env::remove_var("TERM");
        }
    }

    #[test]
    fn env_failover_resolves_override_then_global() {
        let dir = tmpdir("failover");
        let mut cfg = Config::default();
        // Global default is opt-out (halt+warn).
        assert!(!cfg.env_failover(&dir, "default"));
        // An env with no override inherits the (repo-overlaid) global.
        cfg.sandbox.failover = true;
        cfg.env.insert("inherit".into(), EnvConfig::default());
        assert!(cfg.env_failover(&dir, "inherit"));
        // Some(false) forces a halt even when the global allows failover.
        cfg.env.insert(
            "strict".into(),
            EnvConfig {
                failover: Some(false),
                ..Default::default()
            },
        );
        assert!(!cfg.env_failover(&dir, "strict"));
        // Some(true) allows failover even when the global forbids it.
        cfg.sandbox.failover = false;
        cfg.env.insert(
            "loose".into(),
            EnvConfig {
                failover: Some(true),
                ..Default::default()
            },
        );
        assert!(cfg.env_failover(&dir, "loose"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- repo overlay parse-error and yaml/json error paths ----

    #[test]
    fn repo_overlay_malformed_file_is_ignored() {
        let dir = tmpdir("badoverlay");
        std::fs::write(dir.join(".superzej.toml"), "[sandbox\nbroken = =\n").unwrap();
        let cfg = Config::default();
        // Malformed overlay warns and is ignored → global sandbox survives.
        let sb = cfg.repo_sandbox(&dir);
        assert!(sb.enabled);
        assert_eq!(sb.image, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn repo_overlay_parse_error_surfaces_dropped_env_selection() {
        let dir = tmpdir("parseerr");
        // The real-world footgun: `env = "sprites"` (selector string) colliding
        // with an `[env.sprites.provider]` table — fails to parse, and the dropped
        // overlay would silently lose the sprites selection.
        std::fs::write(
            dir.join(".superzej.toml"),
            "env = \"sprites\"\n[env.sprites.provider]\nconnect = \"ssh\"\n",
        )
        .unwrap();
        let pe = repo_overlay_parse_error(&dir).expect("a present, malformed overlay");
        assert!(!pe.error.is_empty());
        assert_eq!(pe.selected_env, "sprites", "lenient env selector recovered");
        // A clean overlay yields None.
        std::fs::write(dir.join(".superzej.toml"), "env = \"sprites\"\n").unwrap();
        assert!(
            repo_overlay_parse_error(&dir).is_none(),
            "valid overlay parses"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lenient_env_selector_reads_toml_and_yaml_not_tables() {
        assert_eq!(lenient_env_selector("env = \"sprites\"\n"), "sprites");
        assert_eq!(lenient_env_selector("env: bigbox\n"), "bigbox");
        // Must not match table headers or sibling keys.
        assert_eq!(
            lenient_env_selector("[env.sprites]\nprovider = \"sprites\"\n"),
            ""
        );
        assert_eq!(
            lenient_env_selector("env_name = \"x\"\nenvironment = \"y\"\n"),
            ""
        );
    }

    #[test]
    fn repo_overlay_json_format_loads() {
        let dir = tmpdir("jsonoverlay");
        std::fs::write(
            dir.join(".superzej.json"),
            r#"{"sandbox":{"backend":"docker","ports":["1:1"],"file_access":"all"}}"#,
        )
        .unwrap();
        let sb = Config::default().repo_sandbox(&dir);
        assert_eq!(sb.backend, SandboxBackend::Docker);
        assert_eq!(sb.ports, vec!["1:1"]);
        assert_eq!(sb.file_access, FileAccess::All);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keybind_config_iter_and_into_iter_and_get() {
        let mut kb = KeybindConfig::default();
        assert!(kb.is_empty());
        assert!(kb.insert("a".into(), "X".into()).is_none());
        // insert returns the previous value when replacing.
        assert_eq!(kb.insert("a".into(), "Y".into()).as_deref(), Some("X"));
        assert_eq!(kb.get("a").map(String::as_str), Some("Y"));
        assert!(!kb.is_empty());
        let collected: Vec<_> = kb.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(collected, vec![("a".to_string(), "Y".to_string())]);
        // IntoIterator for &KeybindConfig.
        let via_ref: Vec<_> = (&kb).into_iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(via_ref, vec!["a".to_string()]);
    }

    #[test]
    fn config_path_uses_xdg_config_home() {
        assert!(Config::path().ends_with("superzej/config.toml"));
    }

    #[test]
    fn validate_str_non_table_top_level_is_tolerated() {
        // A document whose root toml::Value is not a table hits the
        // `as_table() == None` early-return (empty error list, no panic). A bare
        // scalar isn't valid top-level TOML, so wrap it in an array value via a
        // key to keep parsing while still exercising the guard indirectly.
        // (An empty document is the simplest non-erroring non-keyed input.)
        assert!(validate_str("   ").is_empty());
        assert!(validate_str("\n# comment only\n").is_empty());
    }

    #[test]
    fn lsp_servers_and_actions_and_custom_actions_parse() {
        let cfg: Config = toml::from_str(
            r#"
[lsp]
enabled = false
hover = false
[[lsp.servers]]
lang = "rust"
command = "rust-analyzer"
args = ["--stdio"]

[[actions]]
name = "open-logs"
key = "Alt L"
run = "journalctl -f"
menu = true
hint = "logs"

[[actions]]
name = "bare"
key = "Alt B"
run = "echo hi"
"#,
        )
        .unwrap();
        assert!(!cfg.lsp.enabled);
        assert!(!cfg.lsp.hover);
        assert_eq!(cfg.lsp.servers[0].lang, "rust");
        assert_eq!(cfg.lsp.servers[0].args, vec!["--stdio"]);
        assert_eq!(cfg.actions.len(), 2);
        let a = &cfg.actions[0];
        assert!(a.menu);
        assert_eq!(a.hint.as_deref(), Some("logs"));
        assert!(a.floating); // default_true
        assert!(a.close_on_exit); // default_true
        // bare action keeps menu=false and the default_true flags.
        let b = &cfg.actions[1];
        assert!(!b.menu);
        assert!(b.floating && b.close_on_exit);
        // run-form leaves the composite fields empty.
        assert_eq!(a.run.as_deref(), Some("journalctl -f"));
        assert!(a.action.is_none() && a.params.is_empty());
    }

    #[test]
    fn composite_action_with_params_parses() {
        let cfg: Config = toml::from_str(
            r#"
[[actions]]
name = "scratch-shell"
key = "Alt N"
action = "new-worktree"
params = { sandbox = "bwrap", agent = "shell" }
menu = true

[[actions]]
name = "logs-pane"
key = "Alt L"
action = "new-pane"
params = { run = "tail -f log/dev.log", placement = "right" }
"#,
        )
        .unwrap();
        assert_eq!(cfg.actions.len(), 2);
        let a = &cfg.actions[0];
        assert!(a.run.is_none());
        assert_eq!(a.action.as_deref(), Some("new-worktree"));
        assert_eq!(a.params.get("sandbox").map(String::as_str), Some("bwrap"));
        assert_eq!(a.params.get("agent").map(String::as_str), Some("shell"));
        assert!(a.menu);
        let b = &cfg.actions[1];
        assert_eq!(b.action.as_deref(), Some("new-pane"));
        assert_eq!(b.params.get("placement").map(String::as_str), Some("right"));
    }

    #[test]
    fn program_keybinds_and_remap_and_workspace_tables_parse() {
        let cfg: Config = toml::from_str(
            r#"
[program_keybinds.lazygit]
quit = "Ctrl q"
[program_remap.nvim]
"Alt j" = "j"
[workspace.myrepo.keybinds]
focus-down = "Alt j"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.program_keybinds
                .get("lazygit")
                .and_then(|k| k.get("quit"))
                .map(String::as_str),
            Some("Ctrl q")
        );
        assert_eq!(
            cfg.program_remap
                .get("nvim")
                .and_then(|m| m.get("Alt j"))
                .map(String::as_str),
            Some("j")
        );
        assert!(cfg.workspace.contains_key("myrepo"));
    }

    #[test]
    fn named_command_with_hints_parses() {
        let cfg: Config = toml::from_str(
            r#"
[[tools]]
name = "lazygit"
command = "lazygit"
hints = [{ key = "q", label = "quit" }]
"#,
        )
        .unwrap();
        assert_eq!(cfg.tools[0].hints.len(), 1);
        assert_eq!(cfg.tools[0].hints[0].key, "q");
        assert_eq!(cfg.tools[0].hints[0].label, "quit");
    }

    #[test]
    fn issues_full_table_parses() {
        let cfg: Config = toml::from_str(
            r#"
[issues]
provider = "linear"
ttl_secs = 120
max_issues = 50
filter_assignee_me = false
[issues.linear]
api_key = "env:LINEAR_API_KEY"
team_id = "TEAM"
[issues.jira]
base_url = "https://x.atlassian.net"
email = "me@x.com"
project_key = "PROJ"
[issues.github_issues]
extra_flags = ["--assignee", "@me"]
"#,
        )
        .unwrap();
        assert_eq!(cfg.issues.provider, IssueProviderKind::Linear);
        assert_eq!(cfg.issues.ttl_secs, 120);
        assert_eq!(cfg.issues.max_issues, 50);
        assert!(!cfg.issues.filter_assignee_me);
        assert_eq!(cfg.issues.linear.team_id, "TEAM");
        assert_eq!(cfg.issues.jira.project_key, "PROJ");
        assert_eq!(
            cfg.issues.github_issues.extra_flags,
            vec!["--assignee", "@me"]
        );
    }

    #[test]
    fn llm_proxy_full_table_parses() {
        let cfg: Config = toml::from_str(
            r#"
[llm_proxy]
enabled = true
listen = "127.0.0.1:9999"
routing = "speculative"
refuse_on_breach = false
config_path = "/x.json"
first_byte_timeout_secs = 10
idle_timeout_secs = 20
heartbeat_secs = 5
token_reduction = true
token_reduction_level = "balanced"
route_agent = true
bouncer = true
"#,
        )
        .unwrap();
        assert!(cfg.llm_proxy.enabled);
        assert_eq!(cfg.llm_proxy.listen, "127.0.0.1:9999");
        assert_eq!(cfg.llm_proxy.routing, RoutingStrategy::Speculative);
        assert!(!cfg.llm_proxy.refuse_on_breach);
        assert_eq!(cfg.llm_proxy.first_byte_timeout_secs, 10);
        assert_eq!(cfg.llm_proxy.idle_timeout_secs, 20);
        assert_eq!(cfg.llm_proxy.heartbeat_secs, 5);
        assert!(cfg.llm_proxy.token_reduction);
        assert_eq!(
            cfg.llm_proxy.token_reduction_level,
            CompressionLevel::Balanced
        );
        assert!(cfg.llm_proxy.route_agent);
        assert!(cfg.llm_proxy.bouncer);
    }

    #[test]
    fn llm_proxy_bouncer_off_by_default() {
        // The bouncer is opt-in: the additive integration (pi runs its own
        // tools in-process) stays the default.
        let cfg = LlmProxyConfig::default();
        assert!(!cfg.bouncer, "bouncer must default off — AI is additive");
        // A table that omits the key keeps the default.
        let parsed: Config = toml::from_str("[llm_proxy]\nenabled = true\n").unwrap();
        assert!(!parsed.llm_proxy.bouncer);
    }

    #[test]
    fn metrics_interval_secs_alias_parses() {
        // serde alias: kebab-case keys are accepted.
        let cfg: Config =
            toml::from_str("[metrics]\ninterval-secs = 3.0\ntimeout-ms = 200\n").unwrap();
        assert_eq!(cfg.metrics.interval_secs, 3.0);
        assert_eq!(cfg.metrics.timeout_ms, 200);
    }

    #[test]
    fn pin_start_and_restart_enums_parse() {
        let cfg: Config =
            toml::from_str("[[pins]]\nname='x'\ncommand='c'\nstart='eager'\nrestart='onfailure'\n")
                .unwrap();
        assert_eq!(cfg.pins[0].start, PinStart::Eager);
        assert_eq!(cfg.pins[0].restart, PinRestart::OnFailure);
        // Defaults.
        assert_eq!(PinStart::default(), PinStart::Lazy);
        assert_eq!(PinRestart::default(), PinRestart::Never);
    }

    #[test]
    fn task_kind_default_and_parse() {
        assert_eq!(TaskKind::default(), TaskKind::Custom);
        let cfg: Config =
            toml::from_str("[[tasks]]\nname='b'\ncommand='make'\nkind='build'\n").unwrap();
        assert_eq!(cfg.tasks[0].kind, TaskKind::Build);
    }

    #[test]
    fn worktree_template_default_is_all_empty() {
        let t = WorktreeTemplate::default();
        assert!(t.name.is_empty());
        assert!(t.base.is_none() && t.layout.is_none());
        assert!(t.pins.is_empty() && t.commands.is_empty());
    }

    #[test]
    fn vpn_config_defaults_to_disabled() {
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.vpn.provider, VpnProviderKind::None);
        assert!(!cfg.vpn.is_enabled());
        // Forward-looking defaults the runtime relies on.
        assert_eq!(cfg.vpn.mode, VpnMode::Sidecar);
        assert_eq!(cfg.vpn.on_error, VpnOnError::Fail);
        assert_eq!(cfg.vpn.dns, VpnDnsMode::Tunnel);
        assert!(cfg.vpn.ephemeral);
    }

    #[test]
    fn vpn_provider_kind_aliases_and_default() {
        assert_eq!(VpnProviderKind::default(), VpnProviderKind::None);
        for (s, want) in [
            ("tailscale", VpnProviderKind::Tailscale),
            ("ts", VpnProviderKind::Tailscale),
            ("headscale", VpnProviderKind::Headscale),
            ("hs", VpnProviderKind::Headscale),
            ("wg-quick", VpnProviderKind::Wireguard),
            ("ovpn", VpnProviderKind::Openvpn),
            ("nb", VpnProviderKind::Netbird),
            ("zt", VpnProviderKind::Zerotier),
            ("command", VpnProviderKind::Custom),
            ("off", VpnProviderKind::None),
        ] {
            assert_eq!(VpnProviderKind::from_str_validated(s).unwrap(), want, "{s}");
        }
        // Unknown values warn and fall back to the default (infallible deser).
        let k: VpnProviderKind = serde_json::from_str(r#""bogus""#).unwrap();
        assert_eq!(k, VpnProviderKind::None);
    }

    #[test]
    fn vpn_mode_and_dns_aliases() {
        assert_eq!(
            VpnMode::from_str_validated("in-container").unwrap(),
            VpnMode::InContainer
        );
        assert_eq!(
            VpnDnsMode::from_str_validated("filter_front").unwrap(),
            VpnDnsMode::FilterFront
        );
        assert_eq!(
            VpnOnError::from_str_validated("offline").unwrap(),
            VpnOnError::Offline
        );
    }

    #[test]
    fn vpn_config_parses_from_toml_subtables() {
        let cfg: Config = toml::from_str(
            r#"
[sandbox.vpn]
provider = "headscale"
mode = "proxy"
dns = "filter-front"
ready_timeout_secs = 12
ephemeral = false

[sandbox.vpn.tailscale]
auth_key = "env:TS_AUTHKEY"
login_server = "https://headscale.example.com"
tags = ["tag:dev", "tag:ci"]
exit_node = "exit-1"
accept_routes = true
hostname = "my-node"

[sandbox.vpn.wireguard]
config_path = "/etc/wireguard/wg0.conf"
"#,
        )
        .unwrap();
        let v = &cfg.sandbox.vpn;
        assert_eq!(v.provider, VpnProviderKind::Headscale);
        assert_eq!(v.mode, VpnMode::Proxy);
        assert_eq!(v.dns, VpnDnsMode::FilterFront);
        assert_eq!(v.ready_timeout_secs, 12);
        assert!(!v.ephemeral);
        assert_eq!(v.tailscale.login_server, "https://headscale.example.com");
        assert_eq!(v.tailscale.tags, vec!["tag:dev", "tag:ci"]);
        assert_eq!(v.tailscale.exit_node, "exit-1");
        assert!(v.tailscale.accept_routes);
        assert_eq!(v.tailscale.hostname, "my-node");
        assert_eq!(v.wireguard.config_path, "/etc/wireguard/wg0.conf");
    }

    #[test]
    fn vpn_config_round_trips_through_serialization() {
        let v = VpnConfig {
            provider: VpnProviderKind::Zerotier,
            zerotier: ZerotierConfig {
                network_id: "8056c2e21c000001".into(),
                ..ZerotierConfig::default()
            },
            ..VpnConfig::default()
        };
        let s = toml::to_string(&v).unwrap();
        let back: VpnConfig = toml::from_str(&s).unwrap();
        assert_eq!(v, back);
    }

    #[test]
    fn sandbox_overlay_replaces_vpn_wholesale() {
        let mut base = SandboxConfig::default();
        base.vpn.provider = VpnProviderKind::Tailscale;
        base.vpn.tailscale.tags = vec!["tag:base".into()];

        let replacement = VpnConfig {
            provider: VpnProviderKind::Wireguard,
            ..VpnConfig::default()
        };
        let overlay = SandboxOverlay {
            vpn: Some(replacement),
            ..Default::default()
        };
        assert!(!overlay.is_empty());
        overlay.apply(&mut base);
        // Whole-table replace: the base's tailscale tags are gone, not merged.
        assert_eq!(base.vpn.provider, VpnProviderKind::Wireguard);
        assert!(base.vpn.tailscale.tags.is_empty());
    }

    #[test]
    fn sandbox_profile_parses_sealed_tunnel() {
        for s in ["sealed-tunnel", "tunnel-only", "vpn-only"] {
            assert_eq!(
                SandboxProfile::from_str_validated(s).unwrap(),
                SandboxProfile::SealedTunnel,
                "{s}"
            );
        }
    }

    #[test]
    fn sealed_tunnel_profile_floors_match_sealed_but_permits_vpn() {
        let st = SandboxProfile::SealedTunnel;
        // Same hardening floor as sealed.
        assert!(st.read_only_root());
        assert!(st.no_new_privileges());
        assert_eq!(st.pids_limit(), Some(256));
        assert_eq!(st.drop_capabilities(), vec!["ALL".to_string()]);
        assert!(st.forces_no_network());
        // ...but unlike plain sealed, it permits a tunnel.
        assert!(st.permits_vpn());
        assert!(!SandboxProfile::Sealed.permits_vpn());
        assert!(SandboxProfile::Hardened.permits_vpn());
        assert!(SandboxProfile::Open.permits_vpn());
    }

    #[test]
    fn expand_env_ref_reads_file_prefix() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sz-vpn-key-{}.txt", std::process::id()));
        std::fs::write(&path, "  super-secret-key\n").unwrap();
        let r = expand_env_ref(&format!("file:{}", path.display()));
        assert_eq!(r.as_deref(), Some("super-secret-key"));
        std::fs::remove_file(&path).unwrap();
        // Missing file -> None (not an error).
        assert_eq!(expand_env_ref("file:/no/such/sz/file"), None);
    }
}
