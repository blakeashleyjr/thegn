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
use crate::placement::{K8sPlacement, Placement, ProviderPlacement, SshPlacement, TransportKind};
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
/// environment variable `VAR_NAME`. Any other non-empty string is returned
/// as-is. An empty string or a missing environment variable both return `None`.
pub fn expand_env_ref(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(var_name) = v.strip_prefix("env:") {
        std::env::var(var_name)
            .ok()
            .filter(|s| !s.trim().is_empty())
    } else {
        Some(v.to_string())
    }
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
    pub enum SandboxProfile: "sandbox profile" {
        Open = "open" | "off" | "none",
        Hardened = "hardened" | "guarded",
        Sealed = "sealed" | "locked" | "isolated",
    } default = Hardened;
}

impl SandboxProfile {
    /// Mount the container root filesystem read-only (writable: the worktree,
    /// cache binds, and a tmpfs `/tmp`).
    pub fn read_only_root(self) -> bool {
        matches!(self, SandboxProfile::Hardened | SandboxProfile::Sealed)
    }
    /// Set `no-new-privileges` so setuid/setgid binaries can't escalate.
    pub fn no_new_privileges(self) -> bool {
        matches!(self, SandboxProfile::Hardened | SandboxProfile::Sealed)
    }
    /// Cap the number of processes (fork-bomb containment); `None` = unlimited.
    pub fn pids_limit(self) -> Option<i64> {
        match self {
            SandboxProfile::Open => None,
            SandboxProfile::Hardened => Some(512),
            SandboxProfile::Sealed => Some(256),
        }
    }
    /// Linux capabilities to drop. `sealed` drops everything; `hardened` leaves
    /// the runtime's defaults so debuggers (ptrace), `ping` (NET_RAW), and
    /// low-port binds keep working.
    pub fn drop_capabilities(self) -> Vec<String> {
        match self {
            SandboxProfile::Sealed => vec!["ALL".to_string()],
            _ => Vec::new(),
        }
    }
    /// Capabilities to add back after dropping (reserved for future tuning).
    pub fn add_capabilities(self) -> Vec<String> {
        Vec::new()
    }
    /// Force `network=none` regardless of the configured network mode.
    pub fn forces_no_network(self) -> bool {
        matches!(self, SandboxProfile::Sealed)
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
    /// files on the host and only execs remotely; `sshfs` mounts the remote tree
    /// locally. (`local_exec`/`sshfs` lifecycle is wired in the data-mode phase.)
    pub enum DataMode: "data mode" {
        InEnv = "in_env" | "remote" | "native",
        LocalExec = "local_exec" | "local",
        Sshfs = "sshfs" | "mount",
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
    } default = Tab;
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
        }
    }
}

impl LlmProxyConfig {
    /// The launch spec for the `szproxy` daemon — `(program, args, env)` — or
    /// `None` when the proxy is disabled. The host feeds this to its process
    /// supervisor (e.g. as a `restart = "always"` pinned daemon). `SZPROXY_LISTEN`
    /// and `SZPROXY_CONFIG` mirror the standalone env knobs the daemon reads.
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
/// tab (`location = "tab"`, the default) or as a tiled pane in the active
/// layout (`location = "layout"`). Pins are summoned via `Alt-1..9` / the
/// tabbar's pin chips, and can be global (all workspaces) or workspace-scoped.
/// See `src/commands/pin.rs`.
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

/// Git behavior knobs for the panel's write operations (`[git]`).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct GitConfig {
    /// Pass `-c commit.gpgSign=false -c tag.gpgSign=false` to
    /// history-rewriting operations (rebase, amend, cherry-pick) so a gpg
    /// passphrase prompt can never hang a background op. Off by default: a
    /// working gpg-agent signs headlessly.
    pub override_gpg: bool,
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
    /// network_audit, env_passthrough, etc.). Applied after the global [sandbox]
    /// and before the repo-root overlay, so per-profile restrictions take effect
    /// without touching per-repo config.
    #[serde(skip_serializing_if = "SandboxOverlay::is_empty")]
    pub sandbox: SandboxOverlay,
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
    /// Icon for the battery stat (discharging).
    pub battery_icon: String,
    /// Icon shown while the battery is charging / on AC.
    pub battery_charging_icon: String,
    /// Battery percentage at/below which the widget turns red.
    pub battery_warn: u8,
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
            battery_icon: "\u{f240}".into(),          // nf-fa-battery_full
            battery_charging_icon: "\u{f0e7}".into(), // nf-fa-bolt — lightning bolt
            battery_warn: 25,
            refresh_rates: vec![1.0, 2.0, 5.0, 10.0],
        }
    }
}

