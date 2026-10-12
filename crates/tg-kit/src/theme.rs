//! Shared theme: semantic tokens → sRGB, rendered as ratatui [`Style`]s.
//!
//! [`Theme::prism`] mirrors thegn's default chrome palette
//! (`thegn-core/src/theme.rs`). Embedded, the host converts its live
//! `Palette` into a [`Theme`] (so theme-cycling and user `[theme.colors]`
//! overrides flow through). Standalone, [`Theme::load_thegn_config_checked`]
//! reads the same `config.toml` (bounded, typed outcome). Named presets
//! resolve exactly only with the opt-in `host-presets` feature (which links
//! `thegn-core`); without it a non-prism preset is reported as
//! [`ThemeIssue::UnresolvedPreset`] and Prism is the base.

use ratatui::style::{Color, Style};
use serde::Deserialize;

/// An sRGB triple.
pub type Rgb = (u8, u8, u8);

/// A semantic color slot. Names mirror the thegn `Palette` fields so the
/// host conversion is a field-by-field copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Bg0,
    Bg1,
    Panel,
    Panel2,
    Raise,
    Border,
    Focus,
    Text,
    Dim,
    Faint,
    Ghost,
    Ghost2,
    Ghost3,
    Accent,
    ChipFg,
    Teal,
    Magenta,
    Purple,
    Green,
    Amber,
    Red,
    Blue,
    Orange,
}

/// The resolved palette an app tile renders with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub bg0: Rgb,
    pub bg1: Rgb,
    pub panel: Rgb,
    pub panel2: Rgb,
    pub raise: Rgb,
    pub border: Rgb,
    pub focus: Rgb,
    pub text: Rgb,
    pub dim: Rgb,
    pub faint: Rgb,
    pub ghost: Rgb,
    pub ghost2: Rgb,
    pub ghost3: Rgb,
    pub accent: Rgb,
    pub chip_fg: Rgb,
    pub teal: Rgb,
    pub magenta: Rgb,
    pub purple: Rgb,
    pub green: Rgb,
    pub amber: Rgb,
    pub red: Rgb,
    pub blue: Rgb,
    pub orange: Rgb,
}

impl Default for Theme {
    fn default() -> Self {
        Self::prism()
    }
}

impl Theme {
    /// The prism defaults — byte-for-byte the same sRGB as thegn's default
    /// `Palette` (kept in sync by a parity test in `thegn-host`).
    pub fn prism() -> Theme {
        Theme {
            bg0: (11, 14, 22),
            bg1: (16, 20, 31),
            panel: (21, 26, 40),
            panel2: (26, 32, 49),
            raise: (34, 41, 66),
            // Palette::default sets border = ghost.
            border: (111, 120, 142),
            focus: (110, 231, 216),
            text: (237, 240, 248),
            dim: (198, 204, 219),
            faint: (122, 129, 151),
            ghost: (111, 120, 142),
            ghost2: (103, 112, 132),
            ghost3: (96, 104, 123),
            accent: (110, 231, 216),
            chip_fg: (11, 14, 22),
            teal: (110, 231, 216),
            magenta: (239, 143, 196),
            purple: (182, 156, 242),
            green: (127, 220, 160),
            amber: (230, 194, 100),
            red: (239, 111, 111),
            blue: (127, 180, 236),
            orange: (240, 157, 106),
        }
    }

    /// Resolve a token to its sRGB triple.
    pub fn rgb(&self, t: Tok) -> Rgb {
        match t {
            Tok::Bg0 => self.bg0,
            Tok::Bg1 => self.bg1,
            Tok::Panel => self.panel,
            Tok::Panel2 => self.panel2,
            Tok::Raise => self.raise,
            Tok::Border => self.border,
            Tok::Focus => self.focus,
            Tok::Text => self.text,
            Tok::Dim => self.dim,
            Tok::Faint => self.faint,
            Tok::Ghost => self.ghost,
            Tok::Ghost2 => self.ghost2,
            Tok::Ghost3 => self.ghost3,
            Tok::Accent => self.accent,
            Tok::ChipFg => self.chip_fg,
            Tok::Teal => self.teal,
            Tok::Magenta => self.magenta,
            Tok::Purple => self.purple,
            Tok::Green => self.green,
            Tok::Amber => self.amber,
            Tok::Red => self.red,
            Tok::Blue => self.blue,
            Tok::Orange => self.orange,
        }
    }

