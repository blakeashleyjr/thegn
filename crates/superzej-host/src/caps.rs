//! Render-time terminal-capability holder.
//!
//! `superzej_core::termcaps` does the pure detection; this is the host-side
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
//! (`superzej doctor`).

use std::sync::RwLock;
use std::sync::atomic::{AtomicU8, Ordering};

use superzej_core::termcaps::{ColorDepth, GlyphSet, TermCaps, UnicodeLevel, glyphs};

static CAPS: RwLock<TermCaps> = RwLock::new(TermCaps::FULL);
static COLOR_DEPTH: AtomicU8 = AtomicU8::new(0);
static UNICODE_LEVEL: AtomicU8 = AtomicU8::new(0);

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

/// Install the resolved capabilities. Called at startup and on every config
/// reload / probe upgrade.
pub fn install(caps: TermCaps) {
    COLOR_DEPTH.store(color_to_u8(caps.color), Ordering::Relaxed);
    UNICODE_LEVEL.store(level_to_u8(caps.unicode), Ordering::Relaxed);
    if let Ok(mut w) = CAPS.write() {
        *w = caps;
    }
}

/// The full resolved capability set (cold path — diagnostics / telemetry).
#[allow(dead_code)]
pub fn get() -> TermCaps {
    CAPS.read().map(|c| *c).unwrap_or(TermCaps::FULL)
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
}
