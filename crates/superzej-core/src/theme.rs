//! The superzej design system: token palette for the native host's
//! in-process chrome (sidebar, panel, tabbar, statusbar, layers).
//!
//! Colors are "R;G;B" fragments ready for `\x1b[38;2;{}m` / `\x1b[48;2;{}m`.
//! Values are the mockup's gamut-mapped sRGB — do not re-derive from oklch.
//!
//! The default preset is "prism" (the max-TUI redesign palette): a six-step
//! foreground ramp (text → dim → faint → ghost → ghost2 → ghost3), five
//! background shades (bg0 → raise), eight semantic hues, a commit-heat ramp,
//! and shadow/chip tokens for layer compositing. Legacy presets keep their
//! 12 original slots; the extension tokens are derived via [`extend_palette`].
#![allow(dead_code)]

// ---- legacy storm surfaces (the pre-prism default, kept as a preset) ----
pub const BG0: &str = "20;22;31"; // deepest background
pub const BG1: &str = "26;29;41"; // bar / surface
pub const PANEL: &str = "33;36;50"; // panel
pub const PANEL2: &str = "40;44;62"; // panel-2 / selection
pub const RAISE: &str = "47;52;72"; // raised / hover
pub const BORDER: &str = "62;68;92"; // borders / rules

// ---- legacy text ramp ----
pub const TEXT: &str = "224;228;240"; // primary
pub const DIM: &str = "154;160;180"; // secondary
pub const FAINT: &str = "101;107;128"; // tertiary / labels
pub const GHOST: &str = "72;77;96"; // connectors / disabled

// ---- legacy accents ----
pub const TEAL: &str = "118;238;222"; // storm accent (#76eede)
pub const MAGENTA: &str = "240;131;186";
pub const PURPLE: &str = "178;148;250";
pub const GREEN: &str = "121;227;165";
pub const AMBER: &str = "240;198;116";
pub const RED: &str = "247;118;142";

// ---- agent identity hues (codex=GREEN, aider=AMBER, gemini=PURPLE) ----
pub const CORAL: &str = "240;145;125"; // claude
pub const BLUE: &str = "120;170;245"; // shell

// ---- legacy focus / frame defaults ----
pub const FRAME: &str = "170;177;196"; // pane/edge frame lines (#aab1c4)
pub const FOCUS: &str = "155;209;255"; // focused frame + highlights (#9bd1ff)

// ---- prism: the redesign default (mockup-exact sRGB) ----
// surfaces b0..b4
pub const P_BG0: &str = "11;14;22"; // #0b0e16
pub const P_BG1: &str = "16;20;31"; // #10141f
pub const P_PANEL: &str = "21;26;40"; // #151a28
pub const P_PANEL2: &str = "26;32;49"; // #1a2031
pub const P_RAISE: &str = "34;41;66"; // #222942
// fg ramp t d f g g2 g3
pub const P_TEXT: &str = "237;240;248"; // #edf0f8
pub const P_DIM: &str = "198;204;219"; // #c6ccdb (brighter secondary — readable on dark panels)
pub const P_FAINT: &str = "122;129;151"; // #7a8197
pub const P_GHOST: &str = "78;85;110"; // #4e556e
// ghost2/ghost3 are the structural floor (glyph scaffolding, rules, fills), but
// chrome still tints some metadata with them — the old values (#343a52/#232940)
// read as grey-on-grey on bg1. Lifted toward `ghost` (kept strictly below it so
// the contrast ramp still descends) so dim text is legible everywhere at once.
pub const P_GHOST2: &str = "70;78;102"; // #464e66
pub const P_GHOST3: &str = "58;65;86"; // #3a4156
// layer compositing
pub const P_SHADOW_BG: &str = "5;7;12"; // #05070c
pub const P_SHADOW_FG: &str = "28;34;51"; // #1c2233
// semantic hues
pub const HUE_TEAL: &str = "110;231;216"; // #6ee7d8 (also the default accent)
pub const HUE_MAGENTA: &str = "239;143;196"; // #ef8fc4
pub const HUE_PURPLE: &str = "182;156;242"; // #b69cf2
pub const HUE_GREEN: &str = "127;220;160"; // #7fdca0
pub const HUE_AMBER: &str = "230;194;100"; // #e6c264
pub const HUE_RED: &str = "239;111;111"; // #ef6f6f
pub const HUE_BLUE: &str = "127;180;236"; // #7fb4ec
pub const HUE_ORANGE: &str = "240;157;106"; // #f09d6a
// commit-heat ramp h0..h4
pub const P_HEAT: [&str; 5] = [
    "26;35;48",    // #1a2330
    "30;64;52",    // #1e4034
    "44;106;78",   // #2c6a4e
    "69;164;114",  // #45a472
    "138;232;173", // #8ae8ad
];

