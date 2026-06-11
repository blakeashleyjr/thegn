//! The superzej design system: one storm-blue palette for the native host's
//! in-process chrome (sidebar, panel, tabbar, statusbar).
//!
//! Colors are "R;G;B" fragments ready for `\x1b[38;2;{}m` / `\x1b[48;2;{}m`.
//! Values are the mockup's gamut-mapped sRGB — do not re-derive from oklch.
#![allow(dead_code)]

// ---- surfaces ----
pub const BG0: &str = "20;22;31"; // deepest background
pub const BG1: &str = "26;29;41"; // bar / surface
pub const PANEL: &str = "33;36;50"; // panel
pub const PANEL2: &str = "40;44;62"; // panel-2 / selection
pub const RAISE: &str = "47;52;72"; // raised / hover
pub const BORDER: &str = "62;68;92"; // borders / rules

// ---- text ramp ----
pub const TEXT: &str = "224;228;240"; // primary
pub const DIM: &str = "154;160;180"; // secondary
pub const FAINT: &str = "101;107;128"; // tertiary / labels
pub const GHOST: &str = "72;77;96"; // connectors / disabled

// ---- accents ----
pub const TEAL: &str = "118;238;222"; // default accent (#76eede)
pub const MAGENTA: &str = "240;131;186";
pub const PURPLE: &str = "178;148;250";
pub const GREEN: &str = "121;227;165";
pub const AMBER: &str = "240;198;116";
pub const RED: &str = "247;118;142";

// ---- agent identity hues (codex=GREEN, aider=AMBER, gemini=PURPLE) ----
pub const CORAL: &str = "240;145;125"; // claude
pub const BLUE: &str = "120;170;245"; // shell

// ---- focus / frame defaults (overridable via `[theme]`, see Palette) ----
pub const FRAME: &str = "170;177;196"; // pane/edge frame lines (#aab1c4)
pub const FOCUS: &str = "155;209;255"; // focused frame + highlights (#9bd1ff)

pub const RESET: &str = "\u{1b}[0m";

/// The resolved chrome palette: every surface, text, and frame color the host
/// renders with, as "R;G;B" fragments. Defaults mirror the constants above;
/// `Config::palette()` overlays any `[theme]` / `[theme.colors]` overrides so
/// chrome code reads colors from here instead of the constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub bg0: String,
    pub bg1: String,
    pub panel: String,
    pub panel2: String,
    pub raise: String,
    /// Frame lines around panes and chrome edges (light grey by default).
    pub border: String,
    /// The frame/highlight color of whatever owns focus (light blue default).
    pub focus: String,
    pub text: String,
    pub dim: String,
    pub faint: String,
    pub ghost: String,
    pub accent: String,
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            bg0: BG0.into(),
            bg1: BG1.into(),
            panel: PANEL.into(),
            panel2: PANEL2.into(),
            raise: RAISE.into(),
            border: FRAME.into(),
            focus: FOCUS.into(),
            text: TEXT.into(),
            dim: DIM.into(),
            faint: FAINT.into(),
            ghost: GHOST.into(),
            accent: TEAL.into(),
        }
    }
}

/// Build a palette from 12 "R;G;B" fragments, in field order.
fn pal(c: [&str; 12]) -> Palette {
    Palette {
        bg0: c[0].into(),
        bg1: c[1].into(),
        panel: c[2].into(),
        panel2: c[3].into(),
        raise: c[4].into(),
        border: c[5].into(),
        focus: c[6].into(),
        text: c[7].into(),
        dim: c[8].into(),
        faint: c[9].into(),
        ghost: c[10].into(),
        accent: c[11].into(),
    }
}

/// The selectable preset names, in cycle order.
pub const PRESETS: &[&str] = &["storm", "light", "abyss", "ember", "aurora"];