    /// A token as a ratatui [`Color`].
    pub fn color(&self, t: Tok) -> Color {
        let (r, g, b) = self.rgb(t);
        Color::Rgb(r, g, b)
    }

    /// A foreground [`Style`] for a token.
    pub fn fg(&self, t: Tok) -> Style {
        Style::default().fg(self.color(t))
    }

    /// A background [`Style`] for a token.
    pub fn bg(&self, t: Tok) -> Style {
        Style::default().bg(self.color(t))
    }

    /// A filled-chip style: `chip_fg` text on the given token background.
    pub fn chip(&self, bg: Tok) -> Style {
        Style::default()
            .fg(self.color(Tok::ChipFg))
            .bg(self.color(bg))
    }

    /// The accent selection-row tint: accent at ~16% over bg1.
    pub fn sel(&self) -> Color {
        let (r, g, b) = blend(self.accent, self.bg1, 0.16);
        Color::Rgb(r, g, b)
    }

    /// Load the user's thegn theme from `config.toml` (XDG), falling back to
    /// [`Theme::prism`]. Convenience over [`Theme::load_thegn_config_checked`]
    /// that DISCARDS the diagnostics: use the checked form when the caller
    /// needs to tell "Prism selected" from "configuration failed".
    pub fn load_thegn_config() -> Theme {
        Self::load_thegn_config_checked().theme
    }

    /// Like [`Theme::load_thegn_config`], but reports exactly what happened.
    ///
    /// Supported precedence (and nothing else): the single file
    /// `$XDG_CONFIG_HOME/thegn/config.toml` (or `~/.config/thegn/config.toml`),
    /// with `[theme] preset` as the base and `[theme] accent` / `focus_border`
    /// / `[theme.colors]` / `[theme.hues]` `#rrggbb` overrides on top.
    /// NOT supported: `--config`, the `THEGN_*` env layer, `THEGN_PROFILE`
    /// overlays and local user themes (use the embedded host path, or
    /// [`Theme::load_from_path`] for an explicit file).
    pub fn load_thegn_config_checked() -> ThemeLoad {
        match config_path() {
            Some(path) => Self::load_from_path(&path),
            None => ThemeLoad::fallback(ThemeOutcome::NoConfig),
        }
    }

    /// Load a theme from an explicit config file. The read is bounded
    /// ([`MAX_CONFIG_BYTES`]) and only regular files are read. A FIFO or
    /// device present at the initial metadata check is refused; a path swapped
    /// to a FIFO between that check and `open` can still block.
    pub fn load_from_path(path: &std::path::Path) -> ThemeLoad {
        match read_bounded(path) {
            Ok(text) => Self::from_config_str(&text),
            Err(ThemeLoadError::Absent) => ThemeLoad::fallback(ThemeOutcome::NoConfig),
            Err(e) => ThemeLoad::fallback(ThemeOutcome::Failed(e)),
        }
    }

    /// Resolve a theme from config TOML text (no size check: callers that
    /// read the text themselves own the bound; see [`MAX_CONFIG_BYTES`]).
    pub fn from_config_str(text: &str) -> ThemeLoad {
        let doc = match toml::from_str::<ConfigDoc>(text) {
            Ok(doc) => doc,
            Err(e) => {
                return ThemeLoad::fallback(ThemeOutcome::Failed(ThemeLoadError::Malformed(
                    e.to_string(),
                )));
            }
        };
        let mut issues = Vec::new();
        let preset = doc.theme.preset.as_deref().unwrap_or("");
        let (mut theme, outcome) = if preset.is_empty() || preset == "prism" {
            (
                resolve_preset_with_overrides("prism", &doc.theme).unwrap_or_else(Theme::prism),
                ThemeOutcome::Prism,
            )
        } else if let Some(t) = resolve_preset_with_overrides(preset, &doc.theme) {
            (t, ThemeOutcome::Preset(preset.to_string()))
        } else {
            issues.push(ThemeIssue::UnresolvedPreset(preset.to_string()));
            (
                unresolved_preset_fallback(preset, &doc.theme),
                ThemeOutcome::Prism,
            )
        };
        theme.apply(&doc.theme, &mut issues);
        ThemeLoad {
            theme,
            outcome,
            issues,
        }
    }

