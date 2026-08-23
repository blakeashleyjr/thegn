//! User configuration — a layered, validated system.
//!
//! Precedence, lowest to highest:
//!   1. built-in defaults (`Config::default`)
//!   2. `$XDG_CONFIG_HOME/thegn/config.toml` (or `--config <path>`)
//!   3. `THEGN_<SECTION>_<KEY>` environment overrides (see [`env_overlay`])
//!   4. CLI flags (a [`ConfigOverlay`] built by `main`)
//!
//! Plus a repo-root `.thegn.{toml,yaml,yml,json}` overlay, scoped to
//! `[sandbox]`, applied per-repo in [`Config::repo_sandbox`].
//!
//! Bad values never block: an unknown enum value warns and falls back to the
//! default (the strict check lives in `thegn config validate`). The
//! home-manager module renders the file; keys match the serde field names.

use crate::config_defaults::{default_git_context, default_prompt_kind, default_true};
use crate::env::Environment;
use crate::remote::GitLoc;
use crate::resource_alert;
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Prefix a config diagnostic and emit it as a warning. Centralised so the
/// validated-enum deserializers and the env/flag layers speak with one voice.
pub fn config_warn(msg: &str) {
    crate::msg::warn(&format!("config: {msg}"));
}

/// Validate a chrono strftime format string.
///
/// `DateTime::format()` is **lazy**: it hands back a `DelayedFormat` that only
/// discovers a bad specifier when it is `Display`ed. For thegn that Display
/// happens inside the masthead widget on the render path, so a one-character
/// typo in `[bars] clock_format` is a compositor panic with no diagnostic.
/// Walking the parsed items at config-load time turns that into a warning.
pub fn validate_strftime(fmt: &str) -> Result<(), String> {
    use chrono::format::{Item, StrftimeItems};
    if StrftimeItems::new(fmt).any(|i| matches!(i, Item::Error)) {
        return Err(format!("invalid strftime specifier in {fmt:?}"));
    }
    Ok(())
}

/// Whether a strftime format renders seconds — i.e. whether a clock drawn with
/// it needs a 1s tick rather than a 1min one.
///
/// Walks the *parsed* items rather than substring-matching `"%S"`, so `%T`
/// (`%H:%M:%S`), `%r`, `%X` and `%s` all count while the escaped literal `%%S`
/// correctly does not.
pub fn strftime_needs_seconds(fmt: &str) -> bool {
    use chrono::format::{Fixed, Item, Numeric, StrftimeItems};
    StrftimeItems::new(fmt).any(|i| {
        matches!(
            i,
            Item::Numeric(
                Numeric::Second | Numeric::Nanosecond | Numeric::Timestamp,
                _
            ) | Item::Fixed(
                Fixed::Nanosecond
                    | Fixed::Nanosecond3
                    | Fixed::Nanosecond6
                    | Fixed::Nanosecond9
                    | Fixed::RFC2822
                    | Fixed::RFC3339,
            )
        )
    })
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
        let path = util::expand_tilde(path.trim());
        std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        Some(v.to_string())
    }
}

// NOTE: this module deliberately has no private `expand_tilde`. It used to,
// reading `$HOME` directly — which is unset on Windows, so a `file:~/…`
// secrets-ref silently failed to expand and the secret resolved to `None`.
// Everything now goes through `util::expand_tilde`, which resolves
// `%USERPROFILE%` there.

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
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis enum $name { $( $variant ),+ }

        // Manual `JsonSchema`: the derive would advertise the Rust variant
        // identifiers (`InRepo`, `SealedTunnel`, …) which `from_str_validated`
        // rejects. Emit the canonical strings instead, plus an
        // `x-thegn-enum` extension (kind + aliases) that marks the node for
        // the schema-driven walker in `config_validate` — every enum this
        // macro defines is strict-checked by `config validate` by
        // construction, no hand-maintained key list.
        impl schemars::JsonSchema for $name {
            fn schema_name() -> String {
                stringify!($name).into()
            }
            fn json_schema(
                _: &mut schemars::r#gen::SchemaGenerator,
            ) -> schemars::schema::Schema {
                let mut obj = schemars::schema::SchemaObject {
                    instance_type: Some(schemars::schema::InstanceType::String.into()),
                    enum_values: Some(vec![$( serde_json::Value::from($canon) ),+]),
                    ..Default::default()
                };
                obj.extensions.insert(
                    crate::config_validate::ENUM_MARKER.to_string(),
                    serde_json::json!({
                        "kind": $kind,
                        // Comma inside the inner repetition so zero-alias
                        // variants expand to nothing (never `[, ]`).
                        "aliases": [ $($( $alias, )*)+ ],
                    }),
                );
                schemars::schema::Schema::Object(obj)
            }
        }

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
pub(crate) use config_enum;

config_enum! {
    /// TUI used for the agent/tool/repo pickers.
    pub enum Picker: "picker" {
        Auto = "auto", Gum = "gum", Fzf = "fzf", Select = "select",
    } default = Auto;
}
// `FailoverMode` + its deserializers live in `config_placement` (re-exported).
pub use crate::config_placement::FailoverMode;
// The remote/data-placement enums + `MergeRemoteMode` live in `config_remote`
// (re-exported), keeping `config.rs` under the god-file ratchet.
use crate::config_placement::{de_failover, de_failover_opt};
pub(crate) use crate::config_remote::data_mode_from_remote;
pub use crate::config_remote::{DataMode, MergeRemoteMode, RemoteMode};
// The terminal display/glyph config enums (UndercurlMode, ColorMode, GlyphMode,
// AgentGlyphs) live in the `config_theme` sibling module to keep this god-file
// flat; re-exported so `config::{ColorMode, …}` import paths keep working.
pub use crate::config_theme::{
    AgentGlyphs, ColorMode, GlyphMode, MascotKind, MascotMotion, ThemeColors, ThemeHues,
    UndercurlMode,
};
// The `[[accounts]]` entry type lives with its domain logic in `account`; the
// control-plane `[daemon]`/`[serve]` sections live in `config_daemon`.
pub use crate::account::Account;
pub use crate::config_daemon::{DaemonConfig, ServeConfig};

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
    /// workspace/repo via the `.thegn.toml` overlay, or `THEGN_SANDBOX_PROFILE`.
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
    /// Whether thegn pre-warms a worktree's `direnv` cache **on the host** so
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
    /// `[sandbox.home] strategy` — how hard thegn tries to reproduce *your*
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
    /// Default log verbosity (the `THEGN_LOG` env filter can refine it
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
    /// `[merge_queue] conflict_handoff` — fate of a branch the fold can't land:
    /// `"agent"` (default) dispatches the agent to fix it, `"notify"`/`"manual"` don't.
    pub enum ConflictHandoff: "conflict handoff" {
        Agent = "agent",
        Notify = "notify",
        Manual = "manual" | "off" | "none",
    } default = Agent;
}

config_enum! {
    /// `[merge_queue] on_landed` — what to do with a worktree whose branch just
    /// landed (only when `organize_folders = true`): `"off"` nothing, `"move"` file
    /// it into `merged_folder`, `"detach"` remove the worktree but keep the branch,
    /// `"remove"` remove the worktree AND delete the now-merged branch.
    pub enum OnLanded: "on landed" {
        Off = "off" | "none",
        Move = "move" | "folder",
        Detach = "detach",
        Remove = "remove" | "cleanup" | "delete",
        // NB: `MergeQueueConfig::default()` sets `Remove`, which is the shipped
        // default; this enum-level default only applies where an `OnLanded` is
        // built standalone.
    } default = Off;
}

/// `[merge_queue]` — the local "fold-actor": fold worktree branches onto
/// `target_branch` in the object DB (no checkout); land clean, defer conflicts.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MergeQueueConfig {
    /// Master switch. When off, the `integrate`/`merge` commands are inert.
    pub enabled: bool,
    /// How a remote-host (sprite) worktree lands onto the target. See [`MergeRemoteMode`].
    pub remote_mode: MergeRemoteMode,
    /// Branch the fold advances. `"auto"` resolves it per repo (HEAD/default).
    pub target_branch: String,
    /// Shell command gating the CAS-advance. Empty disables it. Keep it lean
    /// (e.g. `just test`): pre-push already ran clippy + test before enqueue.
    ///
    /// It runs in a **bare checkout of the folded tip with no dependencies
    /// installed** — no `node_modules`, no virtualenv, no `.direnv`. A command
    /// whose entry point is a project-local binary (`turbo`, `pytest` in a venv,
    /// a Go tool dir) will not resolve there; use `gate_setup_command` to
    /// provision it first. Rust works out of the box only because `cargo` is
    /// global.
    pub gate_command: String,
    /// Command run in the gate worktree BEFORE `gate_command`, to install the
    /// dependencies the gate needs (`pnpm install --frozen-lockfile`, `uv sync`,
    /// `bundle install`, …). Empty (default) skips it.
    ///
    /// It is **not** part of the pass/fail verdict: a non-zero exit is reported
    /// as a gate *environment* failure, never as "this branch breaks the build",
    /// so a broken setup can neither blame a branch nor wake the fixing agent.
    ///
    /// Runs on every gate invocation, including when a reused worktree is
    /// refreshed in place — so it must be idempotent and cheap when warm, which
    /// every real package manager's install command is. Running it only on
    /// worktree creation would silently rot the moment a lockfile changed.
    pub gate_setup_command: String,
    /// Whether to run `gate_command` at all.
    pub gate_on: bool,
    /// Reuse a stable per-repo gate worktree + `target/` between folds (warm rebuild); `false` ⇒ throwaway `/tmp` (always cold).
    pub gate_reuse_worktree: bool,
    /// `CARGO_TARGET_DIR` for the gate. Empty ⇒ `$XDG_STATE_HOME/thegn/gate/<repo>/target`.
    pub gate_target_dir: String,
    /// On a red gate, bisect to defer just the offending branch, not the batch.
    pub bisect_on_red: bool,
    /// Auto-commit uncommitted worktree work before folding (else skip dirty ones).
    pub snapshot_dirty: bool,
    /// Conflicts confined to these paths (exact/basename) are regenerable, not
    /// handed to a human (e.g. `Cargo.lock`).
    ///
    /// Defaults to the common lockfiles across ecosystems, not just Rust's: a
    /// name costs nothing in a repo that doesn't have that file, and lockfile
    /// skew is precisely the conflict class worth automating — two branches that
    /// both touched the lockfile conflict textually while agreeing semantically.
    pub regenerate_paths: Vec<String>,
    /// Throwaway-worktree command rebuilding `regenerate_paths` to auto-land a
    /// lockfile-only conflict. Empty defers instead.
    pub regenerate_command: String,
    /// What to do with a deferred (conflicting) branch.
    pub conflict_handoff: ConflictHandoff,
    /// Headless CLI agent the driver runs in the branch's worktree to rebase/fix,
    /// then re-folds. Template `{prompt}`/`{branch}`/`{target}`; empty ⇒ notify.
    ///
    /// Placeholders are shell-quoted during substitution, so write them **bare**
    /// (`claude -p {prompt}`) — quoting one yourself delivers the value to the
    /// agent with literal quote characters attached.
    pub agent_command: String,
    /// A configured `[[agents]]`/`[[tools]]` entry to run headlessly, named
    /// instead of restating a command line. Its provider supplies the
    /// non-interactive flags (`claude` ⇒ `-p … --permission-mode acceptEdits`),
    /// so the agent is declared once. `agent_command` overrides this; with
    /// neither set the handoff degrades to notifying.
    pub agent: String,
    /// Queue driver: CAS-advance on green; off ⇒ stop at `ready` for a `merge land`.
    pub auto_land: bool,
    /// Agent-dispatch → re-fold cycles per branch before it's `needs_human`.
    pub agent_max_attempts: u32,
    /// Watchdog (seconds) for one agent invocation. 0 disables it.
    pub agent_timeout_secs: u64,
    /// Master switch for the merge-queue sidebar lifecycle — filing worktrees
    /// into folders as branches move through the queue, and removing a worktree
    /// once its branch lands cleanly (see `on_landed`). On by default; `false` ⇒
    /// fields below inert (today's manual behavior).
    pub organize_folders: bool,
    /// Folder for an enqueued (`queued`) worktree. Empty ⇒ don't file on enqueue.
    pub queued_folder: String,
    /// What to do with a worktree whose branch just landed (only when
    /// `organize_folders = true`):
    ///
    /// - `"off"` (aliases `"none"`) — nothing.
    /// - `"move"` (alias `"folder"`) — file the worktree into `merged_folder`.
    /// - `"detach"` — remove the worktree, **keep** the branch.
    /// - `"remove"` (aliases `"cleanup"`, `"delete"`) — remove the worktree AND
    ///   delete the merged branch. This is the default.
    ///
    /// Spelled out here rather than deferred to the enum, because this doc is
    /// what the generated JSON schema shows and a `[OnLanded]` rustdoc link is
    /// dead there. Worth knowing: `remove` deletes the branch ref (and its
    /// reflog with it) — the one irreversible-feeling step in an otherwise
    /// recoverable pipeline. The commits survive as ancestors of the target;
    /// choose `detach` to keep the branch name too. A worktree with uncommitted
    /// changes is never removed, and keeps its branch.
    pub on_landed: OnLanded,
    /// Folder for a landed branch when `on_landed = "move"`. Empty ⇒ don't file.
    pub merged_folder: String,
    /// Folder for a branch that fails to land. Empty ⇒ leave it in place.
    pub failed_folder: String,
    /// `[merge_queue.prompts]` — what the fixing agent is told, per blocker kind.
    pub prompts: MergeQueuePrompts,
}