/// A named palette preset. `[theme.colors]` overrides still apply on top, so
/// every preset stays fully customizable. `None` for unknown names.
pub fn preset(name: &str) -> Option<Palette> {
    Some(match name {
        // The storm-blue default.
        "storm" | "" => Palette::default(),
        // A paper-bright light mode with an ink text ramp and deep-teal accent.
        "light" => pal([
            "245;246;250",
            "236;238;245",
            "228;231;240",
            "213;218;232",
            "201;208;226",
            "148;156;180",
            "38;99;176",
            "30;34;46",
            "88;95;114",
            "136;143;162",
            "182;188;203",
            "0;138;125",
        ]),
        // True-black OLED with an electric cyan focus and mint accent.
        "abyss" => pal([
            "0;0;0",
            "8;10;14",
            "13;16;22",
            "22;27;36",
            "31;37;49",
            "56;64;84",
            "0;229;255",
            "214;222;236",
            "141;151;171",
            "94;102;120",
            "58;64;80",
            "94;234;212",
        ]),
        // Warm charcoal with amber focus and coral accent — firelight.
        "ember" => pal([
            "24;20;18",
            "30;25;22",
            "38;31;27",
            "50;40;34",
            "61;49;42",
            "106;88;75",
            "255;176;102",
            "240;230;220",
            "181;166;151",
            "131;116;103",
            "89;77;67",
            "255;122;89",
        ]),
        // Deep violet night with lavender focus and mint accent — aurora.
        "aurora" => pal([
            "16;14;26",
            "21;18;34",
            "28;24;44",
            "38;33;58",
            "49;43;73",
            "86;76;120",
            "168;130;255",
            "228;226;244",
            "163;159;187",
            "113;109;139",
            "75;71;101",
            "94;245;190",
        ]),
        _ => return None,
    })
}

/// Foreground escape for an "R;G;B" triple.
pub fn fg(rgb: &str) -> String {
    format!("\u{1b}[38;2;{rgb}m")
}

/// Background escape for an "R;G;B" triple.
pub fn bg(rgb: &str) -> String {
    format!("\u{1b}[48;2;{rgb}m")
}

pub fn bold() -> &'static str {
    "\u{1b}[1m"
}

/// Lerp a hue toward an arbitrary base color: `t` is how much of the hue
/// survives (0.0 = pure base, 1.0 = pure hue). Use this for alpha-style tints
/// on tinted surfaces (e.g. a focus pill on the panel background), where
/// blending toward BG0 would punch a dark hole in the surface.
pub fn blend_over(hue: &str, base: &str, t: f32) -> String {
    let p = |s: &str| -> [f32; 3] {
        let mut it = s.split(';').map(|n| n.parse::<f32>().unwrap_or(0.0));
        [
            it.next().unwrap_or(0.0),
            it.next().unwrap_or(0.0),
            it.next().unwrap_or(0.0),
        ]
    };
    let (h, b) = (p(hue), p(base));
    let c = |i: usize| (b[i] + (h[i] - b[i]) * t).round() as u8;
    format!("{};{};{}", c(0), c(1), c(2))
}

/// Lerp a hue toward BG0: `t` is how much of the hue survives (0.0 = pure
/// BG0, 1.0 = pure hue). `blend(hue, 0.16)` approximates the mockup's
/// 16%-alpha pill/selection tints on the storm-blue base.
pub fn blend(hue: &str, t: f32) -> String {
    blend_over(hue, BG0, t)
}

/// Identity hue for an agent/tool name. Known names get their signature hue;
/// unknown ones get a stable hash-picked hue so a custom agent always looks
/// the same.
pub fn agent_hue(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        "claude" => CORAL,
        "codex" => GREEN,
        "aider" => AMBER,
        "gemini" => PURPLE,
        "shell" | "__shell__" => BLUE,
        "lazygit" => GREEN,
        "yazi" => PURPLE,
        "editor" => TEAL,
        "diff" => AMBER,
        other => {
            const HUES: [&str; 6] = [CORAL, GREEN, AMBER, PURPLE, BLUE, MAGENTA];
            HUES[(fnv1a(other) % HUES.len() as u64) as usize]
        }
    }
}

/// Identity glyph for an agent/tool name (1–2 cells). Unknown names fall back
/// to their first letter, uppercased.
pub fn agent_glyph(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "claude" => "C".into(),
        "codex" => "Cx".into(),
        "aider" => "Ai".into(),
        "gemini" => "G".into(),
        "shell" | "__shell__" => "$".into(),
        "lazygit" => "↟".into(),
        "yazi" => "⊞".into(),
        "editor" => "✎".into(),
        "diff" => "±".into(),
        other => other
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase().to_string())
            .unwrap_or_else(|| "•".into()),
    }
}

/// A filled identity chip: the glyph in BG0 on the agent's hue.
pub fn glyph_square(glyph: &str, hue: &str) -> String {
    format!("{}{} {glyph} {RESET}", bg(hue), fg(BG0))
}