    /// Overlay `[theme]` config fields (each optional `#rrggbb`); malformed
    /// values are skipped and recorded in `issues`.
    fn apply(&mut self, t: &ThemeSection, issues: &mut Vec<ThemeIssue>) {
        fn set(
            slot: &mut Rgb,
            field: &'static str,
            hex: &Option<String>,
            issues: &mut Vec<ThemeIssue>,
        ) {
            if let Some(raw) = hex.as_deref() {
                match parse_hex(raw) {
                    Some(rgb) => *slot = rgb,
                    None => issues.push(ThemeIssue::InvalidColor {
                        field,
                        value: raw.to_string(),
                    }),
                }
            }
        }
        // The host never lets the default accent/focus clobber a preset's own.
        if let Some(raw) = t.accent.as_deref() {
            match parse_hex(raw) {
                Some(rgb) => {
                    if !DEFAULTISH_ACCENTS.contains(&raw) {
                        self.accent = rgb;
                    }
                }
                None => issues.push(ThemeIssue::InvalidColor {
                    field: "accent",
                    value: raw.to_string(),
                }),
            }
        }
        match t.focus_border.as_deref() {
            Some(raw) if DEFAULTISH_FOCUS.contains(&raw) => {}
            _ => set(&mut self.focus, "focus_border", &t.focus_border, issues),
        }
        let c = &t.colors;
        set(&mut self.bg0, "colors.bg0", &c.bg0, issues);
        set(&mut self.bg1, "colors.bg1", &c.bg1, issues);
        set(&mut self.panel, "colors.panel", &c.panel, issues);
        set(&mut self.panel2, "colors.panel2", &c.panel2, issues);
        set(&mut self.raise, "colors.raise", &c.raise, issues);
        set(&mut self.border, "colors.border", &c.border, issues);
        set(&mut self.text, "colors.text", &c.text, issues);
        set(&mut self.dim, "colors.dim", &c.dim, issues);
        set(&mut self.faint, "colors.faint", &c.faint, issues);
        set(&mut self.ghost, "colors.ghost", &c.ghost, issues);
        set(&mut self.ghost2, "colors.ghost2", &c.ghost2, issues);
        set(&mut self.ghost3, "colors.ghost3", &c.ghost3, issues);
        set(&mut self.chip_fg, "colors.chip_fg", &c.chip_fg, issues);
        let h = &t.hues;
        set(&mut self.teal, "hues.teal", &h.teal, issues);
        set(&mut self.magenta, "hues.magenta", &h.magenta, issues);
        set(&mut self.purple, "hues.purple", &h.purple, issues);
        set(&mut self.green, "hues.green", &h.green, issues);
        set(&mut self.amber, "hues.amber", &h.amber, issues);
        set(&mut self.red, "hues.red", &h.red, issues);
        set(&mut self.blue, "hues.blue", &h.blue, issues);
        set(&mut self.orange, "hues.orange", &h.orange, issues);
    }
}

/// Hard cap on config bytes read by [`Theme::load_from_path`] (256 KiB; a
/// real thegn config is a few KiB). Input of exactly this size is accepted.
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// Accent / focus values the host treats as "the default": they never
/// overwrite what a preset chose (mirrors `thegn-core`'s `theme_resolve`).
const DEFAULTISH_ACCENTS: &[&str] = &["#6ee7d8", "#76eede"];
const DEFAULTISH_FOCUS: &[&str] = &["#6ee7d8", "#9bd1ff"];

/// What a config load resolved to. `theme` is always usable; `outcome` and
/// `issues` say whether it is the theme the user actually selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeLoad {
    pub theme: Theme,
    pub outcome: ThemeOutcome,
    /// Per-field problems; the rest of the document still applied.
    pub issues: Vec<ThemeIssue>,
}

impl ThemeLoad {
    fn fallback(outcome: ThemeOutcome) -> ThemeLoad {
        ThemeLoad {
            theme: Theme::prism(),
            outcome,
            issues: Vec::new(),
        }
    }