pub const RESET: &str = "\u{1b}[0m";

/// The eight semantic hues, addressable by name. Chrome code uses these for
/// status/identity coloring instead of hardcoding RGB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hue {
    Teal,
    Magenta,
    Purple,
    Green,
    Amber,
    Red,
    Blue,
    Orange,
}

impl Hue {
    pub const ALL: [Hue; 8] = [
        Hue::Teal,
        Hue::Magenta,
        Hue::Purple,
        Hue::Green,
        Hue::Amber,
        Hue::Red,
        Hue::Blue,
        Hue::Orange,
    ];
}

/// The semantic-hue set of a palette: identity and status colors that ride
/// with the preset (light mode swaps these for darker, paper-legible values).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hues {
    pub teal: String,
    pub magenta: String,
    pub purple: String,
    pub green: String,
    pub amber: String,
    pub red: String,
    pub blue: String,
    pub orange: String,
}

impl Hues {
    /// The prism hue set (mockup values) — the default for dark presets.
    pub fn prism() -> Hues {
        Hues {
            teal: HUE_TEAL.into(),
            magenta: HUE_MAGENTA.into(),
            purple: HUE_PURPLE.into(),
            green: HUE_GREEN.into(),
            amber: HUE_AMBER.into(),
            red: HUE_RED.into(),
            blue: HUE_BLUE.into(),
            orange: HUE_ORANGE.into(),
        }
    }
}

/// The resolved chrome palette: every surface, text, frame, hue, and
/// compositing color the host renders with, as "R;G;B" fragments. Defaults
/// mirror the prism preset; `Config::palette()` overlays any `[theme]` /
/// `[theme.colors]` / `[theme.hues]` overrides so chrome code reads colors
/// from here instead of the constants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub bg0: String,
    pub bg1: String,
    pub panel: String,
    pub panel2: String,
    pub raise: String,
    /// Frame lines around panes and chrome edges.
    pub border: String,
    /// The frame/highlight color of whatever owns focus.
    pub focus: String,
    pub text: String,
    pub dim: String,
    pub faint: String,
    pub ghost: String,
    pub accent: String,
    /// Foreground ramp step below ghost (mockup g2): structural glyphs.
    pub ghost2: String,
    /// Deepest structural foreground (mockup g3): rules, fills, tracks.
    pub ghost3: String,
    /// Background of layer shadow cells.
    pub shadow_bg: String,
    /// Foreground of layer shadow cells (content kept, darkened).
    pub shadow_fg: String,
    /// Text inside inverse chips (≈ bg0 so chips read as filled).
    pub chip_fg: String,
    /// Semantic hues (identity + status colors).
    pub hues: Hues,
    /// Commit-calendar heat ramp, cold → hot.
    pub heat: [String; 5],
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            bg0: P_BG0.into(),
            bg1: P_BG1.into(),
            panel: P_PANEL.into(),
            panel2: P_PANEL2.into(),
            raise: P_RAISE.into(),
            border: P_GHOST.into(),
            focus: HUE_TEAL.into(),
            text: P_TEXT.into(),
            dim: P_DIM.into(),
            faint: P_FAINT.into(),
            ghost: P_GHOST.into(),
            accent: HUE_TEAL.into(),
            ghost2: P_GHOST2.into(),
            ghost3: P_GHOST3.into(),
            shadow_bg: P_SHADOW_BG.into(),
            shadow_fg: P_SHADOW_FG.into(),
            chip_fg: P_BG0.into(),
            hues: Hues::prism(),
            heat: P_HEAT.map(String::from),
        }
    }
}