/// A kbd-hint strip: keys in `accent` on PANEL, labels DIM, "·" GHOST between
/// pairs — e.g. `kbd(&[("d","full diff"),("c","PR")], TEAL)`.
pub fn kbd(pairs: &[(&str, &str)], accent: &str) -> String {
    let mut out = String::new();
    for (i, (key, label)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str(&format!(" {}·{RESET} ", fg(GHOST)));
        }
        out.push_str(&format!(
            "{}{} {key} {RESET} {}{label}{RESET}",
            bg(PANEL),
            fg(accent),
            fg(DIM)
        ));
    }
    out
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_and_cycle_names_are_complete() {
        for name in PRESETS {
            let p = preset(name).expect(name);
            // Every fragment parses as R;G;B.
            for frag in [&p.bg0, &p.text, &p.accent, &p.focus] {
                assert_eq!(frag.split(';').count(), 3, "{name}: {frag}");
            }
        }
        assert_eq!(preset(""), Some(Palette::default()));
        assert!(preset("nope").is_none());
        // Light mode really is light (bg brighter than text).
        let l = preset("light").unwrap();
        let lum = |s: &str| s.split(';').map(|n| n.parse::<u32>().unwrap()).sum::<u32>();
        assert!(lum(&l.bg0) > lum(&l.text));
    }

    #[test]
    fn blend_endpoints() {
        assert_eq!(blend(GREEN, 0.0), BG0);
        assert_eq!(blend(GREEN, 1.0), GREEN);
    }

    #[test]
    fn blend_is_tinted_toward_bg() {
        // 16% green tint stays dark but greener than BG0.
        let t = blend(GREEN, 0.16);
        let g: Vec<u8> = t.split(';').map(|n| n.parse().unwrap()).collect();
        assert!(g[1] > 22 && g[1] < 121); // between BG0.g and GREEN.g
    }

    #[test]
    fn blend_over_endpoints_and_midpoint() {
        assert_eq!(blend_over(GREEN, PANEL, 0.0), PANEL);
        assert_eq!(blend_over(GREEN, PANEL, 1.0), GREEN);
        // Midpoint is the per-channel average (rounded).
        assert_eq!(blend_over("100;200;0", "0;100;200", 0.5), "50;150;100");
    }

    #[test]
    fn blend_delegates_to_blend_over_bg0() {
        for t in [0.0, 0.16, 0.5, 1.0] {
            assert_eq!(blend(AMBER, t), blend_over(AMBER, BG0, t));
        }
    }

    #[test]
    fn blend_over_tolerates_malformed_fragments() {
        // Unparseable channels degrade to 0 instead of panicking.
        assert_eq!(blend_over("not;a;color", "0;0;0", 1.0), "0;0;0");
        assert_eq!(blend_over("10;20", "0;0;0", 1.0), "10;20;0");
    }

    #[test]
    fn agent_hue_known_and_stable() {
        assert_eq!(agent_hue("claude"), CORAL);
        assert_eq!(agent_hue("Claude"), CORAL);
        assert_eq!(agent_hue("shell"), BLUE);
        assert_eq!(agent_hue("__shell__"), BLUE);
        // Unknown names: stable across calls and in the identity palette.
        let h = agent_hue("my-custom-agent");
        assert_eq!(h, agent_hue("my-custom-agent"));
        assert!([CORAL, GREEN, AMBER, PURPLE, BLUE, MAGENTA].contains(&h));
    }

    #[test]
    fn agent_glyph_known_and_fallback() {
        assert_eq!(agent_glyph("claude"), "C");
        assert_eq!(agent_glyph("codex"), "Cx");
        assert_eq!(agent_glyph("shell"), "$");
        assert_eq!(agent_glyph("goose"), "G");
    }

    #[test]
    fn escape_helpers() {
        assert_eq!(fg("1;2;3"), "\u{1b}[38;2;1;2;3m");
        assert_eq!(bg("1;2;3"), "\u{1b}[48;2;1;2;3m");
        assert_eq!(bold(), "\u{1b}[1m");
    }

    #[test]
    fn every_agent_hue_and_glyph_arm() {
        for (name, hue, glyph) in [
            ("claude", CORAL, "C"),
            ("codex", GREEN, "Cx"),
            ("aider", AMBER, "Ai"),
            ("gemini", PURPLE, "G"),
            ("lazygit", GREEN, "↟"),
            ("yazi", PURPLE, "⊞"),
            ("editor", TEAL, "✎"),
            ("diff", AMBER, "±"),
        ] {
            assert_eq!(agent_hue(name), hue, "{name} hue");
            assert_eq!(agent_glyph(name), glyph, "{name} glyph");
        }
        // empty fallback glyph
        assert_eq!(agent_glyph(""), "•");
    }

    #[test]
    fn glyph_square_and_kbd_render() {
        let sq = glyph_square("C", CORAL);
        assert!(sq.contains("C") && sq.contains(RESET));
        let strip = kbd(&[("d", "diff"), ("c", "PR")], TEAL);
        // both labels + the "·" separator between pairs.
        assert!(strip.contains("diff") && strip.contains("PR") && strip.contains('·'));
    }
}
