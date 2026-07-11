//! Shared theme: semantic tokens → sRGB, rendered as ratatui [`Style`]s.
//!
//! [`Theme::prism`] mirrors thegn's default chrome palette
//! (`thegn-core/src/theme.rs`). Embedded, the host converts its live
//! `Palette` into a [`Theme`] (so theme-cycling and user `[theme.colors]`
//! overrides flow through). Standalone, [`Theme::load_thegn_config`] reads
//! the same `config.toml` so an app run on its own still matches the user's
//! thegn look — without this crate depending on `thegn-core`.

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
    /// [`Theme::prism`]. Only `[theme] accent`/`focus_border` and the
    /// `[theme.colors]` / `[theme.hues]` `#rrggbb` overrides are read; a named
    /// non-prism `preset` standalone keeps the prism base (the embedded host
    /// path handles every preset exactly). Tolerant: any parse error → prism.
    pub fn load_thegn_config() -> Theme {
        let mut theme = Theme::prism();
        let Some(path) = config_path() else {
            return theme;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return theme;
        };
        if let Ok(doc) = toml::from_str::<ConfigDoc>(&text) {
            theme.apply(&doc.theme);
        }
        theme
    }

    /// Overlay `[theme]` config fields (each optional `#rrggbb`).
    fn apply(&mut self, t: &ThemeSection) {
        let set = |slot: &mut Rgb, hex: &Option<String>| {
            if let Some(rgb) = hex.as_deref().and_then(parse_hex) {
                *slot = rgb;
            }
        };
        // accent recolors both the accent and (by thegn default) the focus.
        if let Some(rgb) = t.accent.as_deref().and_then(parse_hex) {
            self.accent = rgb;
            self.focus = rgb;
        }
        set(&mut self.focus, &t.focus_border);
        let c = &t.colors;
        set(&mut self.bg0, &c.bg0);
        set(&mut self.bg1, &c.bg1);
        set(&mut self.panel, &c.panel);
        set(&mut self.panel2, &c.panel2);
        set(&mut self.raise, &c.raise);
        set(&mut self.border, &c.border);
        set(&mut self.text, &c.text);
        set(&mut self.dim, &c.dim);
        set(&mut self.faint, &c.faint);
        set(&mut self.ghost, &c.ghost);
        set(&mut self.ghost2, &c.ghost2);
        set(&mut self.ghost3, &c.ghost3);
        set(&mut self.chip_fg, &c.chip_fg);
        let h = &t.hues;
        set(&mut self.teal, &h.teal);
        set(&mut self.magenta, &h.magenta);
        set(&mut self.purple, &h.purple);
        set(&mut self.green, &h.green);
        set(&mut self.amber, &h.amber);
        set(&mut self.red, &h.red);
        set(&mut self.blue, &h.blue);
        set(&mut self.orange, &h.orange);
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

/// Parse `#rrggbb` (case-insensitive). `None` if malformed.
fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some((r, g, b))
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
        assert_eq!(parse_hex("6EE7D8"), Some((110, 231, 216)));
        assert_eq!(parse_hex("#fff"), None);
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
            accent: Some("#ff0000".into()),
            focus_border: None,
            colors: ColorsSection {
                bg0: Some("#010203".into()),
                ..Default::default()
            },
            hues: HuesSection::default(),
        };
        t.apply(&section);
        assert_eq!(t.accent, (255, 0, 0));
        assert_eq!(t.focus, (255, 0, 0)); // accent recolors focus too
        assert_eq!(t.bg0, (1, 2, 3));
    }

    #[test]
    fn blend_endpoints() {
        assert_eq!(blend((100, 200, 0), (0, 100, 200), 0.0), (0, 100, 200));
        assert_eq!(blend((100, 200, 0), (0, 100, 200), 1.0), (100, 200, 0));
        assert_eq!(blend((100, 200, 0), (0, 100, 200), 0.5), (50, 150, 100));
    }
}