    /// True only when the theme is what the config asked for: a genuine
    /// Prism/preset selection with every field honoured. An absent config
    /// is not "faithful" (nothing was selected), see [`ThemeOutcome::NoConfig`].
    pub fn is_faithful(&self) -> bool {
        matches!(self.outcome, ThemeOutcome::Prism | ThemeOutcome::Preset(_))
            && self.issues.is_empty()
    }
}

/// How the base theme was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeOutcome {
    /// The config selected Prism (or named no preset), a genuine choice.
    Prism,
    /// A named non-prism preset resolved exactly.
    Preset(String),
    /// No config file (or no config directory): Prism by default.
    NoConfig,
    /// The config could not be used; Prism is a FALLBACK, not a selection.
    Failed(ThemeLoadError),
}

/// Why a config source could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeLoadError {
    /// The file does not exist (reported as [`ThemeOutcome::NoConfig`]).
    Absent,
    /// Exists but could not be opened/read.
    Unreadable(String),
    /// Not a regular file (directory, FIFO, device, ...).
    NotRegular,
    /// Larger than [`MAX_CONFIG_BYTES`].
    Oversize,
    /// Not valid UTF-8.
    InvalidUtf8,
    /// Not valid TOML for the fields we read.
    Malformed(String),
}

/// A problem that degraded an otherwise-applied config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeIssue {
    /// `[theme] preset` named something this loader cannot resolve: an
    /// unknown name, a local user theme, or (without the `host-presets`
    /// feature) any non-prism preset. The base fell back to Prism.
    UnresolvedPreset(String),
    /// A `#rrggbb` field was malformed and skipped.
    InvalidColor { field: &'static str, value: String },
}

/// Read `path` with a hard byte cap, refusing non-regular files up front.
fn read_bounded(path: &std::path::Path) -> Result<String, ThemeLoadError> {
    use std::io::Read;
    let unreadable = |e: std::io::Error| ThemeLoadError::Unreadable(e.to_string());
    // `metadata` follows symlinks; an existing FIFO is rejected before `open`
    // could block. A concurrent path swap remains possible.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ThemeLoadError::Absent),
        Err(e) => return Err(unreadable(e)),
    };
    if !meta.is_file() {
        return Err(ThemeLoadError::NotRegular);
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(ThemeLoadError::Oversize);
    }
    let file = std::fs::File::open(path).map_err(unreadable)?;
    if !file.metadata().map_err(unreadable)?.is_file() {
        return Err(ThemeLoadError::NotRegular);
    }
    let mut bytes = Vec::new();
    // The file may grow after the stat: cap the read itself, one byte over.
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ThemeLoadError::Oversize);
    }
    String::from_utf8(bytes).map_err(|_| ThemeLoadError::InvalidUtf8)
}

/// Resolve a built-in preset through the host's catalog and resolver. Without
/// `host-presets` the catalog is not linked, so this returns `None`.
#[cfg(feature = "host-presets")]
fn resolve_preset_with_overrides(name: &str, section: &ThemeSection) -> Option<Theme> {
    let name = if name.is_empty() { "prism" } else { name };
    thegn_core::theme::preset(name)?;
    Some(theme_with_host_overrides(name, section))
}

#[cfg(feature = "host-presets")]
fn unresolved_preset_fallback(name: &str, section: &ThemeSection) -> Theme {
    theme_with_host_overrides(name, section)
}

