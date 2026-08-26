//! The file-manager provider seam.
//!
//! The bottom file drawer runs a *file manager* — yazi by default. Everything
//! that makes the default drawer good (a private, seeded config home; the
//! accent-derived theme; the vendored `git.yazi` status plugin; the
//! image-preview containment policy; the `OSC 5379` control plugins that let
//! the manager close the drawer / open a file in the editor) is yazi-specific,
//! so it lives behind this seam rather than in the generic drawer code.
//!
//! Per the provider-seam rules (`crate::seam`):
//!
//! - [`FileManager`] is an **object-safe, synchronous** trait: a provider is
//!   process-bound (it constructs argv/env and scans PTY bytes — no network, no
//!   async client), so there is no [`crate::seam::BoxFuture`] here.
//! - Optional integrations are declared by [`FileManagerCaps`]; the host only
//!   attempts one when its caps bit is set (e.g. the drawer control channel is
//!   scanned only when `control_channel` is true).
//! - The [`DrawerKind`] `config_enum!` selects the provider: `yazi` (default,
//!   implemented), `custom` (implemented — runs `[drawer] command` with no
//!   integration caps), and `lf` / `broot` reserved (accepted by config but not
//!   implemented in this build; rejected by `config validate --strict`).
//! - Every provider describes itself through [`crate::seam::Probe`] so the
//!   drawer manager shows up in `thegn doctor` like every other seam.
//!
//! Back-compat: a non-empty `[drawer] command` with `kind` unset resolves to
//! `custom` (existing configs keep today's "run this binary" behavior); an
//! empty command keeps the pinned yazi. See [`effective_kind`].

use crate::config::{Config, config_enum, config_warn};
use crate::seam::{Availability, ErrorClass, Kind, Probe, ProbeReport, SeamError};
use std::path::{Path, PathBuf};

mod yazi;
pub use yazi::Yazi;

/// The seam label every drawer-manager probe reports under.
const SEAM: &str = "files";

config_enum! {
    /// `[drawer] kind` — the file manager the drawer runs. `yazi` is the
    /// default, fully-integrated provider; `custom` runs `[drawer] command`
    /// verbatim with no integrations. `lf` / `broot` are reserved: accepted by
    /// config so a future build can implement them without a config-format
    /// change, rejected by `config validate --strict` today (they are the
    /// managers users most often ask to swap in). Do not add `[drawer.<kind>]`
    /// sub-tables for reserved kinds — the provider-seams spec forbids config
    /// surface with nothing behind it.
    pub enum DrawerKind : "drawer kind" {
        Yazi   = "yazi",
        Custom = "custom",
        Lf     = "lf" reserved,
        Broot  = "broot" reserved,
    } default = Yazi;
}

/// A private control message the drawer manager emits back to the host on its
/// own PTY stream via `OSC 5379`. The manager owns all its keys (so `q`/`Esc`
/// stay literal in its own input fields); these commands let a manager keybind
/// drive the host chrome without the host intercepting — and mis-stealing —
/// keys. Only decoded for providers whose caps declare a `control_channel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrawerCmd {
    /// Hide the drawer into the keep-alive pool (the manager keeps running;
    /// position survives the reopen).
    Close,
    /// Open this (absolute) path in the center editor tab (via the editor seam).
    Editor(String),
}

/// A resolved spawn plan for the drawer PTY: the manager's argv, extra env
/// pairs, and cwd. Plain data — the host owns the PTY spawn, pooling, prewarm,
/// and the systemd containment wrap for every kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrawerSpawn {
    /// The manager command, argv[0] first. Empty ⇒ nothing runnable (the host
    /// degrades to a worktree shell).
    pub argv: Vec<String>,
    /// Extra environment pairs layered on the host base env.
    pub env: Vec<(String, String)>,
    /// Working directory (the worktree).
    pub cwd: Option<PathBuf>,
}

