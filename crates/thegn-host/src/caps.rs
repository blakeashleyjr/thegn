//! Render-time terminal-capability holder.
//!
//! `thegn_core::termcaps` does the pure detection; this is the host-side
//! mutable cell the render path reads. It is installed once at startup and
//! refreshed on config reload / after the async terminal probe
//! (`run::resolve_termcaps`). It follows the sanctioned pattern the codebase
//! already uses for cross-cutting render state — the undercurl `AtomicBool` in
//! [`crate::seg`] and the chrome `PALETTE` `RwLock`: written by the loop,
//! read lock-free during render.
//!
//! Hot-path fields (color depth, glyph level) are plain atomics so the wire
//! renderer and chrome read them with a branchless load and no allocation. The
//! whole [`TermCaps`] is also kept behind an `RwLock` for the cold readers
//! (`thegn doctor`).

use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, Ordering};

use thegn_core::config::AgentGlyphs;
use thegn_core::termcaps::{ColorDepth, GlyphSet, TermCaps, UnicodeLevel, glyphs};
use thegn_core::theme::AgentGlyphStyle;

/// The glyph token type, re-exported so draw sites name it through the
/// chokepoint it resolves against (`crate::caps::glyph(Glyph::…)`).
pub use thegn_core::termcaps::Glyph;

static CAPS: RwLock<TermCaps> = RwLock::new(TermCaps::FULL);
static COLOR_DEPTH: AtomicU8 = AtomicU8::new(0);
static UNICODE_LEVEL: AtomicU8 = AtomicU8::new(0);
// The `[theme] agent_glyphs` preference (0 = Letter, the shipped default; the
// pre-install value is therefore the safe letter style). Resolved against the
// live glyph level at read time by [`agent_glyph_style`].
static AGENT_GLYPHS: AtomicU8 = AtomicU8::new(0);

fn color_to_u8(d: ColorDepth) -> u8 {
    match d {
        ColorDepth::Truecolor => 0,
        ColorDepth::Ansi256 => 1,
        ColorDepth::Ansi16 => 2,
        ColorDepth::None => 3,
    }
}

fn u8_to_color(v: u8) -> ColorDepth {
    match v {
        1 => ColorDepth::Ansi256,
        2 => ColorDepth::Ansi16,
        3 => ColorDepth::None,
        _ => ColorDepth::Truecolor,
    }
}

fn level_to_u8(l: UnicodeLevel) -> u8 {
    match l {
        UnicodeLevel::Full => 0,
        UnicodeLevel::Basic => 1,
        UnicodeLevel::Ascii => 2,
    }
}

fn u8_to_level(v: u8) -> UnicodeLevel {
    match v {
        1 => UnicodeLevel::Basic,
        2 => UnicodeLevel::Ascii,
        _ => UnicodeLevel::Full,
    }
}

fn agent_glyphs_to_u8(g: AgentGlyphs) -> u8 {
    match g {
        AgentGlyphs::Letter => 0,
        AgentGlyphs::Symbol => 1,
        AgentGlyphs::Auto => 2,
    }
}

fn u8_to_agent_glyphs(v: u8) -> AgentGlyphs {
    match v {
        1 => AgentGlyphs::Symbol,
        2 => AgentGlyphs::Auto,
        _ => AgentGlyphs::Letter,
    }
}

/// Install the resolved capabilities. Called at startup and on every config
/// reload / probe upgrade.
pub fn install(caps: TermCaps) {
    COLOR_DEPTH.store(color_to_u8(caps.color), Ordering::Relaxed);
    UNICODE_LEVEL.store(level_to_u8(caps.unicode), Ordering::Relaxed);
    if let Ok(mut w) = CAPS.write() {
        *w = caps;
    }
}

/// The outer terminal's color depth (hot path — the wire renderer).
pub fn color_depth() -> ColorDepth {
    #[cfg(test)]
    if let Some(d) = test_override::color() {
        return d;
    }
    u8_to_color(COLOR_DEPTH.load(Ordering::Relaxed))
}

/// The outer terminal's glyph level (hot path — chrome rendering).
pub fn unicode_level() -> UnicodeLevel {
    #[cfg(test)]
    if let Some(l) = test_override::unicode() {
        return l;
    }
    u8_to_level(UNICODE_LEVEL.load(Ordering::Relaxed))
}

/// The active glyph table (`&'static`, no allocation) for the current level.
pub fn active_glyphs() -> &'static GlyphSet {
    glyphs(unicode_level())
}

