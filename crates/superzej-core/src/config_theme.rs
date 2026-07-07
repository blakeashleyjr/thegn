//! Terminal display / glyph `[theme]` config enums.
//!
//! These pin how superzej talks to the *outer* terminal: color fidelity
//! ([`ColorMode`]), chrome glyph fidelity ([`GlyphMode`]), curly-underline
//! support ([`UndercurlMode`]), and the sidebar agent-marker style
//! ([`AgentGlyphs`]). They live in this sibling module (rather than the pinned
//! `config.rs` god-file) and are re-exported from `config` so the canonical
//! `superzej_core::config::{ColorMode, …}` paths keep working. The runtime
//! resolution of these against detected terminal capabilities lives in
//! `crate::termcaps` (glyph level) and `crate::theme` (agent-marker style).

use crate::config::{config_enum, config_warn};

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

#[cfg(test)]
mod tests {
    use super::*;

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