/// Which optional integrations a file-manager provider has. An integration is
/// attempted by the host only when its bit is set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct FileManagerCaps {
    /// VCS status linemode (the vendored `git.yazi` fetcher for yazi).
    pub git_status: bool,
    /// Accent-derived theme regeneration.
    pub themed: bool,
    /// The manager can emit drawer control commands (close / open-in-editor)
    /// on its PTY; the host scans its output for `OSC 5379` only when set.
    pub control_channel: bool,
    /// A private config home, seeded once + managed blocks kept fresh.
    pub config_isolation: bool,
    /// The image-preview containment policy is enforceable in the config.
    pub image_policy: bool,
}

/// The error contract for the file-manager seam. Kept minimal — the seam is
/// pure planning, so the only failures are "provider can't do this op".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileManagerError {
    /// The provider declares it cannot do this operation (caps bit off).
    Unsupported(&'static str),
    /// Nothing is configured for this provider (e.g. an empty custom command).
    NotConfigured(&'static str),
    /// Anything else.
    Other(String),
}

impl std::fmt::Display for FileManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileManagerError::Unsupported(op) => write!(f, "file manager does not support {op}"),
            FileManagerError::NotConfigured(w) => write!(f, "file manager not configured: {w}"),
            FileManagerError::Other(m) => f.write_str(m),
        }
    }
}
impl std::error::Error for FileManagerError {}
impl SeamError for FileManagerError {
    fn class(&self) -> ErrorClass {
        match self {
            FileManagerError::Unsupported(_) => ErrorClass::Unsupported,
            FileManagerError::NotConfigured(_) => ErrorClass::NotConfigured,
            FileManagerError::Other(_) => ErrorClass::Other,
        }
    }
    fn unsupported(op: &'static str) -> Self {
        FileManagerError::Unsupported(op)
    }
}

/// The file-manager seam. Object-safe and **synchronous** — a provider only
/// constructs a launch plan and decodes bytes; it never blocks on I/O beyond
/// best-effort config seeding in [`FileManager::prepare`].
pub trait FileManager: Probe + Send + Sync {
    /// The provider id (`"yazi"`, `"custom"`).
    fn id(&self) -> &'static str;
    /// Which optional integrations this provider has.
    fn caps(&self) -> FileManagerCaps;
    /// The spawn plan for the drawer PTY at `cwd` (the worktree).
    fn spawn_spec(&self, cwd: &Path) -> DrawerSpawn;
    /// Seed / refresh the manager's private config dir, returning it. `None`
    /// when there is nothing to prepare (`config_isolation` off). Best-effort
    /// std-fs; a failure just means the manager falls back to its own defaults.
    fn prepare(&self) -> Option<PathBuf> {
        None
    }
    /// Regenerate accent-derived theming into a prepared dir. Only meaningful
    /// when `caps().themed`; the default is a no-op.
    fn apply_theme(&self, _dir: &Path) {}
    /// Decode a private control message from a drawer output chunk. Only called
    /// by the host when `caps().control_channel`; the default has no channel.
    fn control(&self, _bytes: &[u8]) -> Option<DrawerCmd> {
        None
    }
}

/// The effective kind after back-compat resolution:
///
/// - `kind` unset (or explicitly `yazi`) **with a non-empty `command`** ⇒
///   `custom` (the bare-command config keeps today's behavior; an explicit
///   `kind = "yazi"` beside a command lets the command win — see
///   [`ambiguous_yazi_command`]).
/// - `kind` unset with an empty `command` ⇒ `yazi`.
/// - an explicit implemented kind ⇒ itself.
/// - a reserved kind ⇒ `yazi` (defensive: the lenient config loader already
///   remapped a reserved value to the default, so this arm is unreachable at
///   runtime).
pub fn effective_kind(cfg: &Config) -> DrawerKind {
    let kind = cfg.drawer.kind.unwrap_or_default();
    let command_set = !cfg.drawer.command.trim().is_empty();
    if kind == DrawerKind::Yazi && command_set {
        return DrawerKind::Custom;
    }
    if <DrawerKind as Kind>::is_reserved(kind) {
        return DrawerKind::Yazi;
    }
    kind
}

