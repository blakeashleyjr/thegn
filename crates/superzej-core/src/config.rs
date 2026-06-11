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

use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Prefix a config diagnostic and emit it as a warning. Centralised so the
/// validated-enum deserializers and the env/flag layers speak with one voice.
pub fn config_warn(msg: &str) {
    crate::msg::warn(&format!("config: {msg}"));
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

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct NamedCommand {
    pub name: String,
    pub command: String,
    /// Optional list of hint overrides for the statusbar when this tool is focused.
    #[serde(default)]
    pub hints: Vec<CommandHint>,
}

/// A statusbar hint override for a specific tool.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommandHint {
    pub key: String,
    pub label: String,
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

fn default_true() -> bool {
    true
}

/// A user-defined keybind action (`[[actions]]`): a chord bound to a shell
/// command, optionally surfaced in the Cmd+K menu. See `src/keymap.rs`.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CustomAction {
    /// Stable id + default menu/hint label.
    pub name: String,
    /// Key chord (e.g. "Alt D"); validated by the host keymap.
    pub key: String,
    /// Shell command line run via `sh -c`.
    pub run: String,
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
}

/// `[theme]` — visual tuning: the accent, the focus frame color, and optional
/// per-surface overrides of the whole chrome palette (`[theme.colors]`).
/// Invalid hex values warn-and-default; they never block startup.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct ThemeConfig {
    /// Named palette preset: "storm" (default), "light", "abyss", "ember",
    /// "aurora". `[theme.colors]` overrides apply on top.
    pub preset: String,
    /// Focus accent as "#rrggbb" (default the signature teal).
    pub accent: String,
    /// Frame/highlight color of the focused pane, tab, and chrome edge
    /// (default light blue).
    pub focus_border: String,
    /// Horizontal breathing room (cells) between a pane's frame and its
    /// content, each side.
    pub pane_padding: u16,
    /// Optional overrides for every chrome surface/text color.
    pub colors: ThemeColors,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        ThemeConfig {
            preset: "storm".into(),
            accent: "#76eede".into(),
            focus_border: "#9bd1ff".into(),
            pane_padding: 0,
            colors: ThemeColors::default(),
        }
    }
}

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
            cpu_icon: "\u{f4bc}".into(),
            mem_icon: "\u{efc5}".into(),
            net_icon: "\u{f06f3}".into(),
            // Same-width glyph family as the others (the old gpu glyph
            // rendered narrower in the bundled Nerd Font).
            gpu_icon: "\u{f0fb2}".into(),
            battery_icon: "\u{f0079}".into(),
            battery_charging_icon: "\u{f0084}".into(),
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
            bottom_right: vec!["pr".into(), "loc".into(), "status".into()],
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
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
            tool_mem_max: "6G".into(),
            tool_mem_swap_max: "1G".into(),
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

/// `[dashboard]` — the worktree switcher's live refresh.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct DashboardConfig {
    /// Seconds between refreshes of the `--watch` dashboard pane.
    pub interval_secs: u64,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        DashboardConfig { interval_secs: 4 }
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
    pub backend_chain: Vec<String>, // auto detection order; "host" = host fallback
    pub image: String,              // "" => host-toolchain mode
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
    pub on_missing: OnMissing,
    pub remote: RemoteConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            enabled: true,
            backend: SandboxBackend::Auto,
            default_backend: SandboxBackend::Auto,
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
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            auto_caches: true,
            mounts: vec!["~/.gitconfig:ro".into()],
            init_script: String::new(),
            devenv: false,
            on_missing: OnMissing::Warn,
            remote: RemoteConfig::default(),
        }
    }
}

/// Partial overlay deserialized from a repo-root `.superzej.{toml,yaml,yml,json}`
/// — only the keys present override the global `[sandbox]`. Also reused for the
/// `SUPERZEJ_SANDBOX_*` env layer.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SandboxOverlay {
    pub enabled: Option<bool>,
    pub backend: Option<SandboxBackend>,
    pub default_backend: Option<SandboxBackend>,
    pub backend_chain: Option<Vec<String>>,
    pub image: Option<String>,
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
    pub on_missing: Option<OnMissing>,
    pub remote: Option<RemoteOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
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
        if let Some(v) = self.backend_chain {
            base.backend_chain = v;
        }
        if let Some(v) = self.image {
            base.image = v;
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
        if let Some(v) = self.on_missing {
            base.on_missing = v;
        }
        if let Some(r) = self.remote {
            r.apply(&mut base.remote);
        }
    }

    fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.backend.is_none()
            && self.default_backend.is_none()
            && self.backend_chain.is_none()
            && self.image.is_none()
            && self.network.is_none()
            && self.env_passthrough.is_none()
            && self.auto_caches.is_none()
            && self.mounts.is_none()
            && self.init_script.is_none()
            && self.devenv.is_none()
            && self.on_missing.is_none()
            && self.remote.is_none()
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
}

