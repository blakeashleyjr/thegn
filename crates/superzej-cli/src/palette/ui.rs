//! Bridge between the superzej storm-blue palette (`crate::theme`, whose colors
//! are "R;G;B" fragments) and iocraft's `Color`. The palette lives in the main
//! binary crate, so unlike the WASM plugins it imports `crate::theme` directly
//! rather than carrying a synced copy.

use crate::theme;
use iocraft::prelude::Color;

/// Convert a theme "R;G;B" triple into an iocraft truecolor.
pub fn color(rgb: &str) -> Color {
    let mut it = rgb.split(';').map(|n| n.parse::<u8>().unwrap_or(0));
    Color::Rgb {
        r: it.next().unwrap_or(0),
        g: it.next().unwrap_or(0),
        b: it.next().unwrap_or(0),
    }
}

// Convenience accessors for the most-used palette roles, as iocraft Colors.
pub fn bg0() -> Color {
    color(theme::BG0)
}
pub fn bg1() -> Color {
    color(theme::BG1)
}
pub fn border() -> Color {
    color(theme::BORDER)
}
pub fn text() -> Color {
    color(theme::TEXT)
}
pub fn dim() -> Color {
    color(theme::DIM)
}
pub fn faint() -> Color {
    color(theme::FAINT)
}

/// A subtle selection-bar fill: the accent hue blended ~16% onto the base, the
/// same tint the plugins use for pills/selection.
pub fn accent_tint(accent_rgb: &str) -> Color {
    color(&theme::blend(accent_rgb, 0.16))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_parses_rgb_triple() {
        assert_eq!(
            color("118;238;222"),
            Color::Rgb {
                r: 118,
                g: 238,
                b: 222
            }
        );
    }

    #[test]
    fn color_is_lenient_on_garbage() {
        assert_eq!(color(""), Color::Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(color("1;2"), Color::Rgb { r: 1, g: 2, b: 0 });
    }

    #[test]
    fn role_helpers_are_truecolor() {
        // Each named role maps to its theme triple.
        assert_eq!(text(), color(theme::TEXT));
        assert_eq!(bg0(), color(theme::BG0));
        assert_eq!(bg1(), color(theme::BG1));
        assert_eq!(border(), color(theme::BORDER));
        assert_eq!(dim(), color(theme::DIM));
        assert_eq!(faint(), color(theme::FAINT));
    }

    #[test]
    fn accent_tint_is_darker_than_pure_accent() {
        // The tint blends toward BG0, so it never equals the full accent.
        assert_ne!(accent_tint(theme::TEAL), color(theme::TEAL));
    }
}
