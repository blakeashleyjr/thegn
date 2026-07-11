//! Terminal display / glyph `[theme]` config enums.
//!
//! These pin how thegn talks to the *outer* terminal: color fidelity
//! ([`ColorMode`]), chrome glyph fidelity ([`GlyphMode`]), curly-underline
//! support ([`UndercurlMode`]), and the sidebar agent-marker style
//! ([`AgentGlyphs`]). They live in this sibling module (rather than the pinned
//! `config.rs` god-file) and are re-exported from `config` so the canonical
//! `thegn_core::config::{ColorMode, …}` paths keep working. The runtime
//! resolution of these against detected terminal capabilities lives in
//! `crate::termcaps` (glyph level) and `crate::theme` (agent-marker style).

use crate::config::{config_enum, config_warn};
use serde::{Deserialize, Serialize};

config_enum! {
    /// Which mascot the loading splash draws above the wordmark (when the
    /// center is tall enough): "owl" (default — the perched sentinel),
    /// "knight" (the Sutton Hoo helm bust), or "off" to disable the sprite
    /// and keep the plain wordmark splash.
    pub enum MascotKind: "mascot" {
        Owl = "owl",
        Knight = "knight",
        Off = "off" | "none" | "disabled",
    } default = Owl;
}
config_enum! {
    /// Splash-mascot motion. "blink" lets the owl blink — evaluated only
    /// when a wake already redraws the splash, so an idle loop stays idle
    /// (the 0%-idle invariant; the owl holds perfectly still until
    /// something happens, which is the joke). "still" pins the eyes open.
    pub enum MascotMotion: "mascot motion" {
        Blink = "blink",
        Still = "still" | "static",
    } default = Blink;
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
    /// Marker style for the per-worktree agent indicator in the sidebar tree.
    /// "letter" (default) uses universal 1–2 letter marks (C, Cx, Y, Lg) that
    /// render in any font; "symbol" uses compact Unicode marks (⊞ ↟ ✎ ±) for
    /// Nerd-Font / rich terminals (degrading to letters on ASCII-only ones);
    /// "auto" uses symbols only on a confirmed-modern emulator.
    pub enum AgentGlyphs: "agent glyphs" {
        Letter = "letter" | "letters" | "text",
        Symbol = "symbol" | "symbols" | "unicode",
        Auto = "auto",
    } default = Letter;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mascot_kind_parse_aliases_default_and_error() {
        assert_eq!(MascotKind::default(), MascotKind::Owl);
        for (s, want) in [
            ("owl", MascotKind::Owl),
            ("knight", MascotKind::Knight),
            ("off", MascotKind::Off),
            ("none", MascotKind::Off),
            ("DISABLED", MascotKind::Off),
        ] {
            assert_eq!(MascotKind::from_str_validated(s).unwrap(), want, "{s}");
        }
        assert!(MascotKind::from_str_validated("dragon").is_err());
    }

    #[test]
    fn mascot_motion_parse_aliases_default_and_error() {
        assert_eq!(MascotMotion::default(), MascotMotion::Blink);
        for (s, want) in [
            ("blink", MascotMotion::Blink),
            ("still", MascotMotion::Still),
            ("static", MascotMotion::Still),
        ] {
            assert_eq!(MascotMotion::from_str_validated(s).unwrap(), want, "{s}");
        }
        assert!(MascotMotion::from_str_validated("dance").is_err());
    }

    #[test]
    fn mascot_enums_canonical_string_round_trip() {
        for k in [MascotKind::Owl, MascotKind::Knight, MascotKind::Off] {
            assert_eq!(MascotKind::from_str_validated(k.as_str()).unwrap(), k);
            assert_eq!(k.to_string(), k.as_str());
        }
        for m in [MascotMotion::Blink, MascotMotion::Still] {
            assert_eq!(MascotMotion::from_str_validated(m.as_str()).unwrap(), m);
            assert_eq!(m.to_string(), m.as_str());
        }
    }

    #[test]
    fn agent_glyphs_parse_aliases_default_and_error() {
        assert_eq!(AgentGlyphs::default(), AgentGlyphs::Letter);
        for (s, want) in [
            ("letter", AgentGlyphs::Letter),
            ("letters", AgentGlyphs::Letter),
            ("text", AgentGlyphs::Letter),
            ("symbol", AgentGlyphs::Symbol),
            ("SYMBOLS", AgentGlyphs::Symbol),
            ("unicode", AgentGlyphs::Symbol),
            ("auto", AgentGlyphs::Auto),
        ] {
            assert_eq!(AgentGlyphs::from_str_validated(s).unwrap(), want, "{s}");
        }
        assert!(AgentGlyphs::from_str_validated("nerd").is_err());
    }

    #[test]
    fn agent_glyphs_canonical_string_and_display_round_trip() {
        for g in [AgentGlyphs::Letter, AgentGlyphs::Symbol, AgentGlyphs::Auto] {
            assert_eq!(AgentGlyphs::from_str_validated(g.as_str()).unwrap(), g);
            assert_eq!(g.to_string(), g.as_str());
        }
        // Canonical forms are the primary spellings, not the aliases.
        assert_eq!(AgentGlyphs::Letter.as_str(), "letter");
        assert_eq!(AgentGlyphs::Symbol.as_str(), "symbol");
    }

    #[test]
    fn agent_glyphs_serde_round_trips_via_toml() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Holder {
            g: AgentGlyphs,
        }
        let h = Holder {
            g: AgentGlyphs::Symbol,
        };
        let s = toml::to_string(&h).unwrap();
        assert!(s.contains("g = \"symbol\""), "{s}");
        let back: Holder = toml::from_str(&s).unwrap();
        assert_eq!(back, h);
        // An unknown value warns-and-defaults rather than failing the parse.
        let def: Holder = toml::from_str("g = \"bogus\"").unwrap();
        assert_eq!(def.g, AgentGlyphs::Letter);
    }
}