/// `[merge_queue.prompts]` — the task prompt handed to the fixing agent, one
/// template per blocker kind. An empty template means "use thegn's built-in
/// instructions for that kind", so the whole table is optional and unsetting a
/// key restores the default rather than sending the agent an empty prompt.
///
/// Templates take `{var}` placeholders and are **not** shell-quoted (the prompt
/// is prose; the *command* template is what gets quoted). Unknown placeholders
/// are a configuration error — see `thegn config validate`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MergeQueuePrompts {
    /// Branch conflicts textually with the target. Vars: `{branch}`, `{target}`,
    /// `{worktree}`, `{paths}` (one `  - path` line each).
    pub conflict: String,
    /// Branch merges clean but the test gate is red. Vars: `{branch}`,
    /// `{target}`, `{worktree}`, `{log}` (the gate output tail).
    pub gate_failure: String,
}

impl MergeQueuePrompts {
    /// The configured template for a kind, or `None` to use the built-in.
    pub fn for_kind(&self, kind: crate::agent_task::TaskKind) -> Option<&str> {
        let t = match kind {
            crate::agent_task::TaskKind::MergeConflict => &self.conflict,
            crate::agent_task::TaskKind::GateFailure => &self.gate_failure,
            // The PR kinds belong to `[pr_queue.prompts]`; this table has no
            // opinion on them, so they always fall through to the built-in.
            k if k.is_pr() => return None,
            _ => return None,
        };
        (!t.trim().is_empty()).then_some(t.as_str())
    }

    /// The template actually used for a kind: the configured one, else built-in.
    pub fn resolve(&self, kind: crate::agent_task::TaskKind) -> &str {
        self.for_kind(kind)
            .unwrap_or_else(|| crate::agent_task::default_prompt(kind))
    }
}

impl Default for MergeQueueConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            remote_mode: MergeRemoteMode::default(),
            target_branch: "auto".to_string(),
            gate_command: String::new(),
            gate_setup_command: String::new(),
            gate_on: true,
            gate_reuse_worktree: true,
            gate_target_dir: String::new(),
            bisect_on_red: true,
            snapshot_dirty: false,
            regenerate_paths: [
                // Rust
                "Cargo.lock",
                // JS/TS
                "pnpm-lock.yaml",
                "package-lock.json",
                "yarn.lock",
                "bun.lockb",
                // Python
                "poetry.lock",
                "uv.lock",
                "Pipfile.lock",
                // Go / Ruby / PHP
                "go.sum",
                "Gemfile.lock",
                "composer.lock",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            regenerate_command: String::new(),
            conflict_handoff: ConflictHandoff::default(),
            agent_command: String::new(),
            agent: String::new(),
            auto_land: true,
            agent_max_attempts: 2,
            agent_timeout_secs: 900,
            organize_folders: true,
            queued_folder: "Merging".to_string(),
            on_landed: OnLanded::Remove,
            merged_folder: "Merged".to_string(),
            failed_folder: "Needs attention".to_string(),
            prompts: MergeQueuePrompts::default(),
        }
    }
}

/// The `[workspace.<slug>]` key for a repo.
///
/// One definition so every consumer of the workspace layer agrees on the slug —
/// it was previously inlined in `config_resolve::resolve_repo_sandbox`, and a
/// second copy that drifted would silently stop matching the user's block.
pub fn workspace_slug(repo_root: &Path) -> String {
    let base = crate::util::slugify(&crate::repo::repo_name(repo_root));
    if base.is_empty() {
        "repo".to_string()
    } else {
        base
    }
}

/// An `Option`-per-field overlay of `[merge_queue]`, for the layers that refine
/// it per repo: `[workspace.<slug>] merge_queue` in the user's own config
/// (trusted) and a repo-root `.thegn.*` `[merge_queue]` (untrusted, clamped).
///
/// Every key in `[merge_queue]` describes a *repository* — its build tool, its
/// integration branch, its lockfiles — yet the table could only be set once for
/// every repo at once, so a repo that differed from the user's defaults could
/// only be handled with `--set` flags on every invocation.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MergeQueueOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_mode: Option<MergeRemoteMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_setup_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_on: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_reuse_worktree: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_target_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bisect_on_red: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_dirty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regenerate_paths: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub regenerate_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_handoff: Option<ConflictHandoff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_land: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organize_folders: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_folder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_landed: Option<OnLanded>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_folder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_folder: Option<String>,
    /// Merged **field-wise**, not wholesale: a repo that overrides only the
    /// conflict prompt keeps the global gate-failure one. (An empty string means
    /// "built-in", so a wholesale replace would silently discard the other key.)
    #[serde(skip_serializing_if = "MergeQueuePromptsOverlay::is_empty")]
    pub prompts: MergeQueuePromptsOverlay,
}

/// The `Option`-per-field overlay of `[merge_queue.prompts]`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct MergeQueuePromptsOverlay {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gate_failure: Option<String>,
}

impl MergeQueuePromptsOverlay {
    pub fn is_empty(&self) -> bool {
        self.conflict.is_none() && self.gate_failure.is_none()
    }

    /// Apply present fields onto `base`. Exhaustively destructured for the same
    /// reason [`MergeQueueOverlay::apply`] is.
    pub fn apply(self, base: &mut MergeQueuePrompts) {
        let MergeQueuePromptsOverlay {
            conflict,
            gate_failure,
        } = self;
        if let Some(v) = conflict {
            base.conflict = v;
        }
        if let Some(v) = gate_failure {
            base.gate_failure = v;
        }
    }
}

impl MergeQueueOverlay {
    /// True when nothing is set (lets the carrying struct skip serialization).
    pub fn is_empty(&self) -> bool {
        self.enabled.is_none()
            && self.remote_mode.is_none()
            && self.target_branch.is_none()
            && self.gate_command.is_none()
            && self.gate_setup_command.is_none()
            && self.gate_on.is_none()
            && self.gate_reuse_worktree.is_none()
            && self.gate_target_dir.is_none()
            && self.bisect_on_red.is_none()
            && self.snapshot_dirty.is_none()
            && self.regenerate_paths.is_none()
            && self.regenerate_command.is_none()
            && self.conflict_handoff.is_none()
            && self.agent_command.is_none()
            && self.agent.is_none()
            && self.auto_land.is_none()
            && self.agent_max_attempts.is_none()
            && self.agent_timeout_secs.is_none()
            && self.organize_folders.is_none()
            && self.queued_folder.is_none()
            && self.on_landed.is_none()
            && self.merged_folder.is_none()
            && self.failed_folder.is_none()
            && self.prompts.is_empty()
    }