impl Palette {
    /// Resolve a semantic hue to its "R;G;B" fragment.
    pub fn hue(&self, h: Hue) -> &str {
        match h {
            Hue::Teal => &self.hues.teal,
            Hue::Magenta => &self.hues.magenta,
            Hue::Purple => &self.hues.purple,
            Hue::Green => &self.hues.green,
            Hue::Amber => &self.hues.amber,
            Hue::Red => &self.hues.red,
            Hue::Blue => &self.hues.blue,
            Hue::Orange => &self.hues.orange,
        }
    }

    /// Heat-ramp color for a level 0..=4 (clamped).
    pub fn heat(&self, level: usize) -> &str {
        &self.heat[level.min(4)]
    }

    /// The accent selection-row tint: accent at ~16% over bg1 (mockup x-sel).
    pub fn sel_accent(&self) -> String {
        blend_over(&self.accent, &self.bg1, 0.16)
    }

    /// A hue-tinted selection row: `alpha` is how much hue survives
    /// (mockup x-selm = magenta 0.13, x-selr = red 0.14).
    pub fn sel(&self, h: Hue, alpha: f32) -> String {
        blend_over(self.hue(h), &self.bg1, alpha)
    }

    /// Identity hue for an agent/tool name, resolved through this palette so
    /// light mode (and user hue overrides) recolor agents consistently.
    pub fn agent_hue(&self, name: &str) -> &str {
        self.hue(agent_hue_slot(name))
    }
}

/// Fill any empty extension tokens from the 12 legacy slots. Pure and
/// preset-agnostic: every derivation blends relative to the palette's own
/// surfaces (never toward absolute black), so light mode stays light.
pub fn extend_palette(p: &mut Palette) {
    if p.ghost2.is_empty() {
        p.ghost2 = blend_over(&p.ghost, &p.bg0, 0.62);
    }
    if p.ghost3.is_empty() {
        p.ghost3 = blend_over(&p.ghost, &p.bg0, 0.38);
    }
    if p.shadow_bg.is_empty() {
        // 45% of bg0 — darker than every surface but never pure black.
        p.shadow_bg = blend_over("0;0;0", &p.bg0, 0.55);
    }
    if p.shadow_fg.is_empty() {
        p.shadow_fg = blend_over(&p.dim, &p.shadow_bg, 0.22);
    }
    if p.chip_fg.is_empty() {
        p.chip_fg = p.bg0.clone();
    }
    let d = Hues::prism();
    let h = &mut p.hues;
    for (slot, def) in [
        (&mut h.teal, d.teal),
        (&mut h.magenta, d.magenta),
        (&mut h.purple, d.purple),
        (&mut h.green, d.green),
        (&mut h.amber, d.amber),
        (&mut h.red, d.red),
        (&mut h.blue, d.blue),
        (&mut h.orange, d.orange),
    ] {
        if slot.is_empty() {
            *slot = def;
        }
    }
    let green = p.hues.green.clone();
    for (i, t) in [0.04, 0.22, 0.45, 0.68, 0.95].into_iter().enumerate() {
        if p.heat[i].is_empty() {
            p.heat[i] = blend_over(&green, &p.panel, t);
        }
    }
}

/// Build a palette from 12 "R;G;B" fragments, in field order. Extension
/// tokens are left empty for [`extend_palette`] to derive.
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
        ghost2: String::new(),
        ghost3: String::new(),
        shadow_bg: String::new(),
        shadow_fg: String::new(),
        chip_fg: String::new(),
        hues: Hues::default(),
        heat: Default::default(),
    }
}