/// `[bars]` — the customizable widget bars framing the workspace. Each slot is
/// an ordered widget-id list; unknown ids warn and are skipped. Built-ins:
/// `brand` (superzej + version), `cpu`, `mem`, `gpu`, `net`, `battery`,
/// `date`, `clock` (top bar) and `keyhints` (context-dependent keybinds),
/// `pr` (forge + PR number/state), `status` (transient messages + the
/// keybind-lock badge) for the bottom bar.
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
                "gpu".into(),
                "net".into(),
                "battery".into(),
                "date".into(),
                "clock".into(),
            ],
            bottom_left: vec!["keyhints".into()],
            bottom_right: vec!["pr".into(), "tests".into(), "loc".into(), "status".into()],
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

/// `[issues]` — issue tracker integration (Linear, GitHub Issues, Jira).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IssuesConfig {
    /// Active provider. `"none"` disables the integration.
    pub provider: IssueProviderKind,
    /// Cache TTL (seconds) before a background re-fetch.
    pub ttl_secs: u64,
    /// Maximum issues to fetch and display.
    pub max_issues: usize,
    /// Pre-filter to issues assigned to the authenticated user.
    pub filter_assignee_me: bool,
    pub linear: LinearConfig,
    pub github_issues: GitHubIssuesConfig,
    pub jira: JiraConfig,
}