#[cfg(feature = "host-presets")]
fn theme_with_host_overrides(name: &str, section: &ThemeSection) -> Theme {
    let colors = thegn_core::config::ThemeColors {
        bg0: section.colors.bg0.clone(),
        bg1: section.colors.bg1.clone(),
        panel: section.colors.panel.clone(),
        panel2: section.colors.panel2.clone(),
        raise: section.colors.raise.clone(),
        border: section.colors.border.clone(),
        text: section.colors.text.clone(),
        dim: section.colors.dim.clone(),
        faint: section.colors.faint.clone(),
        ghost: section.colors.ghost.clone(),
        ghost2: section.colors.ghost2.clone(),
        ghost3: section.colors.ghost3.clone(),
        chip_fg: section.colors.chip_fg.clone(),
        ..Default::default()
    };
    let hues = thegn_core::config::ThemeHues {
        teal: section.hues.teal.clone(),
        magenta: section.hues.magenta.clone(),
        purple: section.hues.purple.clone(),
        green: section.hues.green.clone(),
        amber: section.hues.amber.clone(),
        red: section.hues.red.clone(),
        blue: section.hues.blue.clone(),
        orange: section.hues.orange.clone(),
    };
    let accent = section.accent.as_deref().unwrap_or("#6ee7d8");
    let focus = section.focus_border.as_deref().unwrap_or("#6ee7d8");
    let p = thegn_core::theme_resolve::palette_with_config(name, &colors, &hues, accent, focus);
    Theme::from_fragments(|f| match f {
        Frag::Bg0 => &p.bg0,
        Frag::Bg1 => &p.bg1,
        Frag::Panel => &p.panel,
        Frag::Panel2 => &p.panel2,
        Frag::Raise => &p.raise,
        Frag::Border => &p.border,
        Frag::Focus => &p.focus,
        Frag::Text => &p.text,
        Frag::Dim => &p.dim,
        Frag::Faint => &p.faint,
        Frag::Ghost => &p.ghost,
        Frag::Ghost2 => &p.ghost2,
        Frag::Ghost3 => &p.ghost3,
        Frag::Accent => &p.accent,
        Frag::ChipFg => &p.chip_fg,
        Frag::Teal => &p.hues.teal,
        Frag::Magenta => &p.hues.magenta,
        Frag::Purple => &p.hues.purple,
        Frag::Green => &p.hues.green,
        Frag::Amber => &p.hues.amber,
        Frag::Red => &p.hues.red,
        Frag::Blue => &p.hues.blue,
        Frag::Orange => &p.hues.orange,
    })
}

#[cfg(not(feature = "host-presets"))]
fn resolve_preset_with_overrides(_name: &str, _section: &ThemeSection) -> Option<Theme> {
    None
}

#[cfg(not(feature = "host-presets"))]
fn unresolved_preset_fallback(_name: &str, _section: &ThemeSection) -> Theme {
    Theme::prism()
}

#[cfg(feature = "host-presets")]
#[derive(Clone, Copy)]
enum Frag {
    Bg0,
    Bg1,
    Panel,
    Panel2,
    Raise,
    Border,
    Focus,
    Text,
    Dim,
    Faint,
    Ghost,
    Ghost2,
    Ghost3,
    Accent,
    ChipFg,
    Teal,
    Magenta,
    Purple,
    Green,
    Amber,
    Red,
    Blue,
    Orange,
}

#[cfg(feature = "host-presets")]
impl Theme {
    /// Build a theme from `"R;G;B"` palette fragments (missing channels 0),
    /// the same conversion the host's `kit_theme` performs.
    fn from_fragments<'a>(get: impl Fn(Frag) -> &'a String) -> Theme {
        let rgb = |f: Frag| {
            let mut it = get(f)
                .split(';')
                .map(|n| n.trim().parse::<u8>().unwrap_or(0));
            (
                it.next().unwrap_or(0),
                it.next().unwrap_or(0),
                it.next().unwrap_or(0),
            )
        };
        Theme {
            bg0: rgb(Frag::Bg0),
            bg1: rgb(Frag::Bg1),
            panel: rgb(Frag::Panel),
            panel2: rgb(Frag::Panel2),
            raise: rgb(Frag::Raise),
            border: rgb(Frag::Border),
            focus: rgb(Frag::Focus),
            text: rgb(Frag::Text),
            dim: rgb(Frag::Dim),
            faint: rgb(Frag::Faint),
            ghost: rgb(Frag::Ghost),
            ghost2: rgb(Frag::Ghost2),
            ghost3: rgb(Frag::Ghost3),
            accent: rgb(Frag::Accent),
            chip_fg: rgb(Frag::ChipFg),
            teal: rgb(Frag::Teal),
            magenta: rgb(Frag::Magenta),
            purple: rgb(Frag::Purple),
            green: rgb(Frag::Green),
            amber: rgb(Frag::Amber),
            red: rgb(Frag::Red),
            blue: rgb(Frag::Blue),
            orange: rgb(Frag::Orange),
        }
    }
}

/// `$XDG_CONFIG_HOME/thegn/config.toml`, or `~/.config/thegn/...`.
fn config_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("thegn").join("config.toml"))
}