/// The selectable preset names, in cycle order.
pub const PRESETS: &[&str] = &["prism", "storm", "light", "abyss", "ember", "aurora"];

/// A named palette preset. `[theme.colors]` / `[theme.hues]` overrides still
/// apply on top, so every preset stays fully customizable. `None` for unknown
/// names. Legacy presets come back with empty extension tokens — callers go
/// through `Config::palette()`, which runs [`extend_palette`]; call it
/// yourself if you use this directly.
pub fn preset(name: &str) -> Option<Palette> {
    Some(match name {
        // The prism default — the max-TUI redesign palette.
        "prism" | "" => Palette::default(),
        // The storm-blue former default.
        "storm" => pal([
            BG0, BG1, PANEL, PANEL2, RAISE, FRAME, FOCUS, TEXT, DIM, FAINT, GHOST, TEAL,
        ]),
        // A paper-bright light mode with an ink text ramp and deep-teal accent.
        "light" => {
            let mut p = pal([
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
            ]);
            // Hand-tuned darker hues — the prism set is illegible on paper.
            p.hues = Hues {
                teal: "0;122;109".into(),
                magenta: "176;61;118".into(),
                purple: "109;79;194".into(),
                green: "43;138;78".into(),
                amber: "154;106;0".into(),
                red: "194;59;59".into(),
                blue: "47;109;184".into(),
                orange: "194;94;31".into(),
            };
            p
        }
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

/// The semantic-hue slot for an agent/tool name. Known names get their
/// signature hue; unknown ones get a stable hash-picked hue so a custom agent
/// always looks the same.
pub fn agent_hue_slot(name: &str) -> Hue {
    match name.to_ascii_lowercase().as_str() {
        "claude" => Hue::Magenta,
        "codex" => Hue::Green,
        "aider" => Hue::Amber,
        "gemini" => Hue::Purple,
        "shell" | "__shell__" => Hue::Teal,
        "lazygit" => Hue::Green,
        "yazi" => Hue::Purple,
        "editor" => Hue::Blue,
        "diff" => Hue::Amber,
        other => {
            const HUES: [Hue; 6] = [
                Hue::Magenta,
                Hue::Green,
                Hue::Amber,
                Hue::Purple,
                Hue::Blue,
                Hue::Orange,
            ];
            HUES[(fnv1a(other) % HUES.len() as u64) as usize]
        }
    }
}

/// Identity hue for an agent/tool name as a static legacy fragment — used by
/// CLI output, which has no live palette. Chrome should prefer
/// [`Palette::agent_hue`].
pub fn agent_hue(name: &str) -> &'static str {
    match agent_hue_slot(name) {
        Hue::Magenta => CORAL,
        Hue::Green => GREEN,
        Hue::Amber => AMBER,
        Hue::Purple => PURPLE,
        Hue::Teal => TEAL,
        Hue::Blue => BLUE,
        Hue::Red => RED,
        Hue::Orange => AMBER,
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

    fn lum(s: &str) -> u32 {
        s.split(';').map(|n| n.parse::<u32>().unwrap()).sum::<u32>()
    }

    #[test]
    fn presets_resolve_and_cycle_names_are_complete() {
        for name in PRESETS {
            let mut p = preset(name).expect(name);
            extend_palette(&mut p);
            // Every fragment parses as R;G;B.
            for frag in [&p.bg0, &p.text, &p.accent, &p.focus] {
                assert_eq!(frag.split(';').count(), 3, "{name}: {frag}");
            }
        }
        assert_eq!(preset(""), Some(Palette::default()));
        assert_eq!(preset("prism"), Some(Palette::default()));
        assert!(preset("nope").is_none());
        // Light mode really is light (bg brighter than text).
        let l = preset("light").unwrap();
        assert!(lum(&l.bg0) > lum(&l.text));
    }

    #[test]
    fn prism_default_is_fully_specified() {
        let p = Palette::default();
        for frag in [
            &p.ghost2,
            &p.ghost3,
            &p.shadow_bg,
            &p.shadow_fg,
            &p.chip_fg,
            &p.hues.teal,
            &p.hues.magenta,
            &p.hues.purple,
            &p.hues.green,
            &p.hues.amber,
            &p.hues.red,
            &p.hues.blue,
            &p.hues.orange,
        ] {
            assert_eq!(frag.split(';').count(), 3, "{frag}");
        }
        for h in &p.heat {
            assert_eq!(h.split(';').count(), 3);
        }
        // The default accent and focus are the prism teal.
        assert_eq!(p.accent, HUE_TEAL);
        assert_eq!(p.focus, HUE_TEAL);
        // Extension is a no-op on a fully specified palette.
        let mut q = p.clone();
        extend_palette(&mut q);
        assert_eq!(p, q);
    }

    #[test]
    fn extend_derives_every_legacy_preset() {
        for name in &["storm", "light", "abyss", "ember", "aurora"] {
            let mut p = preset(name).unwrap();
            extend_palette(&mut p);
            // fg ramp keeps descending contrast order relative to bg0.
            let bg = lum(&p.bg0);
            let dist = |s: &str| lum(s).abs_diff(bg);
            assert!(
                dist(&p.ghost) >= dist(&p.ghost2) && dist(&p.ghost2) >= dist(&p.ghost3),
                "{name}: ghost ramp must fade toward bg0"
            );
            // Shadow is darker than bg0 (an absolute floor, both modes).
            assert!(lum(&p.shadow_bg) < lum(&p.bg0).max(1), "{name}: shadow");
            assert_eq!(p.chip_fg, p.bg0, "{name}: chip fg");
            // Hues and heat fully populated.
            for h in Hue::ALL {
                assert_eq!(p.hue(h).split(';').count(), 3, "{name}: {h:?}");
            }
            for h in &p.heat {
                assert_eq!(h.split(';').count(), 3, "{name}: heat");
            }
            // Heat ramps monotonically away from the panel surface.
            let panel = lum(&p.panel);
            let dists: Vec<u32> = p.heat.iter().map(|h| lum(h).abs_diff(panel)).collect();
            for w in dists.windows(2) {
                assert!(w[0] <= w[1], "{name}: heat ramp {dists:?}");
            }
        }
    }

    /// Contract: `text`, `dim`, and `faint` are the tiers the chrome may use for
    /// readable text. Every one of them must clear a comfortable contrast margin
    /// against every background surface it can be drawn on — this is the
    /// machine-checkable "is everything readable?" guard. (`ghost`/`ghost2`/
    /// `ghost3` are the structural floor — borders, rules, fills, glyph
    /// scaffolding — and are intentionally exempt; chrome must not render text
    /// the user needs to read below the `faint` tier.)
    #[test]
    fn readable_text_tiers_clear_contrast_on_every_surface() {
        // lum() sums R+G+B (0..=765); 250 is a generous floor that the dim
        // grey-on-grey metadata (ghost2/ghost3 ≈ 120–185 distance) failed.
        const MIN_CONTRAST: u32 = 250;
        let p = Palette::default();
        let surfaces = [
            ("bg0", &p.bg0),
            ("bg1", &p.bg1),
            ("panel", &p.panel),
            ("panel2", &p.panel2),
        ];
        let text_tiers = [("text", &p.text), ("dim", &p.dim), ("faint", &p.faint)];
        for (sname, surf) in surfaces {
            for (fname, fg) in text_tiers {
                let contrast = lum(fg).abs_diff(lum(surf));
                assert!(
                    contrast >= MIN_CONTRAST,
                    "text tier `{fname}` on `{sname}` is too low-contrast ({contrast} < {MIN_CONTRAST})"
                );
            }
        }
    }

    /// The default foreground ramp must descend monotonically in contrast
    /// (text brightest → ghost3 dimmest). The legacy-preset version of this
    /// (`extend_derives_every_legacy_preset`) only covers ghost..ghost3 after
    /// derivation; prism sets every step explicitly, so guard the full ramp.
    #[test]
    fn default_fg_ramp_descends_in_contrast() {
        let p = Palette::default();
        let bg = lum(&p.bg0);
        let dist = |s: &str| lum(s).abs_diff(bg);
        let ramp = [
            ("text", &p.text),
            ("dim", &p.dim),
            ("faint", &p.faint),
            ("ghost", &p.ghost),
            ("ghost2", &p.ghost2),
            ("ghost3", &p.ghost3),
        ];
        for pair in ramp.windows(2) {
            let (hi_name, hi) = pair[0];
            let (lo_name, lo) = pair[1];
            assert!(
                dist(hi) > dist(lo),
                "fg ramp must descend: `{hi_name}` ({}) should out-contrast `{lo_name}` ({})",
                dist(hi),
                dist(lo)
            );
        }
    }

    #[test]
    fn light_preset_hues_are_paper_legible() {
        let mut p = preset("light").unwrap();
        extend_palette(&mut p);
        // Every hue darker than the light background by a wide margin.
        for h in Hue::ALL {
            assert!(
                lum(p.hue(h)) + 250 < lum(&p.bg0),
                "{h:?} too bright for light mode"
            );
        }
    }

    #[test]
    fn selection_tints() {
        let p = Palette::default();
        // sel_accent is accent@16% over bg1 — between the two, nearer bg1.
        assert_eq!(p.sel_accent(), blend_over(&p.accent, &p.bg1, 0.16));
        assert_eq!(p.sel(Hue::Red, 1.0), p.hues.red);
        assert_eq!(p.sel(Hue::Red, 0.0), p.bg1);
    }

    #[test]
    fn hue_lookup_is_exhaustive_and_heat_clamps() {
        let p = Palette::default();
        assert_eq!(p.hue(Hue::Teal), HUE_TEAL);
        assert_eq!(p.hue(Hue::Magenta), HUE_MAGENTA);
        assert_eq!(p.hue(Hue::Purple), HUE_PURPLE);
        assert_eq!(p.hue(Hue::Green), HUE_GREEN);
        assert_eq!(p.hue(Hue::Amber), HUE_AMBER);
        assert_eq!(p.hue(Hue::Red), HUE_RED);
        assert_eq!(p.hue(Hue::Blue), HUE_BLUE);
        assert_eq!(p.hue(Hue::Orange), HUE_ORANGE);
        assert_eq!(p.heat(0), P_HEAT[0]);
        assert_eq!(p.heat(4), P_HEAT[4]);
        assert_eq!(p.heat(99), P_HEAT[4]);
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
        assert_eq!(agent_hue("shell"), TEAL);
        assert_eq!(agent_hue("__shell__"), TEAL);
        // Unknown names: stable across calls and inside the legacy set.
        let h = agent_hue("my-custom-agent");
        assert_eq!(h, agent_hue("my-custom-agent"));
        assert!([CORAL, GREEN, AMBER, PURPLE, BLUE, MAGENTA, TEAL].contains(&h));
    }

    #[test]
    fn palette_agent_hue_follows_the_live_hues() {
        let p = Palette::default();
        assert_eq!(p.agent_hue("claude"), HUE_MAGENTA);
        assert_eq!(p.agent_hue("shell"), HUE_TEAL);
        let mut l = preset("light").unwrap();
        extend_palette(&mut l);
        assert_eq!(l.agent_hue("claude"), "176;61;118");
        // Unknown names stay stable and inside the identity set.
        let slot = agent_hue_slot("my-custom-agent");
        assert_eq!(slot, agent_hue_slot("my-custom-agent"));
        assert!(
            [
                Hue::Magenta,
                Hue::Green,
                Hue::Amber,
                Hue::Purple,
                Hue::Blue,
                Hue::Orange,
            ]
            .contains(&slot)
        );
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
            ("editor", BLUE, "✎"),
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
