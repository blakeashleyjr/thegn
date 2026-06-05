//! The superzej design system: one storm-blue palette shared by the host CLI
//! and all four zellij plugins.
//!
//! CANONICAL COPY: src/theme.rs. The plugin crates each carry a byte-identical
//! committed copy at plugin/<name>/src/theme.rs (the Nix plugin builds sandbox
//! each crate's subdir, so a shared path-dependency crate can't work). Edit
//! THIS file, then run `just sync-theme` to refresh the copies; `just lint`
//! fails on drift.
//!
//! Colors are "R;G;B" fragments ready for `\x1b[38;2;{}m` / `\x1b[48;2;{}m`,
//! the form the plugins already build escapes from. Values are the mockup's
//! gamut-mapped sRGB — do not re-derive from oklch.
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

pub const RESET: &str = "\u{1b}[0m";

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

/// Lerp a hue toward BG0: `t` is how much of the hue survives (0.0 = pure
/// BG0, 1.0 = pure hue). `blend(hue, 0.16)` approximates the mockup's
/// 16%-alpha pill/selection tints on the storm-blue base.
pub fn blend(hue: &str, t: f32) -> String {
    let p = |s: &str| -> [f32; 3] {
        let mut it = s.split(';').map(|n| n.parse::<f32>().unwrap_or(0.0));
        [
            it.next().unwrap_or(0.0),
            it.next().unwrap_or(0.0),
            it.next().unwrap_or(0.0),
        ]
    };
    let (h, b) = (p(hue), p(BG0));
    let c = |i: usize| (b[i] + (h[i] - b[i]) * t).round() as u8;
    format!("{};{};{}", c(0), c(1), c(2))
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
}