/// Parse the host's `#rgb` / `#rrggbb` rules (case-insensitive, whitespace
/// trimmed, and a leading `#` required). `None` if malformed.
fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.trim().strip_prefix('#')?;
    match h.len() {
        3 if h.is_ascii() => {
            let mut chars = h.chars();
            let channel = |c: char| u8::from_str_radix(&format!("{c}{c}"), 16).ok();
            Some((
                channel(chars.next()?)?,
                channel(chars.next()?)?,
                channel(chars.next()?)?,
            ))
        }
        6 if h.is_ascii() => Some((
            u8::from_str_radix(&h[0..2], 16).ok()?,
            u8::from_str_radix(&h[2..4], 16).ok()?,
            u8::from_str_radix(&h[4..6], 16).ok()?,
        )),
        _ => None,
    }
}

/// Lerp `hue` toward `base`: `t` is how much of the hue survives.
fn blend(hue: Rgb, base: Rgb, t: f32) -> Rgb {
    let c = |h: u8, b: u8| (b as f32 + (h as f32 - b as f32) * t).round() as u8;
    (c(hue.0, base.0), c(hue.1, base.1), c(hue.2, base.2))
}

// --- the slice of thegn's config.toml we read (serde-tolerant) ---

#[derive(Debug, Default, Deserialize)]
struct ConfigDoc {
    #[serde(default)]
    theme: ThemeSection,
}

#[derive(Debug, Default, Deserialize)]
struct ThemeSection {
    preset: Option<String>,
    accent: Option<String>,
    focus_border: Option<String>,
    #[serde(default)]
    colors: ColorsSection,
    #[serde(default)]
    hues: HuesSection,
}