/// Whether the config is the ambiguous `kind = "yazi"` beside a non-empty
/// `command`: the command wins (resolves to `custom`), but the two knobs
/// disagree. `thegn doctor` surfaces this so the user picks one.
pub fn ambiguous_yazi_command(cfg: &Config) -> bool {
    matches!(cfg.drawer.kind, Some(DrawerKind::Yazi)) && !cfg.drawer.command.trim().is_empty()
}

/// Resolve the file manager the drawer runs for this config. Reserved kinds
/// never reach here (the loader remaps them to the default), so this always
/// yields a runnable provider.
pub fn file_manager_for(cfg: &Config) -> Box<dyn FileManager> {
    file_manager_for_kind(effective_kind(cfg), cfg).unwrap_or_else(|| Box::new(Yazi::from_cfg(cfg)))
}

/// The reserved-aware factory: `None` for a reserved (accepted-but-
/// unimplemented) kind, the built provider otherwise. Drives the kind-coverage
/// conformance test.
pub fn file_manager_for_kind(kind: DrawerKind, cfg: &Config) -> Option<Box<dyn FileManager>> {
    match kind {
        DrawerKind::Yazi => Some(Box::new(Yazi::from_cfg(cfg))),
        DrawerKind::Custom => Some(Box::new(Custom::from_cfg(cfg))),
        // Reserved: accepted by config but not implemented in this build.
        // Exhaustive so a new kind is a compile error until it is implemented
        // here or added as reserved.
        DrawerKind::Lf | DrawerKind::Broot => None,
    }
}

/// Gate + decode the drawer control channel in one call for the host's PTY
/// drain: `None` unless the selected provider declares a `control_channel` and
/// the bytes carry a valid command. The provider decodes it (yazi via its
/// `OSC 5379` grammar); a capless manager (`custom`) is never scanned.
pub fn decode_control(cfg: &Config, bytes: &[u8]) -> Option<DrawerCmd> {
    let fm = file_manager_for(cfg);
    if fm.caps().control_channel {
        fm.control(bytes)
    } else {
        None
    }
}

/// `Ready` when `program`'s binary resolves (an absolute/relative path that
/// exists, or a bare name on `PATH`), else `Unavailable` naming it. Shared by
/// the provider probes.
pub(crate) fn binary_availability(program: &str) -> Availability {
    let prog = program.split_whitespace().next().unwrap_or("");
    if prog.is_empty() {
        return Availability::Unavailable("no command configured".into());
    }
    if prog.contains('/') {
        if Path::new(prog).exists() {
            Availability::Ready
        } else {
            Availability::Unavailable(format!("{prog} not found"))
        }
    } else if crate::util::which_path(prog).is_some() {
        Availability::Ready
    } else {
        Availability::Unavailable(format!("`{prog}` not found on PATH"))
    }
}

// ── the `custom` provider ─────────────────────────────────────────────────────

/// The `custom` file manager: runs `[drawer] command` verbatim with no
/// integration caps. Split on whitespace into argv (the manager binary, then
/// any flags); a manager needing shell quoting should point at a wrapper
/// script.
pub struct Custom {
    command: String,
}

impl Custom {
    pub fn from_cfg(cfg: &Config) -> Self {
        Custom {
            command: cfg.drawer.command.trim().to_string(),
        }
    }
}

impl Probe for Custom {
    fn probe(&self) -> ProbeReport {
        let note = if self.command.is_empty() {
            "[drawer] command is empty".to_string()
        } else {
            format!("[drawer] command = {:?}", self.command)
        };
        ProbeReport::new(SEAM, "custom", binary_availability(&self.command))
            .with_caps(&self.caps())
            .note(note)
            .note("no integrations: git status, theming and control channel are yazi-only")
    }
}