impl Default for IssuesConfig {
    fn default() -> Self {
        IssuesConfig {
            provider: IssueProviderKind::None,
            ttl_secs: 60,
            max_issues: 100,
            filter_assignee_me: true,
            linear: LinearConfig::default(),
            github_issues: GitHubIssuesConfig::default(),
            jira: JiraConfig::default(),
        }
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
    /// Provider sandbox template/image to create from.
    pub template: String,
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
            && self.template.is_empty()
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
    pub devenv: bool,        // wrap inner cmd with `devenv shell --`
    /// Shell to use inside the sandbox. `""` = resolve from the host's `$SHELL`
    /// at pane-spawn time. Set to an absolute path or name (e.g. `"zsh"`) to
    /// override per workspace via `.superzej.toml`.
    pub shell: String,
    pub on_missing: OnMissing,
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
            devenv: false,
            shell: String::new(),
            on_missing: OnMissing::Warn,
            remote: RemoteConfig::default(),
            network_allow: Vec::new(),
            network_block: Vec::new(),
            network_audit: false,
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
    pub devenv: Option<bool>,
    pub shell: Option<String>,
    pub on_missing: Option<OnMissing>,
    pub remote: Option<RemoteOverlay>,
    pub network_allow: Option<Vec<String>>,
    pub network_block: Option<Vec<String>>,
    pub network_audit: Option<bool>,
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
        if let Some(v) = self.devenv {
            base.devenv = v;
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
            && self.devenv.is_none()
            && self.shell.is_none()
            && self.on_missing.is_none()
            && self.remote.is_none()
            && self.network_allow.is_none()
            && self.network_block.is_none()
            && self.network_audit.is_none()
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
    /// Unset kinds use their built-in [`NotificationKind::default_priority`];
    /// unknown keys/values are ignored. `alert` raises the red flag, `notice`
    /// the neutral unread count, `info` is inbox-only (never counted).
    pub priority: std::collections::BTreeMap<String, String>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        NotificationsConfig {
            desktop: true,
            desktop_min_urgency: "normal".into(),
            process_exit: "failures_and_tasks".into(),
            priority: std::collections::BTreeMap::new(),
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
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PanelConfig {
    /// Section display order, by key (`"changes"`, `"git"`, `"files"`,
    /// `"tests"`, `"debug"`, `"sandbox"`, `"db"`, `"telemetry"`, `"keys"`).
    /// Sections omitted from the list are hidden; an empty list (the default)
    /// shows every section in its built-in order. Unknown keys are ignored.
    pub sections: Vec<String>,
}

/// Default `keymap_preset` (no IDE overlay). A free function so both the field
/// default and the `skip_serializing_if` predicate agree.
fn default_preset() -> String {
    "default".into()
}

fn is_default_preset(s: &str) -> bool {
    s.is_empty() || s == "default"
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct Config {
    // --- scalar values (must serialize before any sub-table for TOML) ---
    pub worktrees_dir: String,
    pub workspaces_dir: String,
    pub base_branch: String,
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
    pub git: GitConfig,
    pub theme: ThemeConfig,
    pub monitor: MonitorConfig,
    pub stats: StatsConfig,
    pub metrics: MetricsConfig,
    pub apps: AppsConfig,
    pub bars: BarsConfig,
    pub pr: PrConfig,
    pub issues: IssuesConfig,
    pub watch: WatchConfig,
    pub log: LogConfig,
    pub sandbox: SandboxConfig,
    pub limits: LimitsConfig,
    pub drawer: DrawerConfig,
    pub notifications: NotificationsConfig,
    pub strip: StripConfig,
    pub panel: PanelConfig,
    pub search: SearchConfig,
    pub palette: PaletteConfig,
    pub lsp: LspConfig,
    /// The LLM proxy daemon (`[llm_proxy]`). Disabled by default — AI is additive.
    pub llm_proxy: LlmProxyConfig,
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
            branch_prefix: "sz/".into(),
            picker: Picker::Auto,
            worktree_mode: WorktreeMode::Global,
            name_scheme: NameScheme::Words,
            auto_remove_worktree: false,
            confirm_delete: true,
            repo_roots: Vec::new(),
            repo_scan_depth: 5,
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
            watch: WatchConfig::default(),
            log: LogConfig::default(),
            sandbox: SandboxConfig::default(),
            limits: LimitsConfig::default(),
            drawer: DrawerConfig::default(),
            notifications: NotificationsConfig::default(),
            strip: StripConfig::default(),
            panel: PanelConfig::default(),
            search: SearchConfig::default(),
            palette: PaletteConfig::default(),
            lsp: LspConfig::default(),
            llm_proxy: LlmProxyConfig::default(),
            keybinds: KeybindConfig::default(),
            actions: Vec::new(),
            profile: String::new(),
            keymap_preset: default_preset(),
            profiles: std::collections::BTreeMap::new(),
            workspace: std::collections::BTreeMap::new(),
            env: std::collections::BTreeMap::new(),
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
        set!(base.branch_prefix, self.branch_prefix);
        set!(base.picker, self.picker);
        set!(base.worktree_mode, self.worktree_mode);
        set!(base.name_scheme, self.name_scheme);
        set!(base.auto_remove_worktree, self.auto_remove_worktree);
        set!(base.repo_scan_depth, self.repo_scan_depth);
        set!(base.profile, self.profile);
        set!(base.theme.accent, self.accent);
        set!(base.theme.focus_border, self.focus_border);
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
    if let Some(host) = env.get("SUPERZEJ_SANDBOX_REMOTE_HOST") {
        o.sandbox.remote = Some(RemoteOverlay {
            host: Some(host),
            ..Default::default()
        });
    }
    o
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
        let placement = build_env_placement(envc, &sb, loc);
        Environment {
            name,
            placement,
            sandbox: sb,
            data: envc.data,
        }
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

/// Map the legacy `[sandbox.remote] mode` onto the env [`DataMode`], so the
/// default env honours an existing `mode = "sshfs"`/`"local_exec"` config.
fn data_mode_from_remote(mode: RemoteMode) -> DataMode {
    match mode {
        RemoteMode::Remote => DataMode::InEnv,
        RemoteMode::LocalExec => DataMode::LocalExec,
        RemoteMode::Sshfs => DataMode::Sshfs,
    }
}

/// Build the runtime [`Placement`] for a named env from its `[env.<name>]`
/// placement mode + the matching sub-table. For `ssh`, an empty `[env.*.ssh]
/// host` falls back to the worktree's own remote target, then `[sandbox.remote]`.
fn build_env_placement(envc: &EnvConfig, sb: &SandboxConfig, loc: &GitLoc) -> Placement {
    let opt = |s: &str| {
        let t = s.trim();
        (!t.is_empty()).then(|| t.to_string())
    };
    match envc.placement {
        PlacementMode::Local => Placement::Local,
        PlacementMode::Ssh => {
            let kind = match envc.ssh.transport {
                RemoteTransport::Ssh => TransportKind::Ssh,
                RemoteTransport::Mosh => TransportKind::Mosh,
            };
            let (host, port, forward_agent) = if !envc.ssh.host.trim().is_empty() {
                let port = if envc.ssh.port == 0 {
                    22
                } else {
                    envc.ssh.port
                };
                (
                    envc.ssh.host.trim().to_string(),
                    port,
                    envc.ssh.forward_agent,
                )
            } else if let Some(t) = loc.ssh() {
                (t.host.clone(), t.port, t.forward_agent)
            } else {
                (
                    sb.remote.host.clone(),
                    sb.remote.port,
                    sb.remote.forward_agent,
                )
            };
            Placement::Ssh(SshPlacement {
                host,
                port,
                forward_agent,
                kind,
                ssh_config: opt(&envc.ssh.ssh_config),
                jump_host: opt(&envc.ssh.jump_host),
                identity: opt(&envc.ssh.identity),
                extra_args: envc.ssh.extra_args.clone(),
            })
        }
        PlacementMode::K8s => Placement::K8s(K8sPlacement {
            kubectl: opt(&envc.k8s.kubectl).unwrap_or_else(|| "kubectl".to_string()),
            context: opt(&envc.k8s.context),
            namespace: opt(&envc.k8s.namespace),
            pod: envc.k8s.pod.trim().to_string(),
            container: opt(&envc.k8s.container),
            pod_template: opt(&envc.k8s.pod_template).map(|p| util::expand_tilde(&p)),
            image: opt(&envc.k8s.image),
        }),
        PlacementMode::Provider => {
            let sub = |tpl: &[String]| {
                tpl.iter()
                    .map(|s| s.replace("{id}", envc.provider.id.trim()))
                    .collect::<Vec<_>>()
            };
            let control_prefix = sub(&envc.provider.exec_command);
            let interactive_prefix = if envc.provider.interactive_command.is_empty() {
                control_prefix.clone()
            } else {
                sub(&envc.provider.interactive_command)
            };
            Placement::Provider(ProviderPlacement {
                provider: envc.provider.provider.trim().to_string(),
                id: envc.provider.id.trim().to_string(),
                interactive_prefix,
                control_prefix,
                up_command: sub(&envc.provider.up_command),
                down_command: sub(&envc.provider.down_command),
            })
        }
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
        let env = cfg.resolve_env(&dir, &loc, None);
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
        let env = cfg.resolve_env(&dir, &loc, Some("local-containers"));
        assert_eq!(env.name, "local-containers");
        assert!(env.placement.is_local());
        assert_eq!(env.sandbox.backend, SandboxBackend::Podman);
        assert_eq!(env.sandbox.image, "registry.example.com/dev:latest");
        assert_eq!(env.sandbox.profile, SandboxProfile::Sealed);
        let _ = std::fs::remove_dir_all(&dir);
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
        let env = cfg.resolve_env(&dir, &loc, Some("company-k8s"));
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
        let env = cfg.resolve_env(&dir, &loc, Some("daytona"));
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
        let env = cfg.resolve_env(&dir, &loc, Some("daytona"));
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
        assert_eq!(cfg.resolve_env(&dir, &loc, None).name, "r");
        assert_eq!(
            cfg.resolve_env(&dir, &loc, None).sandbox.backend,
            SandboxBackend::Docker
        );
        // Explicit selection beats the repo overlay.
        assert_eq!(cfg.resolve_env(&dir, &loc, Some("x")).name, "x");
        // Empty/whitespace selection is ignored (falls through to repo).
        assert_eq!(cfg.resolve_env(&dir, &loc, Some("  ")).name, "r");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_env_name_falls_back_to_default() {
        let cfg = Config::default();
        let dir = tmpdir("env-unknown");
        let loc = GitLoc::Local(dir.clone());
        let env = cfg.resolve_env(&dir, &loc, Some("does-not-exist"));
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
        let env = cfg.resolve_env(&dir, &loc, Some("remote-dev"));
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

        // Alert set is exactly the four failures; unread set excludes Info.
        let alerts = cfg.alert_kind_names();
        assert_eq!(alerts.len(), 4);
        for k in ["agent_failed", "test_failed", "log_error", "process_failed"] {
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
    fn bars_config_defaults() {
        let b = BarsConfig::default();
        assert_eq!(b.top_left, vec!["brand"]);
        assert_eq!(
            b.top_right,
            vec!["cpu", "mem", "gpu", "net", "battery", "date", "clock"]
        );
        assert_eq!(b.bottom_left, vec!["keyhints"]);
        assert_eq!(b.bottom_right, vec!["pr", "tests", "loc", "status"]);
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
}