    /// Apply present fields onto `base` (present wins, absent inherits).
    ///
    /// Destructured exhaustively on purpose — no `..`. A field added to
    /// `MergeQueueConfig` later must fail to compile here rather than be
    /// silently dropped from the per-repo layers, which is the same discipline
    /// `config_resolve::classify_repo_overlay` uses for the sandbox overlay.
    pub fn apply(self, base: &mut MergeQueueConfig) {
        let MergeQueueOverlay {
            enabled,
            remote_mode,
            target_branch,
            gate_command,
            gate_setup_command,
            gate_on,
            gate_reuse_worktree,
            gate_target_dir,
            bisect_on_red,
            snapshot_dirty,
            regenerate_paths,
            regenerate_command,
            conflict_handoff,
            agent_command,
            agent,
            auto_land,
            agent_max_attempts,
            agent_timeout_secs,
            organize_folders,
            queued_folder,
            on_landed,
            merged_folder,
            failed_folder,
            prompts,
        } = self;
        prompts.apply(&mut base.prompts);
        if let Some(v) = agent {
            base.agent = v;
        }
        if let Some(v) = enabled {
            base.enabled = v;
        }
        if let Some(v) = remote_mode {
            base.remote_mode = v;
        }
        if let Some(v) = target_branch {
            base.target_branch = v;
        }
        if let Some(v) = gate_command {
            base.gate_command = v;
        }
        if let Some(v) = gate_setup_command {
            base.gate_setup_command = v;
        }
        if let Some(v) = gate_on {
            base.gate_on = v;
        }
        if let Some(v) = gate_reuse_worktree {
            base.gate_reuse_worktree = v;
        }
        if let Some(v) = gate_target_dir {
            base.gate_target_dir = v;
        }
        if let Some(v) = bisect_on_red {
            base.bisect_on_red = v;
        }
        if let Some(v) = snapshot_dirty {
            base.snapshot_dirty = v;
        }
        if let Some(v) = regenerate_paths {
            base.regenerate_paths = v;
        }
        if let Some(v) = regenerate_command {
            base.regenerate_command = v;
        }
        if let Some(v) = conflict_handoff {
            base.conflict_handoff = v;
        }
        if let Some(v) = agent_command {
            base.agent_command = v;
        }
        if let Some(v) = auto_land {
            base.auto_land = v;
        }
        if let Some(v) = agent_max_attempts {
            base.agent_max_attempts = v;
        }
        if let Some(v) = agent_timeout_secs {
            base.agent_timeout_secs = v;
        }
        if let Some(v) = organize_folders {
            base.organize_folders = v;
        }
        if let Some(v) = queued_folder {
            base.queued_folder = v;
        }
        if let Some(v) = on_landed {
            base.on_landed = v;
        }
        if let Some(v) = merged_folder {
            base.merged_folder = v;
        }
        if let Some(v) = failed_folder {
            base.failed_folder = v;
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
    /// Mirror each pane's ring to `$XDG_STATE_HOME/thegn/replay/<session>/
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
    /// `[media] backend` — how thegn talks to your player. `"auto"` (the
    /// default) picks the right backend for the current OS: Linux → MPRIS,
    /// Windows → SMTC, macOS → AppleScript. `"none"` disables. `"mpris"` is the
    /// Linux D-Bus standard (native `zbus`, `playerctl` CLI fallback) covering
    /// Spotify desktop, mpv, ncspot, VLC, cmus, …; `"mpd"` (alias `"mpc"`) talks
    /// MPD directly (mpd/mpc/rmpc/ncmpcpp, no `mpd-mpris` bridge); `"mpv"` drives
    /// one mpv over JSON IPC. `"smtc"` = Windows SMTC; `"applescript"` = macOS
    /// Music/Spotify. `"jellyfin"` reserved. `"auto"` composes every source on the
    /// OS (Linux: MPRIS + MPD + mpv) and shows whatever is actually playing; a
    /// backend on the wrong OS is inert.
    pub enum MediaBackendKind: "media backend" {
        Auto = "auto",
        None = "none" | "off",
        Mpris = "mpris" | "dbus" | "playerctl",
        Mpv = "mpv",
        Mpd = "mpd" | "mpc",
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
    /// Seek step (seconds) applied by `media-seek-forward`/`-back` for audio.
    pub seek_step_secs: u64,
    /// Larger seek step (seconds) used when the loaded media is a video, where
    /// coarser skipping is the norm.
    pub seek_step_video_secs: u64,
    /// Render cover art in the Now-Playing overlay when the backend + terminal
    /// support it (kitty/sixel graphics; falls back to blocks otherwise).
    pub show_art: bool,
    /// Open the Now-Playing overlay when the statusbar media badge is clicked.
    pub overlay_on_badge_click: bool,
    /// Fallback poll cadence (seconds) for backends without a push-signal stream
    /// (mpv IPC / `playerctl`). The native MPRIS path uses D-Bus signals instead,
    /// so this never fires for it (preserving the ~0%-idle contract).
    pub poll_interval_secs: u64,
    pub mpv: MpvMediaConfig,
    pub mpd: MpdMediaConfig,
}

impl Default for MediaConfig {
    fn default() -> Self {
        MediaConfig {
            enabled: true,
            backend: MediaBackendKind::Auto,
            players_priority: Vec::new(),
            default_action: MediaDefaultAction::PlayPause,
            volume_step: 0.05,
            seek_step_secs: 10,
            seek_step_video_secs: 30,
            show_art: true,
            overlay_on_badge_click: true,
            poll_interval_secs: 3,
            mpv: MpvMediaConfig::default(),
            mpd: MpdMediaConfig::default(),
        }
    }
}

/// `[usage]` — the AI-account usage tracker (roadmap V 300). An opt-in detail
/// overlay (`open-usage`) that shows per-account rate-limit windows (session /
/// weekly / …) as usage bars, modeled on orca. Reads each harness's local state:
/// Codex from its rollout files (offline), Claude + Antigravity by reading the
/// locally-stored OAuth token and making a lightweight authenticated request —
/// the latter gated behind `allow_network` so the shell stays offline by default.
/// AI features are strictly additive; this never affects the AI-free shell.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct UsageConfig {
    /// Master switch. `false` ⇒ the `open-usage` overlay reports the feature off
    /// and gathers nothing.
    pub enabled: bool,
    /// Which harnesses to track (`"codex"`, `"claude"`, `"antigravity"`). Order is
    /// the display order; an unknown id is ignored.
    pub providers: Vec<String>,
    /// Allow the outward-facing live fetches (Claude `/api/oauth/usage`,
    /// Antigravity quota summary) using the on-disk OAuth token. `false` ⇒ only
    /// offline sources are used (Codex rollup files); Claude/Antigravity show
    /// "unavailable". Off by default — the live GET/POST is opt-in.
    pub allow_network: bool,
    /// Re-poll cadence (seconds) for a future live refresh while the overlay is
    /// open. v1 gathers once on open, so this is currently advisory.
    pub poll_interval_secs: u64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        UsageConfig {
            enabled: true,
            providers: vec!["codex".into(), "claude".into(), "antigravity".into()],
            allow_network: false,
            poll_interval_secs: 60,
        }
    }
}

// `MediaConfig`'s inherent impls (`resolve_opts`, `seek_step`) live in the
// `config_media` sibling module to keep this ratcheted god-file from growing.

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

// `[media.mpd]`'s `MpdMediaConfig` lives in the `config_media` sibling module to
// keep this ratcheted god-file from growing.
pub use crate::config_media::MpdMediaConfig;

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

// UI/presentation (`[ui]`) settings live in the `config_ui` sibling module;
// re-exported so `config::UiConfig` keeps working.
pub use crate::config_ui::{FocusDetail, TerminalsSection, UiConfig, WorkspaceSort};

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
    /// the canonical (primary) checkout from *inside* a thegn sandbox, where
    /// the canonical worktree's filesystem view can be incoherent and silently
    /// corrupt the merge. On by default; points at `thegn integrate` (an
    /// object-DB fold with no checkout). No-op outside a sandbox, in linked
    /// worktrees, and when a foreign hook is already present. Per-merge escape
    /// hatch: `THEGN_MERGE_GUARD_OFF=1`.
    pub merge_guard: bool,
    /// Poll the remote in the background (`git fetch --prune`, never a merge) so
    /// the sidebar's `↓N` badge and the panel header's `⇣N` reflect the remote
    /// without a manual fetch. On by default; the fetch is a pure read — it only
    /// writes `refs/remotes/…`, never the working tree. Skipped while offline.
    /// Set `false` to keep thegn fully hands-off.
    pub auto_fetch: bool,
    /// Background-fetch cadence in seconds. `0` disables the periodic poll while
    /// leaving the event-driven ones (startup, worktree switch) alive. Clamped to
    /// ≥ 30s — each poll is a network round trip per repo.
    pub auto_fetch_interval_secs: u64,
    /// Floor between two background fetches of the SAME repo, whatever triggered
    /// them (ticker, startup, switching to a worktree). Keeps rapid worktree
    /// switching from turning into a fetch storm — worktrees of one repo share
    /// an object store, so one fetch serves them all.
    pub auto_fetch_min_interval_secs: u64,
    /// Also raise an inbox notification when a background fetch finds new
    /// upstream commits on a worktree's branch. **Off by default**: the ambient
    /// surface for "you are behind" is the `↓N` badge on the sidebar row (and
    /// `⇣N` in the panel header), which the poll keeps current on its own — an
    /// interrupting notification for a routine upstream commit is noise. Opt in
    /// for an inbox entry; a transient toast additionally needs a
    /// `[[notifications.rules]]` with `route = ["inbox", "toast"]`.
    pub auto_fetch_notify: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            override_gpg: false,
            merge_guard: true,
            auto_fetch: true,
            auto_fetch_interval_secs: 300,
            auto_fetch_min_interval_secs: 60,
            auto_fetch_notify: false,
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
/// `profile` key (or `THEGN_PROFILE` / `--profile`). A profile may set the
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
    /// Named identity this profile resolves its credentials from
    /// (`[profiles.<p>] identity = "washu"` → `[identities.washu]`). Each tool
    /// (git config, git SSH key, `gh` config, GnuPG home, agent accounts) the
    /// identity sets overrides the profile-root default independently; tools it
    /// leaves unset fall back to `<profile_root>/…`. Empty ⇒ the profile-root
    /// paths (today's behavior). See [`crate::identity`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub identity: String,
}

/// Per-workspace config (`[workspace.<slug>.keybinds]`), keyed by repo slug.
/// Layered above the global + profile keybinds but below a repo-root
/// `.thegn.*` overlay. See [`Config::effective_keybinds`].
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
    /// any profile / repo-root `.thegn.*` overlay) for every worktree of
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
    /// Per-repo `[merge_queue]` refinements (`[workspace.<slug>.merge_queue]`).
    ///
    /// The TRUSTED per-repo layer: it lives in the user's own config, so unlike
    /// a repo-root `.thegn.*` it needs no clamping and never blocks a
    /// non-interactive drain. This is where a repo whose gate differs from your
    /// global default belongs (`gate_command = "pnpm test"`), and the only place
    /// the command-shaped keys can be set without an approval step.
    #[serde(skip_serializing_if = "MergeQueueOverlay::is_empty")]
    pub merge_queue: MergeQueueOverlay,
    /// Per-repo `[pr_queue]`. A repo's review rules, merge method, and base
    /// conventions are repository facts, exactly like the merge queue's gate.
    #[serde(skip_serializing_if = "PrQueueOverlay::is_empty")]
    pub pr_queue: PrQueueOverlay,
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
    /// Owning zone (`[bundle.<n>] zone = "clientA"`): a credential sub-vault only
    /// worktrees in that zone may compose. Empty ⇒ global. See [`crate::zone`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub zone: String,
    /// Named identity this bundle resolves credentials from (`[bundle.<n>]
    /// identity = "work"` → `[identities.work]`), folded before the bundle's own
    /// `env`/`config_dirs`/`accounts` (so the explicit fields override the
    /// referenced identity) and honoring the usual scope precedence. The named,
    /// reusable form of `config_dirs` + `accounts`. See [`crate::identity`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub identity: String,
}

/// A named, decoupled **identity** (`[identities.<name>]`) — a reusable set of
/// per-tool credential locations plus per-provider agent accounts, referenced by
/// name from a profile or a bundle. Every field is optional so an identity may
/// set some tools and leave others to fall through (mix-and-match: `git` from one
/// identity, `gh` from another, `gpg` shared). Resolution lives in
/// [`crate::identity`]; folding into a pane's env lives in [`crate::bundle`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IdentityConfig {
    /// git bindings (`git.config` → `GIT_CONFIG_GLOBAL`, `git.ssh_key` →
    /// `GIT_SSH_COMMAND` with `IdentitiesOnly=yes`).
    #[serde(skip_serializing_if = "IdentityGit::is_empty")]
    pub git: IdentityGit,
    /// GitHub/forge binding (`gh.config` → `GH_CONFIG_DIR`).
    #[serde(skip_serializing_if = "IdentityGh::is_empty")]
    pub gh: IdentityGh,
    /// GnuPG binding (`gpg.home` → `GNUPGHOME`).
    #[serde(skip_serializing_if = "IdentityGpg::is_empty")]
    pub gpg: IdentityGpg,
    /// Per-provider agent account selection (`accounts = { claude = "washu" }`),
    /// resolved through [`crate::account`] exactly like a bundle's `accounts`.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub accounts: std::collections::BTreeMap<String, String>,
}