impl Default for DrawerConfig {
    fn default() -> Self {
        DrawerConfig {
            command: String::new(),
            config_home: String::new(),
            height: "35%".into(),
            width: "full".into(),
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
    // --- arrays of tables (must serialize before any plain sub-table) ---
    pub agents: Vec<NamedCommand>,
    pub tools: Vec<NamedCommand>,
    pub pins: Vec<Pin>,
    pub actions: Vec<CustomAction>,
    pub plugins: Vec<crate::plugin_api::PluginManifest>,
    // --- sub-tables ---
    pub theme: ThemeConfig,
    pub monitor: MonitorConfig,
    pub stats: StatsConfig,
    pub bars: BarsConfig,
    pub pr: PrConfig,
    pub dashboard: DashboardConfig,
    pub watch: WatchConfig,
    pub log: LogConfig,
    pub sandbox: SandboxConfig,
    pub limits: LimitsConfig,
    pub drawer: DrawerConfig,
    pub strip: StripConfig,
    /// Rebind a built-in action by id, e.g. `new-worktree = "Ctrl w"`. The flat
    /// table is the global/default layer; nested mode tables are native-host only.
    pub keybinds: KeybindConfig,
    /// Named keybind profiles (`[profiles.<name>]`), selected by `profile`.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub profiles: std::collections::BTreeMap<String, ProfileConfig>,
    /// Per-workspace config keyed by repo slug (`[workspace.<slug>]`).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub workspace: std::collections::BTreeMap<String, WorkspaceConfig>,
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
            pins: Vec::new(),
            plugins: Vec::new(),
            theme: ThemeConfig::default(),
            monitor: MonitorConfig::default(),
            stats: StatsConfig::default(),
            bars: BarsConfig::default(),
            pr: PrConfig::default(),
            dashboard: DashboardConfig::default(),
            watch: WatchConfig::default(),
            log: LogConfig::default(),
            sandbox: SandboxConfig::default(),
            limits: LimitsConfig::default(),
            drawer: DrawerConfig::default(),
            strip: StripConfig::default(),
            keybinds: KeybindConfig::default(),
            actions: Vec::new(),
            profile: String::new(),
            profiles: std::collections::BTreeMap::new(),
            workspace: std::collections::BTreeMap::new(),
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
    pub dashboard_interval_secs: Option<u64>,
    pub watch_pr_interval_secs: Option<u64>,
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
        set!(base.dashboard.interval_secs, self.dashboard_interval_secs);
        set!(base.watch.pr_interval_secs, self.watch_pr_interval_secs);
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
    // [dashboard] — SUPERZEJ_DASHBOARD_INTERVAL, deprecated SZ_DASH_INTERVAL.
    if let Some(v) = env.get("SUPERZEJ_DASHBOARD_INTERVAL") {
        o.dashboard_interval_secs = parse_num(v, "SUPERZEJ_DASHBOARD_INTERVAL");
    } else if let Some(v) = env.get("SZ_DASH_INTERVAL") {
        config_warn("SZ_DASH_INTERVAL is deprecated; use SUPERZEJ_DASHBOARD_INTERVAL");
        o.dashboard_interval_secs = parse_num(v, "SZ_DASH_INTERVAL");
    }
    if let Some(v) = env.get("SUPERZEJ_WATCH_PR_INTERVAL") {
        o.watch_pr_interval_secs = parse_num(v, "SUPERZEJ_WATCH_PR_INTERVAL");
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
                },
                NamedCommand {
                    name: "shell".into(),
                    command: "__shell__".into(),
                    hints: vec![],
                },
            ];
        }
        if self.tools.is_empty() {
            self.tools = vec![
                NamedCommand {
                    name: "lazygit".into(),
                    command: "lazygit".into(),
                    hints: vec![],
                },
                NamedCommand {
                    name: "yazi".into(),
                    command: "yazi".into(),
                    hints: vec![],
                },
                NamedCommand {
                    name: "editor".into(),
                    command: "${EDITOR:-vi} .".into(),
                    hints: vec![],
                },
                NamedCommand {
                    name: "diff".into(),
                    command: "git diff".into(),
                    hints: vec![],
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
        if let Some(overlay) = load_repo_overlay(repo_root) {
            overlay.sandbox.apply(&mut sb);
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

    /// The accent as a truecolor "R;G;B" fragment; invalid hex falls back to
    /// the default teal.
    pub fn accent_rgb(&self) -> String {
        parse_hex_rgb(&self.theme.accent).unwrap_or_else(|| crate::theme::TEAL.to_string())
    }

    /// The accent as "#rrggbb" (validated; falls back to the default teal).
    pub fn accent_hex(&self) -> String {
        match parse_hex_rgb(&self.theme.accent) {
            Some(_) => self.theme.accent.to_ascii_lowercase(),
            None => "#76eede".into(),
        }
    }

    /// Resolve the full chrome palette: built-in defaults overlaid with any
    /// `[theme]` / `[theme.colors]` overrides. Invalid hex keeps the default.
    pub fn palette(&self) -> crate::theme::Palette {
        self.palette_with_preset(&self.theme.preset)
    }

    /// The palette for a named preset with this config's `[theme.colors]` +
    /// accent/focus overrides applied — the live theme-cycle uses this.
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
        // Only override the preset's focus/accent when the user actually
        // customized them (the built-in defaults would clobber presets).
        let default_theme = ThemeConfig::default();
        if self.theme.focus_border != default_theme.focus_border {
            set(&mut p.focus, &Some(self.theme.focus_border.clone()));
        }
        if self.theme.accent != default_theme.accent {
            p.accent = self.accent_rgb();
        }
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
                    _ => return None,
                };
                slot.clone().unwrap_or_default()
            }
            "pr.ttl_secs" => self.pr.ttl_secs.to_string(),
            "dashboard.interval_secs" => self.dashboard.interval_secs.to_string(),
            "watch.pr_interval_secs" => self.watch.pr_interval_secs.to_string(),
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
    fn monitor_defaults() {
        let m = MonitorConfig::default();
        assert_eq!(m.system, "btm");
        assert_eq!(m.gpu, "nvtop");
    }

    #[test]
    fn stats_defaults() {
        let s = StatsConfig::default();
        assert_eq!(s.refresh_secs, 2.0);
        // Nerd Font glyphs by default; overridable to plain text.
        assert_eq!(s.cpu_icon, "\u{f4bc}");
        assert_eq!(s.mem_icon, "\u{efc5}");
        assert_eq!(s.net_icon, "\u{f06f3}");
        assert_eq!(s.gpu_icon, "\u{f0fb2}");
        assert_eq!(s.battery_icon, "\u{f0079}");
        assert_eq!(s.battery_charging_icon, "\u{f0084}");
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
        assert_eq!(bad.accent_hex(), "#76eede");
        assert_eq!(bad.accent_rgb(), crate::theme::TEAL);
    }

    #[test]
    fn palette_defaults_match_builtins() {
        let p = Config::default().palette();
        assert_eq!(p, crate::theme::Palette::default());
        assert_eq!(p.focus, crate::theme::FOCUS); // #9bd1ff
        assert_eq!(p.border, crate::theme::FRAME); // light grey default
        assert_eq!(p.accent, crate::theme::TEAL);
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
        assert_eq!(p.text, crate::theme::TEXT);
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
    fn drawer_defaults() {
        let d = DrawerConfig::default();
        assert_eq!(d.command, "");
        assert_eq!(d.config_home, ""); // empty = private default
        assert_eq!(d.height, "35%");
        assert_eq!(d.width, "full");
    }

    #[test]
    fn config_without_drawer_section_uses_defaults() {
        let cfg: Config = toml::from_str("base_branch = \"main\"").unwrap();
        assert_eq!(cfg.drawer.height, "35%");
        assert_eq!(cfg.drawer.width, "full");
        assert_eq!(cfg.drawer.command, "");
    }

    #[test]
    fn drawer_section_overrides_parse() {
        let cfg: Config = toml::from_str(
            "[drawer]\ncommand = \"ranger\"\nconfig_home = \"system\"\nheight = \"50%\"\nwidth = \"center\"\n",
        )
        .unwrap();
        assert_eq!(cfg.drawer.command, "ranger");
        assert_eq!(cfg.drawer.config_home, "system");
        assert_eq!(cfg.drawer.height, "50%");
        assert_eq!(cfg.drawer.width, "center");
    }

    #[test]
    fn drawer_partial_section_keeps_other_defaults() {
        // Only height set; the rest fall back to defaults via #[serde(default)].
        let cfg: Config = toml::from_str("[drawer]\nheight = \"20%\"\n").unwrap();
        assert_eq!(cfg.drawer.height, "20%");
        assert_eq!(cfg.drawer.width, "full");
        assert_eq!(cfg.drawer.command, "");
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
            ("SUPERZEJ_DASHBOARD_INTERVAL", "6"),
            ("SUPERZEJ_WATCH_PR_INTERVAL", "13"),
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
        assert_eq!(c.dashboard.interval_secs, 6);
        assert_eq!(c.watch.pr_interval_secs, 13);
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
            "dashboard.interval_secs",
            "watch.pr_interval_secs",
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
        assert_eq!(c.accent_hex(), "#76eede");
        assert!(c.accent_rgb().contains(';'));
        c.theme.accent = "#fff".into();
        assert_eq!(c.accent_rgb(), "255;255;255"); // 3-digit hex expands
        c.theme.accent = "garbage".into();
        assert_eq!(c.accent_hex(), "#76eede"); // invalid falls back
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
        });
        assert_eq!(cfg.tool_command("test"), Some("echo test"));
        assert_eq!(cfg.tool_command("missing"), None);
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
}