impl FileManager for Custom {
    fn id(&self) -> &'static str {
        "custom"
    }
    fn caps(&self) -> FileManagerCaps {
        FileManagerCaps::default()
    }
    fn spawn_spec(&self, cwd: &Path) -> DrawerSpawn {
        DrawerSpawn {
            argv: self
                .command
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            env: Vec::new(),
            cwd: Some(cwd.to_path_buf()),
        }
    }
}

// ── the drawer control channel grammar (OSC 5379) ─────────────────────────────

/// Private OSC number for the drawer→host control channel. Chosen high to
/// avoid colliding with any standard OSC; the vt100 emulator ignores it on
/// `feed`.
const DRAWER_OSC: &[u8] = b"5379;";

/// Scan a drawer output chunk for the first `OSC 5379;<cmd>` control message
/// and decode it. Returns `None` for ordinary output. The seam's control
/// vocabulary — a provider whose caps declare a control channel decodes bytes
/// through here.
pub(crate) fn scan_drawer_control(bytes: &[u8]) -> Option<DrawerCmd> {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b']' {
            let body = &bytes[i + 2..];
            if let Some((seq, len)) = osc_seq(body) {
                if let Some(rest) = seq.strip_prefix(DRAWER_OSC) {
                    return decode_drawer_cmd(rest);
                }
                i += 2 + len;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// Read one OSC sequence body up to its terminator (`BEL` or `ESC \`),
/// returning the payload and the total consumed length.
fn osc_seq(body: &[u8]) -> Option<(&[u8], usize)> {
    for (i, &b) in body.iter().enumerate() {
        if b == 0x07 {
            return Some((&body[..i], i + 1));
        }
        if b == 0x1b && body.get(i + 1) == Some(&b'\\') {
            return Some((&body[..i], i + 2));
        }
    }
    None
}

fn decode_drawer_cmd(rest: &[u8]) -> Option<DrawerCmd> {
    if rest == b"close" {
        return Some(DrawerCmd::Close);
    }
    if let Some(path) = rest.strip_prefix(b"editor;") {
        let path = String::from_utf8_lossy(path).into_owned();
        if !path.is_empty() {
            return Some(DrawerCmd::Editor(path));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(kind: Option<DrawerKind>, command: &str) -> Config {
        let mut c = Config::default();
        c.drawer.kind = kind;
        c.drawer.command = command.into();
        c
    }

    #[test]
    fn effective_kind_back_compat_arms() {
        // Unset kind + empty command ⇒ the pinned yazi.
        assert_eq!(effective_kind(&cfg_with(None, "")), DrawerKind::Yazi);
        // Unset kind + a bare command ⇒ custom (existing configs keep behavior).
        assert_eq!(effective_kind(&cfg_with(None, "lf")), DrawerKind::Custom);
        // Explicit custom ⇒ custom, command or not.
        assert_eq!(
            effective_kind(&cfg_with(Some(DrawerKind::Custom), "")),
            DrawerKind::Custom
        );
        assert_eq!(
            effective_kind(&cfg_with(Some(DrawerKind::Custom), "ranger")),
            DrawerKind::Custom
        );
        // Explicit yazi + a command: the command wins (ambiguous), resolves custom.
        assert_eq!(
            effective_kind(&cfg_with(Some(DrawerKind::Yazi), "broot")),
            DrawerKind::Custom
        );
        // Explicit yazi + no command ⇒ yazi.
        assert_eq!(
            effective_kind(&cfg_with(Some(DrawerKind::Yazi), "")),
            DrawerKind::Yazi
        );
    }

    #[test]
    fn ambiguous_yazi_command_only_for_explicit_yazi_plus_command() {
        assert!(ambiguous_yazi_command(&cfg_with(
            Some(DrawerKind::Yazi),
            "lf"
        )));
        // Unset + command is the silent back-compat default, not ambiguous.
        assert!(!ambiguous_yazi_command(&cfg_with(None, "lf")));
        assert!(!ambiguous_yazi_command(&cfg_with(
            Some(DrawerKind::Yazi),
            ""
        )));
        assert!(!ambiguous_yazi_command(&cfg_with(
            Some(DrawerKind::Custom),
            "lf"
        )));
    }

    #[test]
    fn factory_covers_every_kind_and_reserved_returns_none() {
        let cfg = Config::default();
        for k in DrawerKind::ALL {
            let built = file_manager_for_kind(*k, &cfg).is_some();
            assert_eq!(built, !k.is_reserved(), "kind {k:?}");
        }
        // The runtime factory always yields a runnable provider.
        assert_eq!(file_manager_for(&cfg_with(None, "")).id(), "yazi");
        assert_eq!(file_manager_for(&cfg_with(None, "lf")).id(), "custom");
    }

    #[test]
    fn reserved_kinds_reject_under_strict_validation() {
        for name in ["lf", "broot"] {
            let e = DrawerKind::from_str_validated(name).unwrap_err();
            assert!(e.contains("reserved"), "{name}: {e}");
        }
        assert_eq!(
            DrawerKind::from_str_validated("yazi").unwrap(),
            DrawerKind::Yazi
        );
        assert_eq!(
            DrawerKind::from_str_validated("custom").unwrap(),
            DrawerKind::Custom
        );
    }

    #[test]
    fn custom_caps_are_all_off_and_spawn_splits_the_command() {
        let fm = file_manager_for(&cfg_with(Some(DrawerKind::Custom), "lf -x"));
        assert_eq!(fm.id(), "custom");
        assert_eq!(fm.caps(), FileManagerCaps::default());
        let spawn = fm.spawn_spec(Path::new("/tmp/wt"));
        assert_eq!(spawn.argv, vec!["lf".to_string(), "-x".to_string()]);
        assert!(spawn.env.is_empty());
        assert_eq!(spawn.cwd.as_deref(), Some(Path::new("/tmp/wt")));
        // A capless manager is never scanned, even if it emits the sequence.
        assert!(
            decode_control(
                &cfg_with(Some(DrawerKind::Custom), "lf"),
                b"\x1b]5379;close\x07"
            )
            .is_none()
        );
        assert!(fm.control(b"\x1b]5379;close\x07").is_none());
    }

    #[test]
    fn custom_probe_reports_missing_binary_by_name() {
        let fm = file_manager_for(&cfg_with(
            Some(DrawerKind::Custom),
            "thegn-no-such-file-manager-xyz",
        ));
        match fm.probe().availability {
            Availability::Unavailable(reason) => {
                assert!(
                    reason.contains("thegn-no-such-file-manager-xyz"),
                    "{reason}"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn drawer_control_decodes_close_and_editor() {
        assert_eq!(
            scan_drawer_control(b"\x1b]5379;close\x07"),
            Some(DrawerCmd::Close)
        );
        assert_eq!(
            scan_drawer_control(b"noise\x1b]5379;editor;/home/u/a q.rs\x1b\\more"),
            Some(DrawerCmd::Editor("/home/u/a q.rs".into()))
        );
    }

    #[test]
    fn drawer_control_ignores_unrelated_or_malformed() {
        assert_eq!(scan_drawer_control(b"\x1b]52;c;aGk=\x07"), None);
        assert_eq!(scan_drawer_control(b"\x1b]5379;close"), None);
        assert_eq!(scan_drawer_control(b"\x1b]5379;bogus\x07"), None);
        assert_eq!(scan_drawer_control(b"\x1b]5379;editor;\x07"), None);
        assert_eq!(scan_drawer_control(b"just some text\r\n"), None);
    }

    #[test]
    fn errors_classify() {
        assert_eq!(
            FileManagerError::Unsupported("x").class(),
            ErrorClass::Unsupported
        );
        assert_eq!(
            FileManagerError::NotConfigured("y").class(),
            ErrorClass::NotConfigured
        );
        assert_eq!(
            FileManagerError::unsupported("z").class(),
            ErrorClass::Unsupported
        );
        assert_eq!(FileManagerError::Other("w".into()).to_string(), "w");
    }
}