/// git half of an [`IdentityConfig`] (`~` expanded at fold time).
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IdentityGit {
    /// Path to a git config file → `GIT_CONFIG_GLOBAL`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub config: String,
    /// Path to an SSH private key → `GIT_SSH_COMMAND -i <key> -o IdentitiesOnly=yes`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ssh_key: String,
}

impl IdentityGit {
    /// True when neither git binding is set (so serialization skips the table).
    pub fn is_empty(&self) -> bool {
        self.config.is_empty() && self.ssh_key.is_empty()
    }
}

/// GitHub/forge half of an [`IdentityConfig`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IdentityGh {
    /// Path to a `gh` config dir → `GH_CONFIG_DIR`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub config: String,
}

impl IdentityGh {
    /// True when the `gh` binding is unset.
    pub fn is_empty(&self) -> bool {
        self.config.is_empty()
    }
}

/// GnuPG half of an [`IdentityConfig`].
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct IdentityGpg {
    /// Path to a GnuPG home dir → `GNUPGHOME`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub home: String,
}

impl IdentityGpg {
    /// True when the GnuPG binding is unset.
    pub fn is_empty(&self) -> bool {
        self.home.is_empty()
    }
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
    /// Sidebar agent marker style: "letter" (universal), "symbol" (Nerd-Font),
    /// "auto" (symbols on confirmed-modern emulators only).
    pub agent_glyphs: AgentGlyphs,
    /// Loading-splash mascot: "owl" (default), "knight" (Sutton Hoo bust),
    /// "off". The sprite recolors with the active `preset`.
    pub mascot: MascotKind,
    /// Splash-mascot motion: "blink" (default; only redraws on wakes that
    /// already repaint — idle stays 0%) or "still".
    pub mascot_motion: MascotMotion,
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
            agent_glyphs: AgentGlyphs::Letter,
            mascot: MascotKind::Owl,
            mascot_motion: MascotMotion::Blink,
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
    /// Starting time window for the built-in monitor's graphs:
    /// `30s` / `2m` / `10m` / `1h` / `all`.
    pub default_window: String,
    /// Starting graph style: `area` / `line` / `spark`.
    pub default_style: String,
    /// Starting scale: `window` (relative to the visible max), `fixed` (the
    /// metric's natural full scale), or `log`.
    pub default_scale: String,
    /// Rows on the Processes tab.
    pub proc_rows: usize,
    /// Enable the Processes tab at all.
    ///
    /// Enumerating every process is the one genuinely expensive sample thegn
    /// takes, so it only runs while that tab is open — but this is the kill
    /// switch for a machine where even that is too much.
    pub processes: bool,
    /// Also read per-process disk IO. One extra `/proc/<pid>/io` open per
    /// process, frequently permission-denied for other users' processes, so it
    /// is opt-in.
    pub process_io: bool,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        MonitorConfig {
            system: "btm".into(),
            gpu: "nvtop".into(),
            default_window: "2m".into(),
            default_style: "area".into(),
            default_scale: "window".into(),
            proc_rows: 20,
            processes: true,
            process_io: false,
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
    /// `[stats.alerts]` — threshold alerts on the sampled metrics.
    pub alerts: StatsAlertsConfig,
}

/// `[stats.alerts]` — threshold alerts on system metrics.
///
/// Sits under `[stats]` rather than in its own table because `[stats]` already
/// owns `battery_warn` / `disk_free_warn` / `disk_free_critical`; splitting the
/// thresholds across two tables is how they drift apart.
/// [`StatsConfig::effective_alerts`] folds the two into one resolved set.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct StatsAlertsConfig {
    pub enabled: bool,
    /// Also record to the notification inbox. Off by default — an in-app toast
    /// is the right loudness for a resource threshold, and a pegged CPU during
    /// a build would otherwise fire a desktop notification every sample.
    pub notify: bool,
    /// Seconds past a threshold before firing, so a one-sample spike is silent.
    pub sustain_secs: u32,
    /// Minimum seconds between repeats of the same metric at the same level.
    pub repeat_secs: u32,
    /// Fractional retreat past the threshold required before an alert clears,
    /// so a value hovering on the line cannot flap.
    pub clear_margin: f32,
    /// Emit an event when an alert clears.
    pub notify_clear: bool,
    /// Percent. `0` disables that level.
    pub cpu: resource_alert::AlertRule,
    pub mem: resource_alert::AlertRule,
    pub swap: resource_alert::AlertRule,
    pub gpu: resource_alert::AlertRule,
    /// Degrees Celsius.
    pub temp: resource_alert::AlertRule,
    /// Load average per core (1.0 == fully loaded). Unix only.
    pub load: resource_alert::AlertRule,
    /// Percent free, fires when falling BELOW. Left at 0, inherits
    /// `disk_free_warn` / `disk_free_critical`.
    pub disk_free: resource_alert::AlertRule,
    /// Percent charge, fires when falling BELOW. Left at 0, inherits
    /// `battery_warn`.
    pub battery: resource_alert::AlertRule,
}

impl Default for StatsAlertsConfig {
    fn default() -> Self {
        StatsAlertsConfig {
            enabled: true,
            notify: false,
            sustain_secs: 15,
            repeat_secs: 900,
            clear_margin: 0.05,
            notify_clear: false,
            cpu: rule(90.0, 98.0),
            mem: rule(85.0, 95.0),
            swap: rule(50.0, 80.0),
            gpu: rule(0.0, 0.0),
            temp: rule(85.0, 95.0),
            load: rule(0.0, 0.0),
            // Zero = inherit the `[stats]` widget-coloring keys; see
            // `effective_alerts`.
            disk_free: rule(0.0, 0.0),
            battery: rule(0.0, 0.0),
        }
    }
}

const fn rule(warn: f32, critical: f32) -> resource_alert::AlertRule {
    resource_alert::AlertRule { warn, critical }
}

impl StatsConfig {
    /// Fold `[stats.alerts]` with the widget-coloring keys into one resolved
    /// set: a zero `disk_free` / `battery` rule inherits `disk_free_warn` /
    /// `disk_free_critical` / `battery_warn`, so those thresholds have exactly
    /// one place to be set and cannot disagree with the bar colors.
    pub fn effective_alerts(&self) -> resource_alert::ResolvedAlerts {
        use resource_alert::AlertMetric as M;
        let a = &self.alerts;
        let mut out = resource_alert::ResolvedAlerts {
            enabled: a.enabled,
            notify: a.notify,
            sustain_secs: a.sustain_secs,
            repeat_secs: a.repeat_secs,
            clear_margin: a.clear_margin,
            notify_clear: a.notify_clear,
            ..Default::default()
        };
        out.set(M::Cpu, a.cpu);
        out.set(M::Mem, a.mem);
        out.set(M::Swap, a.swap);
        out.set(M::Gpu, a.gpu);
        out.set(M::Temp, a.temp);
        out.set(M::Load, a.load);
        // An explicitly-set rule wins; an all-zero one inherits.
        out.set(
            M::DiskFree,
            if a.disk_free == rule(0.0, 0.0) {
                rule(
                    f32::from(self.disk_free_warn),
                    f32::from(self.disk_free_critical),
                )
            } else {
                a.disk_free
            },
        );
        out.set(
            M::Battery,
            if a.battery == rule(0.0, 0.0) {
                rule(f32::from(self.battery_warn), 0.0)
            } else {
                a.battery
            },
        );
        out
    }
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
            alerts: StatsAlertsConfig::default(),
        }
    }
}

/// `[bars]` — the customizable widget bars framing the workspace. Each slot is
/// an ordered widget-id list; unknown ids warn and are skipped. Built-ins:
/// `brand` (thegn + version), `cpu`, `mem`, `gpu`, `temp` (CPU °C), `net`,
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
            // `help` is the clickable `?` chip — the one always-visible pointer
            // at the help system. Drop it from the list to hide it.
            bottom_left: vec!["help".into(), "keyhints".into()],
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