/// Resolve a [`Glyph`] token against the active glyph set — the glyph twin of a
/// color token resolving against the live palette. This is the chokepoint an
/// element builder (or any draw site) uses so it carries `Glyph::DotFilled`
/// rather than a raw `"●"` literal, degrading to the ASCII fallback for free
/// on a `[theme] glyphs = ascii` / non-UTF-8 terminal.
pub fn glyph(g: Glyph) -> &'static str {
    g.resolve(active_glyphs())
}

/// A `(bar, track)` pair that degrades: the precision eighth-block gauge on a
/// UTF-8 terminal, `GlyphSet::bar_fill`/`bar_empty` on an ASCII one (`=`/`-`).
/// Every shared draw site routes its `Cell::Bar` through this — the glyph
/// chokepoint for gauges, so `[theme] glyphs = ascii` / a non-UTF-8 locale can
/// never render mojibake (the same contract [`glyph`] enforces for markers).
///
/// Invariant: `bar.chars().count() + track.chars().count() == w` on every
/// branch — table column sizing reserves exactly `w` cells for a bar cell, and
/// a short bar shifts every column after it.
pub fn bar_track(frac: f32, w: usize) -> (String, String) {
    match unicode_level() {
        // The Full and Basic sets share the Unicode table, so the precision
        // gauge is byte-identical to `viz::bar_track` on both — delegate
        // verbatim rather than re-deriving it.
        UnicodeLevel::Full | UnicodeLevel::Basic => thegn_core::viz::bar_track(frac, w),
        UnicodeLevel::Ascii => {
            let g = active_glyphs();
            let filled = (frac.clamp(0.0, 1.0) * w as f32).round() as usize;
            let filled = filled.min(w);
            (g.bar_fill.repeat(filled), g.bar_empty.repeat(w - filled))
        }
    }
}

/// Install the resolved capabilities together with the config's themed glyph
/// preferences. The single install entry point used by the loop at startup and
/// on config reload — [`install`] handles the color/glyph atomics, the
/// `[theme] agent_glyphs` preference is a cheap extra atomic store read back by
/// [`agent_glyph_style`], and the splash-mascot settings ride along into
/// [`crate::owl`]'s atomics.
pub fn install_themed(cfg: &thegn_core::config::Config, caps: TermCaps) {
    install(caps);
    AGENT_GLYPHS.store(
        agent_glyphs_to_u8(cfg.theme.agent_glyphs),
        Ordering::Relaxed,
    );
    crate::owl::install(&cfg.theme);
}

/// The resolved agent-marker style for the sidebar — the configured preference
/// folded with the live glyph level (see
/// [`thegn_core::theme::resolve_agent_glyph_style`]). Hot path: two atomic
/// loads, no allocation.
pub fn agent_glyph_style() -> AgentGlyphStyle {
    thegn_core::theme::resolve_agent_glyph_style(
        u8_to_agent_glyphs(AGENT_GLYPHS.load(Ordering::Relaxed)),
        unicode_level(),
    )
}

/// Per-thread capability overrides for tests. Each `#[test]` runs on its own
/// thread, so an override here is isolated from concurrently-running tests —
/// unlike the process-wide atomics, which a test must never mutate (it would
/// race other tests that read them). Use [`with_unicode`] / [`with_color`].
#[cfg(test)]
pub mod test_override {
    use super::{ColorDepth, UnicodeLevel};
    use std::cell::Cell;

    thread_local! {
        static UNICODE: Cell<Option<UnicodeLevel>> = const { Cell::new(None) };
        static COLOR: Cell<Option<ColorDepth>> = const { Cell::new(None) };
    }

    pub(super) fn unicode() -> Option<UnicodeLevel> {
        UNICODE.with(|c| c.get())
    }
    pub(super) fn color() -> Option<ColorDepth> {
        COLOR.with(|c| c.get())
    }

    /// Run `f` with the glyph level overridden on this thread.
    pub fn with_unicode<R>(level: UnicodeLevel, f: impl FnOnce() -> R) -> R {
        UNICODE.with(|c| c.set(Some(level)));
        let r = f();
        UNICODE.with(|c| c.set(None));
        r
    }

