//! Architecture ratchets for the host crate (see
//! `thegn_core::test_support::ratchet`): platform-conditional code, and the
//! two render chokepoints. Each allowlist under `test/` is shrink-only and
//! explains its rule in its header.

use thegn_core::test_support::ratchet::{file_ratchet, has_platform_cfg};

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// `#[cfg(unix|windows|target_os|…)]` belongs in `src/platform/` — the one
/// seam whose job is per-OS code — so call sites stay platform-free and a new
/// OS is a new file there, not a sweep of the tree.
#[test]
fn platform_cfgs_live_in_platform_modules() {
    file_ratchet(
        MANIFEST,
        "platform-cfg-host-ratchet.txt",
        &["platform/"],
        |_, body| has_platform_cfg(body),
        "Platform-conditional code belongs in src/platform/ (CLAUDE.md: keep the \
         seam thin, call sites platform-free). Move the `#[cfg]` arm behind a \
         `platform::` function.",
    );
}

/// Colors are composed in truecolor and quantized once, at `wire.rs`'s
/// `color_spec`; the theme resolves roles to RGB. A literal anywhere else
/// bypasses both the theme and the degradation ladder.
#[test]
fn color_literals_stay_in_the_chokepoints() {
    let re = regex::Regex::new(r"Color::Rgb|Color::TrueColor|\bRgb\s*\(|\brgb\(").unwrap();
    file_ratchet(
        MANIFEST,
        "color-literal-ratchet.txt",
        // wire.rs quantizes; caps.rs holds the resolved depth; theme* resolve
        // roles; the two ratatui bridges (apps/bridge.rs blits cells,
        // apps/mod.rs converts the Palette to tg_kit::Theme) convert whole
        // palettes, which is exactly a chokepoint's job.
        &[
            "wire.rs",
            "caps.rs",
            "theme",
            "apps/bridge.rs",
            "apps/mod.rs",
        ],
        |_, body| re.is_match(body),
        "Color literals belong to the theme (a `Hue` role resolved by the active \
         palette) and are quantized once in wire.rs. Name a role instead of an RGB.",
    );
}

/// The sprite/nix ssh call sites in thegn-host (`agent.rs`, `agent_ssh.rs`,
/// `autoscale.rs`) get their host-key options from the one chokepoint; no
/// literal here. See `thegn_core::hostkey`.
#[test]
fn host_key_literals_stay_in_the_chokepoint() {
    file_ratchet(
        MANIFEST,
        "hostkey-host-ratchet.txt",
        &[],
        |_, body| thegn_core::hostkey::is_host_key_literal(body),
        "SSH host-key options belong to the one policy chokepoint \
         (`thegn_core::hostkey::host_key_args`): name a connection class and let \
         it build the `-o` args. Do not write a StrictHostKeyChecking / \
         UserKnownHostsFile / HostKeyAlias literal at a call site.",
    );
}

/// Box-drawing / block glyphs come from `caps::active_glyphs()` (Unicode or
/// ASCII, per the detected terminal). A literal bypasses the ASCII fallback.
#[test]
fn glyph_literals_go_through_active_glyphs() {
    file_ratchet(
        MANIFEST,
        "glyph-literal-ratchet.txt",
        &["caps.rs"],
        |_, body| body.chars().any(|c| ('\u{2500}'..='\u{259f}').contains(&c)),
        "Box-drawing and block glyphs must come from `caps::active_glyphs()` so \
         the ASCII fallback (`[theme] glyphs = ascii`, non-UTF-8 locales) still \
         renders. Use the GlyphSet field instead of the literal.",
    );
}