#[derive(Debug, Default, Deserialize)]
struct ColorsSection {
    bg0: Option<String>,
    bg1: Option<String>,
    panel: Option<String>,
    panel2: Option<String>,
    raise: Option<String>,
    border: Option<String>,
    text: Option<String>,
    dim: Option<String>,
    faint: Option<String>,
    ghost: Option<String>,
    ghost2: Option<String>,
    ghost3: Option<String>,
    chip_fg: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct HuesSection {
    teal: Option<String>,
    magenta: Option<String>,
    purple: Option<String>,
    green: Option<String>,
    amber: Option<String>,
    red: Option<String>,
    blue: Option<String>,
    orange: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_round_trips_and_rejects_garbage() {
        assert_eq!(parse_hex("#6ee7d8"), Some((110, 231, 216)));
        assert_eq!(parse_hex("#6EE7D8"), Some((110, 231, 216)));
        assert_eq!(parse_hex("#fff"), Some((255, 255, 255)));
        assert_eq!(parse_hex("  #abc  "), Some((170, 187, 204)));
        assert_eq!(parse_hex("6ee7d8"), None);
        assert_eq!(parse_hex("#zzzzzz"), None);
    }

    #[test]
    fn prism_accent_and_focus_are_the_signature_teal() {
        let p = Theme::prism();
        assert_eq!(p.accent, (110, 231, 216));
        assert_eq!(p.focus, p.accent);
        assert_eq!(p.chip_fg, p.bg0);
    }

    #[test]
    fn apply_overlays_accent_and_colors() {
        let mut t = Theme::prism();
        let section = ThemeSection {
            preset: None,
            accent: Some("#ff0000".into()),
            focus_border: None,
            colors: ColorsSection {
                bg0: Some("#010203".into()),
                ..Default::default()
            },
            hues: HuesSection::default(),
        };
        t.apply(&section, &mut Vec::new());
        assert_eq!(t.accent, (255, 0, 0));
        assert_eq!(t.focus, Theme::prism().focus); // accent does not change focus
        assert_eq!(t.bg0, (1, 2, 3));
    }

    #[test]
    fn blend_endpoints() {
        assert_eq!(blend((100, 200, 0), (0, 100, 200), 0.0), (0, 100, 200));
        assert_eq!(blend((100, 200, 0), (0, 100, 200), 1.0), (100, 200, 0));
        assert_eq!(blend((100, 200, 0), (0, 100, 200), 0.5), (50, 150, 100));
    }

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let d = std::env::temp_dir()
                .join(format!("tg-kit-theme-{}-{id}-{name}", std::process::id()));
            std::fs::create_dir_all(&d).unwrap();
            Self(d)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn absent_is_no_config_not_prism_selection() {
        let dir = TempDir::new("absent");
        let l = Theme::load_from_path(&dir.path().join("nope.toml"));
        assert_eq!(l.outcome, ThemeOutcome::NoConfig);
        assert!(!l.is_faithful());
        assert_eq!(l.theme, Theme::prism());
    }

    #[test]
    fn explicit_prism_is_a_faithful_selection() {
        let l = Theme::from_config_str("[theme]\npreset = \"prism\"\n");
        assert_eq!(l.outcome, ThemeOutcome::Prism);
        assert!(l.is_faithful());
    }

    #[test]
    fn malformed_toml_is_failed_not_prism() {
        let l = Theme::from_config_str("[theme\n");
        assert!(matches!(
            l.outcome,
            ThemeOutcome::Failed(ThemeLoadError::Malformed(_))
        ));
        assert!(!l.is_faithful());
    }

    #[test]
    fn invalid_color_is_reported_and_rest_applies() {
        let l = Theme::from_config_str("[theme.colors]\nbg0 = \"nope\"\nbg1 = \"#010203\"\n");
        assert_eq!(l.theme.bg1, (1, 2, 3));
        assert_eq!(l.theme.bg0, Theme::prism().bg0);
        assert_eq!(
            l.issues,
            vec![ThemeIssue::InvalidColor {
                field: "colors.bg0",
                value: "nope".into()
            }]
        );
        assert!(!l.is_faithful());
    }

    #[test]
    fn default_accent_does_not_clobber() {
        let mut t = Theme::prism();
        t.accent = (1, 2, 3);
        t.focus = (4, 5, 6);
        let section = ThemeSection {
            preset: None,
            accent: Some("#6ee7d8".into()),
            focus_border: Some("#9bd1ff".into()),
            colors: ColorsSection::default(),
            hues: HuesSection::default(),
        };
        t.apply(&section, &mut Vec::new());
        assert_eq!((t.accent, t.focus), ((1, 2, 3), (4, 5, 6)));
    }

    #[cfg(not(feature = "host-presets"))]
    #[test]
    fn non_prism_preset_without_catalog_is_reported() {
        let l = Theme::from_config_str("[theme]\npreset = \"light\"\n");
        assert_eq!(l.issues, vec![ThemeIssue::UnresolvedPreset("light".into())]);
        assert!(!l.is_faithful());
    }

    #[test]
    fn unknown_preset_is_reported() {
        let l = Theme::from_config_str("[theme]\npreset = \"no-such-theme\"\n");
        assert_eq!(
            l.issues,
            vec![ThemeIssue::UnresolvedPreset("no-such-theme".into())]
        );
        assert_eq!(l.theme, Theme::prism());
    }

    #[test]
    fn preset_name_is_not_trimmed() {
        let l = Theme::from_config_str("[theme]\npreset = \" light \"\n");
        assert_eq!(
            l.issues,
            vec![ThemeIssue::UnresolvedPreset(" light ".into())]
        );
    }

    #[test]
    fn size_cap_is_exact() {
        let dir = TempDir::new("cap");
        let pad = |n: usize| format!("#{}", "x".repeat(n - 1));
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, pad(MAX_CONFIG_BYTES as usize)).unwrap();
        // exactly at the cap is read (and is valid TOML: a comment)
        assert_eq!(Theme::load_from_path(&ok).outcome, ThemeOutcome::Prism);
        let big = dir.path().join("big.toml");
        std::fs::write(&big, pad(MAX_CONFIG_BYTES as usize + 1)).unwrap();
        assert_eq!(
            Theme::load_from_path(&big).outcome,
            ThemeOutcome::Failed(ThemeLoadError::Oversize)
        );
    }

    #[test]
    fn invalid_utf8_and_non_regular_are_typed() {
        let dir = TempDir::new("kinds");
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            Theme::load_from_path(&bad).outcome,
            ThemeOutcome::Failed(ThemeLoadError::InvalidUtf8)
        );
        // a directory is not a regular file
        assert_eq!(
            Theme::load_from_path(dir.path()).outcome,
            ThemeOutcome::Failed(ThemeLoadError::NotRegular)
        );
    }
}