    /// Run `f` with the color depth overridden on this thread.
    pub fn with_color<R>(depth: ColorDepth, f: impl FnOnce() -> R) -> R {
        COLOR.with(|c| c.set(Some(depth)));
        let r = f();
        COLOR.with(|c| c.set(None));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u8_conversions_round_trip() {
        for d in [
            ColorDepth::Truecolor,
            ColorDepth::Ansi256,
            ColorDepth::Ansi16,
            ColorDepth::None,
        ] {
            assert_eq!(u8_to_color(color_to_u8(d)), d);
        }
        for l in [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii] {
            assert_eq!(u8_to_level(level_to_u8(l)), l);
        }
    }

    #[test]
    fn agent_glyphs_u8_round_trip() {
        for g in [AgentGlyphs::Letter, AgentGlyphs::Symbol, AgentGlyphs::Auto] {
            assert_eq!(u8_to_agent_glyphs(agent_glyphs_to_u8(g)), g);
        }
    }

    #[test]
    fn default_agent_glyph_style_is_letter_even_on_modern_terminal() {
        // The process-wide preference atomic defaults to Letter (0) — so the
        // resolved style stays Letter regardless of the detected glyph level.
        // (We never mutate the global here; only the thread-local level.)
        test_override::with_unicode(UnicodeLevel::Full, || {
            assert_eq!(agent_glyph_style(), AgentGlyphStyle::Letter);
        });
        test_override::with_unicode(UnicodeLevel::Ascii, || {
            assert_eq!(agent_glyph_style(), AgentGlyphStyle::Letter);
        });
    }

    #[test]
    fn glyph_token_resolves_through_the_active_set_and_degrades() {
        // A token resolves to the Unicode glyph by default and to the ASCII
        // fallback under an ASCII override — with no branch at the call site.
        assert_eq!(glyph(Glyph::DotFilled), "\u{25cf}");
        test_override::with_unicode(UnicodeLevel::Ascii, || {
            assert_eq!(glyph(Glyph::DotFilled), "*");
            assert_eq!(glyph(Glyph::BoxV), "|");
        });
        assert_eq!(glyph(Glyph::DotFilled), "\u{25cf}");
    }

    #[test]
    fn thread_local_override_selects_glyphs_without_touching_globals() {
        // Default (no override) is the modern terminal.
        assert_eq!(active_glyphs().box_tl, "╭");
        test_override::with_unicode(UnicodeLevel::Ascii, || {
            assert_eq!(unicode_level(), UnicodeLevel::Ascii);
            assert_eq!(active_glyphs().box_tl, "+");
        });
        // Override is cleared after the scope; globals were never mutated.
        assert_eq!(active_glyphs().box_tl, "╭");
        test_override::with_color(ColorDepth::None, || {
            assert_eq!(color_depth(), ColorDepth::None);
        });
        assert_eq!(color_depth(), ColorDepth::Truecolor);
    }

    #[test]
    fn bar_track_fills_its_full_width_on_every_unicode_level() {
        // The invariant `draw_table` sizes its column on: bar + track == w, on
        // both branches (the Unicode gauge and the ASCII fallback), across the
        // whole fraction range and several widths.
        for level in [UnicodeLevel::Full, UnicodeLevel::Basic, UnicodeLevel::Ascii] {
            test_override::with_unicode(level, || {
                for i in 0..=100 {
                    let frac = i as f32 / 100.0;
                    for w in [1usize, 7, 16, 33] {
                        let (bar, track) = bar_track(frac, w);
                        assert_eq!(
                            bar.chars().count() + track.chars().count(),
                            w,
                            "frac {frac} w {w} level {level:?}"
                        );
                    }
                }
            });
        }
    }

    #[test]
    fn bar_track_degrades_to_the_ascii_bar_glyphs_and_delegates_verbatim() {
        // ASCII: runs of `=`/`-` (the GlyphSet bar cells), never a block
        // glyph — the mojibake the chokepoint exists to prevent.
        test_override::with_unicode(UnicodeLevel::Ascii, || {
            let (bar, track) = bar_track(0.5, 16);
            assert_eq!(bar, "========");
            assert_eq!(track, "--------");
            for c in bar.chars().chain(track.chars()) {
                assert!(!('\u{2500}'..='\u{259f}').contains(&c), "{c:?}");
            }
            // Out-of-range fractions clamp, like the Unicode branch.
            let (bar, track) = bar_track(2.0, 4);
            assert_eq!(bar, "====");
            assert_eq!(track, "");
            let (bar, track) = bar_track(-1.0, 4);
            assert_eq!(bar, "");
            assert_eq!(track, "----");
        });
        // Unicode: byte-identical to `viz::bar_track` — nothing on a UTF-8
        // terminal moves.
        for frac in [0.0f32, 0.37, 0.5, 0.99, 1.0] {
            assert_eq!(bar_track(frac, 16), thegn_core::viz::bar_track(frac, 16));
        }
    }
}