/// `[limits]` — resource ceilings for thegn-spawned `cargo` test/discovery
/// runs. When `systemd-run` is available, an explicit run executes in a
/// transient `--user --scope` with these caps so a heavy suite can't pin the
/// machine or trigger a global OOM that takes the terminal session down; the
/// scope teardown on exit also reaps orphaned children. (The yazi files-drawer
/// has its own containment under `[drawer]`.)
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LimitsConfig {
    /// `CPUQuota` for test/discovery runs (e.g. "150%" = 1.5 cores). Empty =
    /// uncapped. thegn never auto-runs tests, but an explicit run is still
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
    /// Run thegn-spawned `cargo` (test + discovery) under a private
    /// `CARGO_TARGET_DIR` (`<worktree>/target/thegn`) so it never blocks on
    /// the build-directory lock held by the user's own `cargo`/rust-analyzer.
    /// Costs a separate artifact cache; set false to share the default `target`.
    pub isolated_target_dir: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        LimitsConfig {
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

/// `[session]` — snapshot/restore hardening plus worktree-create UX: scrollback
/// capture/repaint, restore-time stale-agent-dot grace, jump-to-new-worktree.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SessionConfig {
    /// Max plain-text scrollback lines captured per pane on snapshot and
    /// repainted on restore. 0 disables scrollback capture (panes restore blank,
    /// exactly the pre-feature behavior).
    pub scrollback_lines: u32,
    /// Restore-time stale-state grace (seconds). A persisted `active`/`running`
    /// activity dot whose last live signal is older than this at resurrection is
    /// downgraded to a settled state. Applied once at restore; the live activity
    /// FSM is untouched.
    pub restore_grace_secs: u64,
    /// Whether creating a worktree (Alt+w) jumps to its new tab vs. background.
    pub focus_on_create: bool,
    /// Max **parked** workspaces kept resident (panes alive) for instant
    /// switch-back, beyond the active one. Past this the least-recently-used
    /// parked workspace is evicted and its panes reaped (it re-resurrects on the
    /// next visit), bounding PTY fd/thread growth over a long multi-workspace
    /// session. `0` = no pooling (reap on every switch); default is generous.
    pub resident_pool_limit: usize,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            scrollback_lines: 500,
            restore_grace_secs: 600,
            focus_on_create: true,
            resident_pool_limit: 16,
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

// The `[ci]` provider config family lives in the `config_ci` sibling module
// (file-size ratchet); re-exported so `crate::config::*` paths are unchanged.
pub use crate::config_ci::{
    ArgoCiConfig, CiConfig, CiProviderKind, DroneCiConfig, GitLabCiConfig, JenkinsCiConfig,
    WoodpeckerCiConfig,
};

pub use crate::config_forge::{ForgeConfig, ForgeKind};
pub use crate::config_issues::{
    GitHubIssuesConfig, IssueAccount, IssueProviderKind, IssuesConfig, JiraConfig, LinearConfig,
};

pub use crate::config_calendar::{
    CalendarAccount, CalendarConfig, CalendarProviderKind, TimeFormat, WeekStart, WorldClock,
};

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
/// sink under `dir` is opt-in. `THEGN_LOG` (env) is a `tracing`-style filter
/// that overrides `level` per-module.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct LogConfig {
    pub level: LogLevel,
    /// Mirror diagnostics to a rotating file under `dir`.
    pub file: bool,
    /// Log directory ("" => `$XDG_STATE_HOME/thegn/logs`). Tilde-expanded.
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
    /// The resolved log directory (default under `$XDG_STATE_HOME/thegn`).
    pub fn dir_path(&self) -> PathBuf {
        if self.dir.trim().is_empty() {
            util::xdg_state_home().join("thegn").join("logs")
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
            remote_dir: "~/thegn-worktrees".into(),
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
/// workspace/repo/worktree (DB `env_name`, repo `.thegn.*` `env =`, or the
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
    /// `[host.<name>]` this env lands on (host lifecycle is paid once and
    /// shared across every env/worktree that references it). Empty ⇒ derived
    /// from the inline ssh/provider tables. See [`crate::host_config`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub host: String,
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
    /// Per-env override of `[sandbox] failover`; `None` ⇒ inherit.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_failover_opt"
    )]
    pub failover: Option<FailoverMode>,
    /// Requested placement class (`None` ⇒ inherit `[placement] mode`); clamped by the resolved mode floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement_mode: Option<PlacementModePref>,
    /// `[env.<name>.resources]` — the declared resource ask the placement
    /// engine reserves (fields fall back to `[placement.default_resources]`).
    #[serde(skip_serializing_if = "ResourcesDecl::is_empty")]
    pub resources: ResourcesDecl,
}

pub use crate::config_env_tables::{
    EnvK8sConfig, EnvProviderConfig, EnvSshConfig, MetricsConfig, MetricsTarget, NixInstaller,
    ProviderConnect, ProviderExecMode, provider_scale_to_zero, provider_self_suspends,
    ssh_reached_provider_kind, vps_provider_kind,
};

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

pub use crate::config_sandbox::SandboxLimits;

// The `[sandbox.vpn]` config family lives in the `config_vpn` sibling module
// (file-size ratchet); re-exported so `crate::config::*` paths are unchanged.
pub use crate::config_vpn::{
    CustomVpnConfig, NetbirdConfig, OpenvpnConfig, TailscaleConfig, VpnConfig, VpnDnsMode, VpnMode,
    VpnOnError, VpnProviderKind, WireguardConfig, ZerotierConfig,
};

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
    /// them to the host. When `false`, only an explicit `thegn forward` acts.
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
    /// Name of the `[env.<name>]` env to default to for a worktree on the repo's
    /// PRIMARY branch (origin/HEAD, e.g. `main`/`master`), when it hasn't selected
    /// one explicitly. Empty ⇒ the branch is never consulted and `default_env`
    /// applies to every branch (today's behavior). Set both (e.g. `main_env =
    /// "host"`, `default_env = "machine0"`) to run the main checkout locally and
    /// feature branches on a remote provider. See [`Config::resolve_env`].
    pub main_env: String,
    pub backend_chain: Vec<String>, // auto detection order; "host" = host fallback
    pub image: String,              // "" => host-toolchain mode
    /// Hardening preset for the worktree's interactive container (shell panes).
    pub profile: SandboxProfile,
    pub network: Network,
    pub file_access: FileAccess,
    pub ports: Vec<String>, // e.g. ["8080:8080"]
    pub gpu: Option<String>,
    pub limits: SandboxLimits,
    pub volumes: std::collections::HashMap<String, String>,
    pub compose: Option<String>,
    /// Programmatic Dockerfile build (set by the devcontainer overlay from a
    /// `build` block; never from TOML). When present, the sandbox builds the
    /// image to `image` before create instead of pulling. See [`crate::sandbox_build`].
    #[serde(skip)]
    pub build: Option<crate::sandbox_build::SandboxBuild>,
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
    /// exported vars) into worktree panes — resolved on the host and cached, so a
    /// sandboxed pane that can't reach the Nix daemon still gets the project
    /// tools. No-op without a flake `devShell`. See [`crate::devenv`].
    pub inject_devshell: bool,
    /// Which flake devShell attribute a sandbox/sprite enters, e.g. `"sandbox"`
    /// for a lean build-only shell (`.#devShells.sandbox`). Empty ⇒ `default`.
    /// Exported as `THEGN_DEVSHELL` into the sandbox, which the repo `.envrc`
    /// reads to `use flake ".#$THEGN_DEVSHELL"` — a smaller closure than full
    /// host dev (the host, with it unset, uses devenv instead).
    pub devshell: String,
    /// Bind-mount the host Nix daemon socket into the sandbox for full in-sandbox
    /// `nix develop`/`build`/`fmt`. `true` forces it on; `false` still auto-enables
    /// it as a backstop for a local flake `.envrc` — off: `warm_direnv=off`/sealed.
    pub nix_daemon: bool,
    /// Shell to use inside the sandbox. `""` = resolve from the host's `$SHELL`
    /// at pane-spawn time; else an absolute path or name (e.g. `"zsh"`).
    pub shell: String,
    pub on_missing: OnMissing,
    /// When a *selected* non-local env can't be brought up: `halt` (default,
    /// blocks+warns), `ask` (prompt: retry/run-on-host), `auto` (silent host
    /// fallback). Legacy bool: `true`⇒`auto`, `false`⇒`halt`. Per-env overridable.
    #[serde(deserialize_with = "de_failover")]
    pub failover: FailoverMode,
    /// Drive the OCI runtime against a **remote daemon** instead of SSH-wrapping
    /// the whole backend argv: a podman connection URL/name or a docker host
    /// (e.g. `ssh://user@host`). Empty ⇒ local daemon (the default). Injected as
    /// `podman --url`/`--connection` or `docker -H` before every container
    /// subcommand. An alternative reach to `[env.<name>] placement = "ssh"`,
    /// useful when the daemon (not just the shell) lives on another box.
    pub oci_host: String,
    /// OCI runtime for worktree containers (`--runtime`): `"runsc"` (gVisor) / `"krun"`
    /// (libkrun microVM); empty ⇒ default. OCI backends only. See config.toml.example.
    pub oci_runtime: String,
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
            main_env: String::new(),
            backend_chain: crate::config_defaults::default_backend_chain(),
            image: String::new(),
            profile: SandboxProfile::Hardened,
            network: Network::Nat,
            file_access: FileAccess::default(),
            ports: Vec::new(),
            gpu: None,
            limits: SandboxLimits::default(),
            volumes: std::collections::HashMap::new(),
            compose: None,
            build: None,
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
            failover: FailoverMode::Halt,
            oci_host: String::new(),
            oci_runtime: String::new(),
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
        // The repo `.envrc` reads `THEGN_DEVSHELL` (`use flake ".#$THEGN_DEVSHELL"`),
        // so a fresh sprite enters the lean build shell instead of the full dev
        // closure. Unset on the host → devenv (the host default), unchanged here.
        let attr = self.devshell.trim();
        if !attr.is_empty() {
            env.push(("THEGN_DEVSHELL".to_string(), attr.to_string()));
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
/// description of *your* setup (independent of any repo) that thegn reproduces
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

/// Partial overlay deserialized from a repo-root `.thegn.{toml,yaml,yml,json}`
/// — only the keys present override the global `[sandbox]`. Also reused for the
/// `THEGN_SANDBOX_*` env layer.
#[derive(Debug, Clone, Default, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct SandboxOverlay {
    pub enabled: Option<bool>,
    pub backend: Option<SandboxBackend>,
    pub default_backend: Option<SandboxBackend>,
    pub default_env: Option<String>,
    pub main_env: Option<String>,
    pub backend_chain: Option<Vec<String>>,
    pub image: Option<String>,
    pub profile: Option<SandboxProfile>,
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
    /// Per-env override of `[sandbox] devshell` (the flake devShell attr a
    /// sandbox/sprite enters, e.g. `"sprite-full"`), via `THEGN_DEVSHELL`.
    pub devshell: Option<String>,
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

impl RemoteOverlay {
    pub(crate) fn apply(self, base: &mut RemoteConfig) {
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

/// The shape of a repo-root `.thegn.*` file: a `[sandbox]` table overlay
/// plus an optional `[keybinds]` table (the most-specific keybind layer).
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub(crate) struct RepoConfigFile {
    pub(crate) sandbox: SandboxOverlay,
    keybinds: KeybindConfig,
    /// Per-repo notification routing overlay, applied on top of global +
    /// profile (see [`Config::effective_notifications`]).
    #[serde(default)]
    notifications: NotificationsOverlay,
    /// Per-repo issue-tracker overlay (Linear team / Jira project) that scopes
    /// this repo's "My Work" feed (see [`Config::repo_issues`]).
    #[serde(default)]
    issues: crate::config_issues::IssuesOverlay,
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
    /// File manager to run. Empty ⇒ the pinned yazi (`THEGN_YAZI_BIN`).
    pub command: String,
    /// `YAZI_CONFIG_HOME` for the drawer's yazi. Empty (default) ⇒ a private
    /// `<thegn-dir>/yazi`, fully separate from the user's `~/.config/yazi` and
    /// seeded with thegn's bundled config. "system" (or "none") ⇒ use the
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
    /// `config_home` (thegn never edits the user's own yazi config).
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
    /// Whether the active worktree's yazi drawer is prewarmed (spawned hidden in
    /// the pool) before the user opens it, so even the first open is instant. On
    /// by default; set false to never spawn an unopened yazi.
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
            prewarm: true,
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
    /// Surface thegn's own log errors as user notifications (dev flag; off by default, stays quiet Info).
    #[serde(skip_serializing_if = "is_false")]
    pub surface_self_log_errors: bool,
    /// Poll GitHub's notifications API for @mentions in the active repo
    /// (`gh api notifications`, reason == `mention`), throttled to one call
    /// per repo per 5 minutes on the PR-refresh cadence. Feeds the
    /// `mentioned` notification kind. On by default; requires `gh` auth.
    pub github_mentions: bool,
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
            surface_self_log_errors: false,
            github_mentions: true,
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

    /// True when routing rules are present.
    ///
    /// NOTE: the sidebar unread/alert badge counts currently always use the
    /// SQL kind-level queries (`get_unread_counts_by_worktree` /
    /// `get_alert_counts_by_worktree`), which apply each kind's DEFAULT
    /// priority — a rule's `set_priority` override does not reach those two
    /// badges. The rule-aware aggregation this predicate was meant to gate
    /// was never wired up; if it is, this is the switch.
    pub fn has_rules(&self) -> bool {
        !self.rules.is_empty()
    }
}

/// serde `skip_serializing_if` helper for `bool` fields (skips `false`).
fn is_false(b: &bool) -> bool {
    !*b
}

/// serde `skip_serializing_if` helper for `bool` fields (skips `true`).
fn is_true(b: &bool) -> bool {
    *b
}

config_enum! {
    /// `[notifications.sound] mode` — how the audible cue is produced. `chime`
    /// (default) plays a bundled sound through an auto-detected system player
    /// (falling back to the terminal `BEL`); `bell` writes a terminal `BEL` on
    /// the next render flush; `command` runs a configured command off-thread;
    /// `off` is silent.
    pub enum SoundMode: "notification sound mode" {
        Off = "off" | "none" | "silent",
        Chime = "chime" | "sound",
        Bell = "bell" | "beep" | "terminal",
        Command = "command" | "cmd" | "exec",
    } default = Chime;
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
    /// How to produce the cue: `chime` (default), `bell`, `command`, or `off`.
    pub mode: SoundMode,
    /// Minimum effective priority that makes a sound (`"info"`/`"notice"`/
    /// `"alert"`, default `"alert"`). Kinds in `always_kinds` bypass this floor.
    pub min_priority: String,
    /// Notification kinds that always chime regardless of `min_priority`
    /// (snake_case, e.g. `"agent_done"`). Defaults to the agent
    /// finished/needs-you set so a background worktree is audible out of the box.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub always_kinds: Vec<String>,
    /// Suppress the audible cue for the currently-focused worktree (you can
    /// already see it) — the inbox record + desktop toast still fire. Default
    /// `true`.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub suppress_focused: bool,
    /// Optional custom sound file for `mode = "chime"` (a `.wav` path). Empty ⇒
    /// the bundled chime. The auto-detected player still applies.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub chime_file: String,
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
            mode: SoundMode::Chime,
            min_priority: "alert".into(),
            always_kinds: vec![
                "agent_done".into(),
                "agent_attention".into(),
                "agent_failed".into(),
            ],
            suppress_focused: true,
            chime_file: String::new(),
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
    pub surface_self_log_errors: Option<bool>,
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
            && self.surface_self_log_errors.is_none()
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
        if let Some(v) = self.surface_self_log_errors {
            base.surface_self_log_errors = v;
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
    /// Maximum number of fuzzy-matched results returned per search. Capped at
    /// the UI renderer's visible row count; higher values are just sorted but
    /// not all drawn.
    pub max_results: usize,
}

impl Default for SearchConfig {
    fn default() -> Self {
        SearchConfig { max_results: 1_000 }
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
    /// Resting (Normal) panel width in columns. Unset = the built-in default
    /// (44). Clamped to 30–200 at apply time; the layout still shrinks it
    /// when the screen can't afford it.
    pub width: Option<usize>,
    /// Fraction of the window the Half width claims. Unset = 0.5. Clamped to
    /// 0.3–0.9 at apply time; never below the resting width.
    pub half_ratio: Option<f32>,
}

impl Default for PanelConfig {
    fn default() -> Self {
        Self {
            sections: Vec::new(),
            collapse_on_escape: true,
            width: None,
            half_ratio: None,
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

pub use crate::config_env_tables::{EagerScope, LifecycleConfig, PoolConfig};
pub use crate::config_observe::{LokiSourceConfig, ObserveConfig, PrometheusSourceConfig};
pub use crate::config_placement::{
    OnExhaustion, PackStrategy, PlacementConfig, PlacementModePref, ResourcesDecl,
};
pub use crate::config_pr_queue::{
    PrAutoEnqueue, PrMergeMethod, PrMergeMode, PrQueueConfig, PrQueueOverlay, PrQueuePrompts,
    PrQueuePromptsOverlay, PrWatchKind,
};

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
    /// overridden by `THEGN_PROFILE` / `--profile`.
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
    /// Named git forges (`[[forges]]`); see [`crate::config_forge`].
    pub forges: Vec<ForgeConfig>,
    // --- sub-tables ---
    #[serde(default)]
    pub ui: UiConfig,
    pub git: GitConfig,
    pub theme: ThemeConfig,
    pub monitor: MonitorConfig,
    pub stats: StatsConfig,
    pub metrics: MetricsConfig,
    pub apps: AppsConfig,
    pub observe: ObserveConfig,
    pub bars: BarsConfig,
    /// `[calendar]` — the month popup, its world clocks, and its event sources.
    pub calendar: CalendarConfig,
    pub pr: PrConfig,
    pub issues: IssuesConfig,
    /// `[ci]` — cross-provider CI/CD inspection (AV group).
    pub ci: CiConfig,
    pub watch: WatchConfig,
    pub log: LogConfig,
    pub sandbox: SandboxConfig,
    /// `[toolchain]` — the batteries-included toolchain for languages-only
    /// repos (synthesized Nix devShell; mode + per-language package overrides).
    pub toolchain: crate::toolchain::ToolchainConfig,
    pub limits: LimitsConfig,
    /// `[disk]` — disk-usage visibility, cleanup, and shared build caches.
    pub disk: DiskConfig,
    /// `[session]` — scrollback capture + restore-time stale-state grace.
    pub session: SessionConfig,
    pub drawer: DrawerConfig,
    pub notifications: NotificationsConfig,
    pub strip: StripConfig,
    pub panel: PanelConfig,
    pub search: SearchConfig,
    pub palette: PaletteConfig,
    pub lsp: LspConfig,
    /// `[daemon]` — the pane daemon (center panes survive UI exit; tmux
    /// semantics). On by default; set `[daemon] enabled = false` for plain
    /// in-process PTYs.
    pub daemon: DaemonConfig,
    /// `[serve]` — remote thin-client serving + pairing policy (`thegn serve`).
    pub serve: ServeConfig,
    /// `[merge_queue]` — the local fold-actor (parallel-branch integration).
    /// On by default; the core is AI-free (agent handoff only fires on conflict).
    /// The RAW global `[merge_queue]` table.
    ///
    /// Do NOT read this on a repo-scoped path — use
    /// [`Config::repo_merge_queue`], or the `[workspace.<slug>] merge_queue`
    /// layer silently does nothing there. Reading it directly is correct only
    /// where no repo is in scope (a global enable check for command/palette
    /// registration, or an enqueue being forwarded to another host before a repo
    /// root exists).
    pub merge_queue: MergeQueueConfig,
    /// `[pr_queue]` — watch queued pull requests on the forge, optionally fix
    /// blockers with an agent, and merge them once green. Off by default (it is
    /// the one part of the shell that makes network writes).
    pub pr_queue: PrQueueConfig,
    /// `[replay]` — per-pane time-travel recording + scrub/search (`Alt+r`). On
    /// by default, bounded 8 MiB / 30 m per pane; free when disabled.
    pub replay: ReplayConfig,
    /// `[media]` — media-player control. On by default (`mpris` backend), inert
    /// where D-Bus/`playerctl` are absent. Additive — the shell never depends on it.
    pub media: MediaConfig,
    /// `[usage]` — the AI-account usage tracker overlay (`open-usage`). Opt-in,
    /// additive; the shell never depends on it. See [`UsageConfig`].
    pub usage: UsageConfig,
    /// `[remote]` — ssh keepalive/retry/heal tuning. See [`crate::config_remote`].
    pub remote: crate::config_remote::RemoteConfig,
    /// `[network]` — offline-detection policy. See [`crate::config_network`].
    pub network: crate::config_network::NetworkConfig,
    /// `[share]` — expose a worktree port at a public URL. Disabled by default.
    pub share: ShareConfig,
    /// `[forward]` — auto-forward sandbox-internal dev-server ports to the host's
    /// loopback for browser preview. On by default (loopback-only ⇒ safe).
    pub forward: ForwardConfig,
    /// `[lifecycle]` — budget-governed warm/suspend policy for managed-provider
    /// sandboxes (keep recently-used ones warm for fast resume; let idle ones
    /// suspend; provision ahead of focus). Budget-safe defaults.
    pub lifecycle: LifecycleConfig,
    /// `[placement]` — the multi-host placement engine (dedicated / packed /
    /// autoscale). Off by default; strictly additive. See
    /// [`crate::config_placement`].
    pub placement: PlacementConfig,
    /// Rebind a built-in action by id, e.g. `new-worktree = "Ctrl w"`. The flat
    /// table is the global/default layer; nested mode tables are native-host only.
    pub keybinds: KeybindConfig,
    /// Named keybind profiles (`[profiles.<name>]`), selected by `profile`.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub profiles: std::collections::BTreeMap<String, ProfileConfig>,
    /// Optional seed for the profile-switcher order (`profile_order =
    /// ["personal", "washu"]`). Consulted only when the shared, never-rerooted
    /// `~/.config/thegn/profiles-order.json` sidecar is absent; once the user
    /// reorders in the switcher the sidecar is authoritative. Names not listed
    /// here (or in the sidecar) are appended alphabetically. See
    /// [`crate::profile::order`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub profile_order: Vec<String>,
    /// Named, decoupled per-tool identities (`[identities.<name>]`) — a reusable
    /// set of git/gh/gpg/ssh credential locations + per-provider accounts,
    /// referenced by name from a profile (`[profiles.<p>] identity = "…"`) or a
    /// bundle (`[bundle.<n>] identity = "…"`), each tool assignable
    /// independently (mix-and-match). See [`crate::identity`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub identities: std::collections::BTreeMap<String, IdentityConfig>,
    /// Per-workspace config keyed by repo slug (`[workspace.<slug>]`).
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub workspace: std::collections::BTreeMap<String, WorkspaceConfig>,
    /// Named execution environments (`[env.<name>]`) — the reusable library a
    /// workspace/repo/worktree selects from. See [`Config::resolve_env`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, EnvConfig>,
    /// Container-capable machines (`[host.<name>]`) provisioned once and shared
    /// by every env that references them. Global config only — never the repo
    /// overlay. See [`crate::host_config`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub host: std::collections::BTreeMap<String, crate::host_config::HostConfig>,
    /// Named environment bundles (`[bundle.<name>]`) — soft work/personal
    /// identities bound per scope and injected at every pane spawn. See
    /// [`crate::bundle`].
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub bundle: std::collections::BTreeMap<String, Bundle>,
    /// Zone policy (`[zone.<name>]`) — egress/budget ceilings + bundle binding.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub zone: std::collections::BTreeMap<String, crate::zone::ZoneConfig>,
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
            // Under thegn's root (honors THEGN_DIR) so a dev/test instance
            // gets its own worktrees, isolated from the daily-driver instance.
            worktrees_dir: util::thegn_dir()
                .join("worktrees")
                .to_string_lossy()
                .into_owned(),
            workspaces_dir: home.join("code").to_string_lossy().into_owned(),
            base_branch: "auto".into(),
            window_margin: 0,
            branch_prefix: "tg/".into(),
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
            forges: Vec::new(),
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
            observe: ObserveConfig::default(),
            bars: BarsConfig::default(),
            calendar: CalendarConfig::default(),
            pr: PrConfig::default(),
            issues: IssuesConfig::default(),
            ci: CiConfig::default(),
            watch: WatchConfig::default(),
            log: LogConfig::default(),
            sandbox: SandboxConfig::default(),
            toolchain: crate::toolchain::ToolchainConfig::default(),
            limits: LimitsConfig::default(),
            disk: DiskConfig::default(),
            session: SessionConfig::default(),
            drawer: DrawerConfig::default(),
            notifications: NotificationsConfig::default(),
            strip: StripConfig::default(),
            panel: PanelConfig::default(),
            search: SearchConfig::default(),
            palette: PaletteConfig::default(),
            lsp: LspConfig::default(),
            daemon: DaemonConfig::default(),
            serve: ServeConfig::default(),
            merge_queue: MergeQueueConfig::default(),
            pr_queue: PrQueueConfig::default(),
            replay: ReplayConfig::default(),
            media: MediaConfig::default(),
            usage: UsageConfig::default(),
            remote: crate::config_remote::RemoteConfig::default(),
            network: crate::config_network::NetworkConfig::default(),
            share: ShareConfig::default(),
            forward: ForwardConfig::default(),
            lifecycle: LifecycleConfig::default(),
            placement: PlacementConfig::default(),
            keybinds: KeybindConfig::default(),
            actions: Vec::new(),
            profile: String::new(),
            keymap_preset: default_preset(),
            profiles: std::collections::BTreeMap::new(),
            profile_order: Vec::new(),
            identities: std::collections::BTreeMap::new(),
            workspace: std::collections::BTreeMap::new(),
            env: std::collections::BTreeMap::new(),
            host: std::collections::BTreeMap::new(),
            bundle: std::collections::BTreeMap::new(),
            zone: std::collections::BTreeMap::new(),
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
    pub theme_agent_glyphs: Option<AgentGlyphs>,
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
    pub(crate) fn apply(self, base: &mut Config) {
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
        set!(base.theme.agent_glyphs, self.theme_agent_glyphs);
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

/// Read the `THEGN_<SECTION>_<KEY>` env layer. Each knob is one line here —
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

    o.worktrees_dir = env.get("THEGN_WORKTREES_DIR");
    o.workspaces_dir = env.get("THEGN_WORKSPACES_DIR");
    o.base_branch = env.get("THEGN_BASE_BRANCH");
    o.branch_prefix = env.get("THEGN_BRANCH_PREFIX");
    if let Some(v) = env.get("THEGN_PICKER") {
        o.picker = Picker::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_WORKTREE_MODE") {
        o.worktree_mode = WorktreeMode::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_NAME_SCHEME") {
        o.name_scheme = NameScheme::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_AUTO_REMOVE_WORKTREE") {
        o.auto_remove_worktree = parse_bool(&v, "THEGN_AUTO_REMOVE_WORKTREE");
    }
    if let Some(v) = env.get("THEGN_REPO_SCAN_DEPTH") {
        o.repo_scan_depth = parse_num(v, "THEGN_REPO_SCAN_DEPTH").map(|n| n as usize);
    }
    o.profile = env.get("THEGN_PROFILE");
    o.accent = env.get("THEGN_THEME_ACCENT");
    o.focus_border = env.get("THEGN_THEME_FOCUS_BORDER");
    o.frame_border = env.get("THEGN_THEME_BORDER");
    if let Some(v) = env.get("THEGN_THEME_COLOR") {
        o.theme_color = ColorMode::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_THEME_GLYPHS") {
        o.theme_glyphs = GlyphMode::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_THEME_AGENT_GLYPHS") {
        o.theme_agent_glyphs = AgentGlyphs::from_str_validated(v.trim()).ok();
    }

    // [pr] — THEGN_PR_TTL, with deprecated TG_PR_TTL fallback.
    if let Some(v) = env.get("THEGN_PR_TTL") {
        o.pr_ttl_secs = parse_num(v, "THEGN_PR_TTL");
    } else if let Some(v) = env.get("TG_PR_TTL") {
        config_warn("TG_PR_TTL is deprecated; use THEGN_PR_TTL");
        o.pr_ttl_secs = parse_num(v, "TG_PR_TTL");
    }
    if let Some(v) = env.get("THEGN_WATCH_PR_INTERVAL") {
        o.watch_pr_interval_secs = parse_num(v, "THEGN_WATCH_PR_INTERVAL");
    }

    // [metrics]
    if let Some(v) = env.get("THEGN_METRICS_INTERVAL_SECS") {
        o.metrics_interval_secs = parse_float(v, "THEGN_METRICS_INTERVAL_SECS");
    }
    if let Some(v) = env.get("THEGN_METRICS_TIMEOUT_MS") {
        o.metrics_timeout_ms = parse_num(v, "THEGN_METRICS_TIMEOUT_MS");
    }
    if let Some(v) = env.get("THEGN_METRICS_MAX_BODY_BYTES") {
        o.metrics_max_body_bytes = parse_num(v, "THEGN_METRICS_MAX_BODY_BYTES").map(|n| n as usize);
    }

    // [apps]
    o.apps_default_tab = env.get("THEGN_APPS_DEFAULT_TAB");
    if let Some(v) = env.get("THEGN_APPS_TAB_ORDER") {
        o.apps_tab_order = Some(parse_list(v));
    }

    // [log]
    if let Some(v) = env.get("THEGN_LOG_LEVEL") {
        o.log_level = LogLevel::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_LOG_FILE") {
        o.log_file = parse_bool(&v, "THEGN_LOG_FILE");
    }
    o.log_dir = env.get("THEGN_LOG_DIR");
    if let Some(v) = env.get("THEGN_LOG_ROTATION_SIZE_MB") {
        o.log_rotation_size_mb = parse_num(v, "THEGN_LOG_ROTATION_SIZE_MB");
    }
    if let Some(v) = env.get("THEGN_LOG_MAX_FILES") {
        o.log_max_files = parse_num(v, "THEGN_LOG_MAX_FILES").map(|n| n as usize);
    }
    if let Some(v) = env.get("THEGN_LOG_FORMAT") {
        o.log_format = LogFormat::from_str_validated(v.trim()).ok();
    }

    // [disk]
    if let Some(v) = env.get("THEGN_DISK_SHOW_SIZES") {
        o.disk_show_sizes = parse_bool(&v, "THEGN_DISK_SHOW_SIZES");
    }
    if let Some(v) = env.get("THEGN_DISK_WARN_THRESHOLD_GB") {
        o.disk_warn_threshold_gb = parse_num(v, "THEGN_DISK_WARN_THRESHOLD_GB");
    }
    if let Some(v) = env.get("THEGN_DISK_SCAN_INTERVAL_SECS") {
        o.disk_scan_interval_secs = parse_num(v, "THEGN_DISK_SCAN_INTERVAL_SECS");
    }
    if let Some(v) = env.get("THEGN_DISK_AUTO_CLEAN_ON_MERGE") {
        o.disk_auto_clean_on_merge = parse_bool(&v, "THEGN_DISK_AUTO_CLEAN_ON_MERGE");
    }
    if let Some(v) = env.get("THEGN_DISK_CLEAN_ON_PR_CLOSED") {
        o.disk_clean_on_pr_closed = parse_bool(&v, "THEGN_DISK_CLEAN_ON_PR_CLOSED");
    }
    if let Some(v) = env.get("THEGN_DISK_SCCACHE") {
        o.disk_sccache = parse_bool(&v, "THEGN_DISK_SCCACHE");
    }
    o.disk_sccache_dir = env.get("THEGN_DISK_SCCACHE_DIR");
    o.disk_shared_target_dir = env.get("THEGN_DISK_SHARED_TARGET_DIR");

    // [sandbox]
    if let Some(v) = env.get("THEGN_SANDBOX_BACKEND") {
        o.sandbox.backend = SandboxBackend::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_SANDBOX_NETWORK") {
        o.sandbox.network = Network::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_SANDBOX_PROFILE") {
        o.sandbox.profile = SandboxProfile::from_str_validated(v.trim()).ok();
    }
    o.sandbox.image = env.get("THEGN_SANDBOX_IMAGE");
    if let Some(v) = env.get("THEGN_SANDBOX_ON_MISSING") {
        o.sandbox.on_missing = OnMissing::from_str_validated(v.trim()).ok();
    }
    if let Some(v) = env.get("THEGN_SANDBOX_ENABLED") {
        o.sandbox.enabled = parse_bool(&v, "THEGN_SANDBOX_ENABLED");
    }
    if let Some(v) = env.get("THEGN_SANDBOX_INJECT_DEVSHELL") {
        o.sandbox.inject_devshell = parse_bool(&v, "THEGN_SANDBOX_INJECT_DEVSHELL");
    }
    if let Some(v) = env.get("THEGN_SANDBOX_NIX_DAEMON") {
        o.sandbox.nix_daemon = parse_bool(&v, "THEGN_SANDBOX_NIX_DAEMON");
    }
    if let Some(v) = env.get("THEGN_SANDBOX_WARM_DIRENV") {
        o.sandbox.warm_direnv = WarmDirenv::from_str_validated(v.trim()).ok();
    }
    if let Some(host) = env.get("THEGN_SANDBOX_REMOTE_HOST") {
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
        util::xdg_config_home().join("thegn").join("config.toml")
    }

    /// Load with all layers: defaults < file (`path` or the default) < env < flags.
    pub fn try_load_layered(
        env: &dyn EnvSource,
        cli_overrides: &[String],
        path: Option<PathBuf>,
    ) -> Result<Self, String> {
        let _span = tracing::info_span!("config_load_layered").entered();
        let file = path.unwrap_or_else(Self::path);
        // Only a *missing* file falls back to empty. An existing but unreadable
        // file (EACCES/EIO/invalid-UTF-8) must NOT masquerade as absent — return
        // Err so load_layered's warn-and-default path fires instead of silently
        // running on defaults with no signal to the user.
        let s = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
        };
        let mut cfg: Config = toml::from_str(&s).map_err(|e| format!("{e}"))?;

        // The AI layer ([llm_proxy], the LLM proxy + agent control plane) was
        // removed before the public alpha. A leftover section is harmless
        // (tolerant deser drops unknown keys) but worth one line of signal.
        if let Ok(raw) = toml::from_str::<toml::Value>(&s)
            && raw.get("llm_proxy").is_some()
        {
            config_warn("[llm_proxy] is no longer supported and is ignored");
        }

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
    /// (`<real XDG_CONFIG_HOME>/thegn/profiles/<name>/config.toml`), or `None`
    /// for the default profile. Uses `xdg_config_home()` directly (the shared
    /// config home is never rerooted).
    pub(crate) fn profile_overlay_path(env: &dyn EnvSource) -> Option<PathBuf> {
        let name = crate::profile::normalize_name(&env.get("THEGN_PROFILE").unwrap_or_default());
        (name != "default").then(|| {
            util::xdg_config_home()
                .join("thegn")
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

    /// Coerce a `--set KEY=VALUE` string to the JSON type the field expects.
    ///
    /// Bools and integers were already inferred; arrays and inline tables were
    /// not, so `apps.tab_order` needed a hand-written special case above and no
    /// other sequence-typed key could be set at all. TOML syntax is used for the
    /// bracketed forms because that is what the config file itself uses — a
    /// value the user can paste either way. A bare string still wins by default,
    /// so nothing that used to parse changes meaning.
    fn coerce_override_value(val: &str) -> serde_json::Value {
        let t = val.trim();
        let bracketed =
            (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('{') && t.ends_with('}'));
        if bracketed
            && let Ok(parsed) = toml::from_str::<serde_json::Value>(&format!("v = {t}"))
            && let Some(v) = parsed.get("v")
        {
            return v.clone();
        }
        if let Ok(b) = val.parse::<bool>() {
            return serde_json::Value::Bool(b);
        }
        if let Ok(n) = val.parse::<u64>() {
            return serde_json::Value::Number(n.into());
        }
        serde_json::Value::String(val.to_string())
    }

    pub(crate) fn apply_override_str(cfg: &mut Config, key: &str, val: &str) -> Result<(), String> {
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
                current[*part] = Self::coerce_override_value(val);
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
        // Reject a typo'd LEAF key: serde silently drops an unknown struct field,
        // so the override would vanish without a word. Re-serialize and confirm
        // the path survived the round-trip. A valid map/HashMap key (e.g.
        // `theme.colors.bg1`) survives; a struct-field typo does not. Intermediate
        // segments were already validated against the pre-image above.
        let check = serde_json::to_value(&new_cfg).map_err(|e| e.to_string())?;
        let mut cur = &check;
        for part in &parts {
            match cur.get(part) {
                Some(v) => cur = v,
                None => return Err(format!("Invalid path: {}", key)),
            }
        }
        *cfg = new_cfg;
        Ok(())
    }

    pub(crate) fn post_process(&mut self) {
        // Install the resolved [remote] tuning into the process-global holders
        // (ssh keepalives / control-plane retry / heal cadence); first set wins.
        self.remote.install();
        self.network.install(); // [network] → connectivity holder (mode + thresholds)
        if self.agents.is_empty() {
            self.agents = vec![
                NamedCommand {
                    name: "claude".into(),
                    command: "claude".into(),
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
        // `chrono::format()` only fails when the DelayedFormat is Displayed —
        // which for these two is inside `masthead_widget` on the render path.
        // Reject a bad specifier here so a config typo can't panic the
        // compositor; fall back to the shipped default for that field alone.
        let bars_default = BarsConfig::default();
        for (label, fmt, fallback) in [
            (
                "bars.date_format",
                &mut self.bars.date_format,
                bars_default.date_format,
            ),
            (
                "bars.clock_format",
                &mut self.bars.clock_format,
                bars_default.clock_format,
            ),
        ] {
            if let Err(e) = validate_strftime(fmt) {
                config_warn(&format!("{label}: {e} — using {fallback:?}"));
                *fmt = fallback;
            }
        }
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
    /// repo-root `.thegn.*` overlay. The host applies each layer in turn so a
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
    /// then a repo-root `.thegn.*` overlay when a worktree is in scope. Follows
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
            && let Some(mut overlay) = load_repo_overlay(root)
            && !overlay.notifications.is_empty()
        {
            // A repo's `.thegn.*` is UNTRUSTED. A hostile overlay must not be able
            // to supply a `[notifications.sound] mode = "command"` (+ a command /
            // per-priority command / chime file it controls), which would reach
            // `sh -c` on the next notification — RCE merely on opening the repo.
            // De-fang the repo-supplied sound before folding it; the user's own
            // config + profile keep the feature.
            if let Some(s) = overlay.notifications.sound.as_mut() {
                if s.mode == SoundMode::Command {
                    s.mode = SoundMode::Chime;
                }
                s.command.clear();
                s.per_priority.clear();
                s.chime_file.clear();
            }
            overlay.notifications.apply(&mut n);
        }
        n
    }

    /// The effective `[issues]` config for a worktree's repo: the global
    /// `[issues]` with a repo-root `.thegn.*` `[issues]` overlay applied on
    /// top, so each repo's "My Work" feed can pin its Linear team / Jira project
    /// (GitHub is auto-scoped to the repo's remote). `repo_root` is `None`
    /// outside a workspace, in which case the global config is returned verbatim.
    pub fn repo_issues(&self, repo_root: Option<&std::path::Path>) -> IssuesConfig {
        let mut issues = self.issues.clone();
        if let Some(root) = repo_root
            && let Some(overlay) = load_repo_overlay(root)
            && !overlay.issues.is_empty()
        {
            overlay.issues.apply(&mut issues);
        }
        issues
    }

    /// The effective sandbox config for a worktree's repo: global `[sandbox]` +
    /// profile overlay, with the repo `.thegn.*` overlay **clamped** and
    /// `[workspace.<slug>]` mounts extended. Fail-closed (gated requests denied);
    /// see [`crate::config_resolve`] / [`Config::repo_sandbox_resolved`].
    pub fn repo_sandbox(&self, repo_root: &std::path::Path) -> SandboxConfig {
        crate::config_resolve::resolve_repo_sandbox(
            self,
            repo_root,
            &crate::config_resolve::Approvals::deny_all(),
        )
        .sandbox
    }

    /// Like [`Config::repo_sandbox`] but honours `approvals` (trust-on-first-use)
    /// and returns the full [`crate::config_resolve::ResolvedRepoSandbox`] with
    /// clamp denials + pending gated requests for the host to surface.
    pub fn repo_sandbox_resolved(
        &self,
        repo_root: &std::path::Path,
        approvals: &crate::config_resolve::Approvals,
    ) -> crate::config_resolve::ResolvedRepoSandbox {
        crate::config_resolve::resolve_repo_sandbox(self, repo_root, approvals)
    }

    /// The effective `[merge_queue]` for a repo: the global table with the
    /// `[workspace.<slug>] merge_queue` overlay applied on top.
    ///
    /// Every repo-scoped merge-queue consumer MUST go through here rather than
    /// reading `Config::merge_queue` directly, or the per-repo layer silently
    /// does nothing on that path. `repo_root` identifies the repo; the slug is
    /// derived the same way `[workspace.<slug>]`'s other keys are.
    pub fn repo_merge_queue(&self, repo_root: &Path) -> MergeQueueConfig {
        let mut mq = self.merge_queue.clone();
        if !self.workspace.is_empty()
            && let Some(ws) = self.workspace.get(&workspace_slug(repo_root))
            && !ws.merge_queue.is_empty()
        {
            ws.merge_queue.clone().apply(&mut mq);
        }
        mq
    }

    /// `[pr_queue]` resolved for a repo: the global table with that repo's
    /// `[workspace.<slug>.pr_queue]` overlay applied. Same shape as
    /// [`Self::repo_merge_queue`].
    pub fn repo_pr_queue(&self, repo_root: &Path) -> PrQueueConfig {
        let mut pq = self.pr_queue.clone();
        if !self.workspace.is_empty()
            && let Some(ws) = self.workspace.get(&workspace_slug(repo_root))
            && !ws.pr_queue.is_empty()
        {
            ws.pr_queue.clone().apply(&mut pq);
        }
        pq
    }

    /// The name of the env a repo's `.thegn.*` overlay selects (`env = "…"`),
    /// or empty. The repo-level layer of env selection.
    pub fn repo_env_name(&self, repo_root: &Path) -> String {
        load_repo_overlay(repo_root)
            .map(|r| r.env.trim().to_string())
            .unwrap_or_default()
    }

    /// Resolve the full execution [`Environment`] for a worktree.
    ///
    /// Env-name precedence (most specific wins): `selected` (the DB worktree/
    /// workspace `env_name`, or a launch flag) → the repo `.thegn.*` `env =`
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
        crate::config_resolve::resolve_environment(
            self,
            repo_root,
            loc,
            worktree,
            selected,
            &crate::config_resolve::Approvals::deny_all(),
        )
        .0
    }

    /// Like [`Config::resolve_env`] but honours `approvals` and also returns the
    /// [`crate::config_resolve::ResolvedRepoSandbox`] (denials + pending) for a
    /// launch path to surface.
    pub fn resolve_env_with(
        &self,
        repo_root: &Path,
        loc: &GitLoc,
        worktree: &Path,
        selected: Option<&str>,
        approvals: &crate::config_resolve::Approvals,
    ) -> (Environment, crate::config_resolve::ResolvedRepoSandbox) {
        crate::config_resolve::resolve_environment(
            self, repo_root, loc, worktree, selected, approvals,
        )
    }

    /// Effective failover mode for `env_name`: the env's `[env.<name>] failover`
    /// override if set, else the (repo-overlaid) global `[sandbox] failover`.
    pub fn env_failover_mode(&self, repo_root: &Path, env_name: &str) -> FailoverMode {
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
            // Anything not spelled out above resolves generically. The arms
            // exist only for keys whose SCALAR rendering differs from the JSON
            // one (`repo_roots` is newline-joined for shell consumption,
            // `log.dir` is the resolved path, enums render as their TOML
            // spelling). Everything else used to fall off the end and report
            // "unknown config key" — including every key under `[merge_queue]`,
            // `[ui]`, `[drawer]`, `[placement]`, … which made `config get`
            // unusable for the entire nested surface even though `config
            // explain` resolved the same dotted paths fine.
            _ => return self.value_at(key).map(render_scalar),
        })
    }

    /// The raw JSON value at a dotted key, or `None` when the key doesn't exist.
    ///
    /// Distinguishes "absent" from "present but null", which a bare
    /// `Value::Null` cannot — `config get` needs that to keep reporting an
    /// unknown key as an error.
    pub fn value_at(&self, key: &str) -> Option<serde_json::Value> {
        let ptr = crate::config_resolve::dotted_to_pointer(key);
        serde_json::to_value(self).ok()?.pointer(&ptr).cloned()
    }
}

/// Render a JSON value the way `config get` should print it: bare scalars
/// unquoted (so `$(thegn config get …)` is directly usable in a script), and
/// arrays newline-joined to match the hand-written `repo_roots` arm.
fn render_scalar(v: serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => String::new(),
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(render_scalar)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

// Strict `config validate` lives in the sibling `config_validate` module
// (schema-driven: every `config_enum!` reachable from `Config` is checked).
// Re-exported here so callers keep the historical `config::validate_str` path.
pub use crate::config_validate::validate_str;

/// Load and parse a repo-root `.thegn.*` overlay, if present. Tries TOML,
/// YAML, then JSON (first existing file wins); parse errors warn and are ignored
/// so a malformed repo file never blocks opening a worktree.
pub(crate) fn load_repo_overlay(repo_root: &std::path::Path) -> Option<RepoConfigFile> {
    for (ext, kind) in [
        ("toml", "toml"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("json", "json"),
    ] {
        let path = repo_root.join(format!(".thegn.{ext}"));
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

/// A repo-root `.thegn.*` overlay that EXISTS but failed to parse. Returned by
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

/// If a repo-root `.thegn.*` file exists but fails to parse, return the error
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
        let path = repo_root.join(format!(".thegn.{ext}"));
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
#[path = "config_tests.rs"]
mod tests;
