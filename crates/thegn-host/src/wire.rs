//! The wire renderer: serialize the frame's `Change` list to escape
//! sequences ourselves instead of `terminal().render()`.
//!
//! termwiz 0.23's terminfo renderer only emits Single/Double underline and
//! never emits underline color — `Underline::Curly` cells silently lose
//! their squiggle on the wire. This renderer serializes through
//! `termwiz::escape::csi::Sgr` (which does speak `4:3m` and `58:2::r:g:b`),
//! covering exactly the `Change` vocabulary the frame flush produces:
//! CursorPosition / AllAttributes / Text / ClearScreen / CursorVisibility.
//!
//! `THEGN_RENDERER=termwiz` falls back to the stock renderer.

use std::fmt::Write as _;

use termwiz::cell::{Blink, CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::escape::csi::{Cursor, DecPrivateMode, DecPrivateModeCode, Mode, Sgr};
use termwiz::escape::{CSI, OneBased};
use termwiz::surface::{Change, CursorVisibility, Position};

use thegn_core::termcaps::{ColorDepth, index_256_to_rgb, rgb_to_16, rgb_to_256};

/// Convert a frame `ColorAttribute` to the `ColorSpec` we put on the wire,
/// downsampling to the outer terminal's [`ColorDepth`]. The frame is always
/// composed in truecolor; on a lesser terminal we quantize here — the single
/// site every chrome + pane color flows through. `None` is handled by the
/// caller (it skips color SGRs entirely), but maps to `Default` defensively.
fn color_spec(c: ColorAttribute, depth: ColorDepth) -> termwiz::color::ColorSpec {
    use termwiz::color::ColorSpec;
    if depth == ColorDepth::None {
        return ColorSpec::Default;
    }
    match c {
        ColorAttribute::Default => ColorSpec::Default,
        ColorAttribute::PaletteIndex(i) => match depth {
            // Indices 0..15 render natively even on a 16-color terminal; higher
            // indices are re-quantized through their RGB value down to 16.
            ColorDepth::Ansi16 if i >= 16 => {
                let (r, g, b) = index_256_to_rgb(i);
                ColorSpec::PaletteIndex(rgb_to_16(r, g, b))
            }
            _ => ColorSpec::PaletteIndex(i),
        },
        ColorAttribute::TrueColorWithDefaultFallback(t)
        | ColorAttribute::TrueColorWithPaletteFallback(t, _) => match depth {
            ColorDepth::Truecolor => ColorSpec::TrueColor(t),
            ColorDepth::Ansi256 => {
                let (r, g, b, _) = t.to_srgb_u8();
                ColorSpec::PaletteIndex(rgb_to_256(r, g, b))
            }
            ColorDepth::Ansi16 => {
                let (r, g, b, _) = t.to_srgb_u8();
                ColorSpec::PaletteIndex(rgb_to_16(r, g, b))
            }
            ColorDepth::None => ColorSpec::Default,
        },
    }
}

/// Serializes `Change`s to escape sequences, tracking SGR state so identical
/// consecutive attribute runs emit nothing.
pub struct WireRenderer {
    cur: CellAttributes,
    /// Force the next attribute emission (start of stream / after reset).
    dirty: bool,
    /// The outer terminal's color depth, refreshed once per frame from the
    /// installed [`crate::caps`]. Colors are quantized to it in [`color_spec`].
    depth: ColorDepth,
}

impl Default for WireRenderer {
    fn default() -> Self {
        WireRenderer {
            cur: CellAttributes::default(),
            dirty: true,
            depth: ColorDepth::Truecolor,
        }
    }
}

impl WireRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget tracked terminal state (call when something else may have
    /// written to the terminal, e.g. a full repaint or suspend/resume).
    pub fn invalidate(&mut self) {
        self.cur = CellAttributes::default();
        self.dirty = true;
    }

    /// Set the outer terminal's color depth (from [`crate::caps`]); if it
    /// changed (config reload / async probe upgrade) force a re-emit so
    /// on-screen colors are re-quantized to the new depth.
    pub fn set_depth(&mut self, depth: ColorDepth) {
        if depth != self.depth {
            self.depth = depth;
            self.dirty = true;
        }
    }

    fn emit_attrs(&mut self, out: &mut String, next: &CellAttributes) {
        if !self.dirty && &self.cur == next {
            return;
        }
        let mono = self.depth == ColorDepth::None;
        // Reset-then-set: a handful of bytes per style run, no transition
        // table to get wrong.
        let mut w = |sgr: Sgr| {
            let _ = write!(out, "{}", CSI::Sgr(sgr));
        };
        w(Sgr::Reset);
        if next.intensity() != Intensity::Normal {
            w(Sgr::Intensity(next.intensity()));
        }
        if next.italic() {
            w(Sgr::Italic(true));
        }
        if next.underline() != Underline::None {
            w(Sgr::Underline(next.underline()));
        }
        if !mono && next.underline_color() != ColorAttribute::Default {
            w(Sgr::UnderlineColor(color_spec(
                next.underline_color(),
                self.depth,
            )));
        }
        if next.strikethrough() {
            w(Sgr::StrikeThrough(true));
        }
        if next.reverse() {
            w(Sgr::Inverse(true));
        }
        if next.invisible() {
            w(Sgr::Invisible(true));
        }
        if next.blink() != Blink::None {
            w(Sgr::Blink(next.blink()));
        }
        if !mono && next.foreground() != ColorAttribute::Default {
            w(Sgr::Foreground(color_spec(next.foreground(), self.depth)));
        }
        if !mono && next.background() != ColorAttribute::Default {
            w(Sgr::Background(color_spec(next.background(), self.depth)));
        }
        self.cur = next.clone();
        self.dirty = false;
    }

    fn emit_cursor(&self, out: &mut String, x: usize, y: usize) {
        let _ = write!(
            out,
            "{}",
            CSI::Cursor(Cursor::Position {
                line: OneBased::from_zero_based(y as u32),
                col: OneBased::from_zero_based(x as u32),
            })
        );
    }

    /// Serialize `changes` onto `out`. Positions must be absolute (that is
    /// all the frame flush produces); anything else is skipped.
    pub fn render(&mut self, changes: &[Change], out: &mut String) {
        for change in changes {
            match change {
                Change::CursorPosition {
                    x: Position::Absolute(x),
                    y: Position::Absolute(y),
                } => self.emit_cursor(out, *x, *y),
                Change::CursorPosition { .. } => {
                    tracing::warn!(target: "thegn::frame", "wire: non-absolute cursor change skipped");
                }
                Change::AllAttributes(attrs) => self.emit_attrs(out, attrs),
                Change::Attribute(ac) => {
                    let mut next = self.cur.clone();
                    next.apply_change(ac);
                    self.emit_attrs(out, &next);
                }
                Change::Text(t) => out.push_str(t),
                Change::ClearScreen(color) => {
                    let mut attrs = CellAttributes::default();
                    attrs.set_background(*color);
                    self.dirty = true;
                    self.emit_attrs(out, &attrs);
                    // ED 2 (erase display) + home. Re-assert autowrap OFF
                    // (DECRST 7): a full repaint resets our baseline after the
                    // one class of event that could plausibly have reset the
                    // terminal's modes, so pin ?7l here too — writing the
                    // bottom-right cell must never scroll the alt buffer.
                    out.push_str("\u{1b}[2J\u{1b}[?7l");
                    self.emit_cursor(out, 0, 0);
                }
                Change::CursorVisibility(v) => {
                    let _ = write!(out, "{}", CSI::Mode(vis_mode(*v)));
                }
                other => {
                    tracing::warn!(target: "thegn::frame", ?other, "wire: unhandled change skipped");
                }
            }
        }
    }
}

/// Show/hide cursor as DECSET/DECRST 25.
fn vis_mode(v: CursorVisibility) -> Mode {
    let code = DecPrivateMode::Code(DecPrivateModeCode::ShowCursor);
    match v {
        CursorVisibility::Visible => Mode::SetDecPrivateMode(code),
        CursorVisibility::Hidden => Mode::ResetDecPrivateMode(code),
    }
}

/// Whether the outer terminal is known to render curly underlines, from
/// `$TERM` / `$TERM_PROGRAM` / `$VTE_VERSION`. Pure for tests. The detection now
/// lives in `thegn_core::termcaps` (folded into `TermCaps`); re-exported here
/// so existing callers/tests keep working.
pub use thegn_core::termcaps::undercurl_supported_env;

/// Read the environment-sniffed undercurl capability.
pub fn detect_undercurl() -> bool {
    undercurl_supported_env(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("VTE_VERSION").ok().as_deref(),
    )
}

/// Whether the stock termwiz renderer was requested (`THEGN_RENDERER=termwiz`).
pub fn use_termwiz_renderer() -> bool {
    std::env::var("THEGN_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("termwiz"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use termwiz::color::SrgbaTuple;
    use termwiz::escape::Action;
    use termwiz::escape::parser::Parser;

    fn parse(s: &str) -> Vec<Action> {
        let mut p = Parser::new();
        let mut acts = Vec::new();
        p.parse(s.as_bytes(), |a| acts.push(a));
        acts
    }

    fn sgrs(acts: &[Action]) -> Vec<Sgr> {
        acts.iter()
            .filter_map(|a| match a {
                Action::CSI(CSI::Sgr(s)) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    fn attrs_with<R>(f: impl FnOnce(&mut CellAttributes) -> R) -> CellAttributes {
        let mut a = CellAttributes::default();
        let _ = f(&mut a);
        a
    }

    #[test]
    fn curly_underline_and_color_round_trip() {
        let mut r = WireRenderer::new();
        let mut out = String::new();
        let attrs = attrs_with(|a| {
            a.set_underline(Underline::Curly);
            a.set_underline_color(ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                1.0, 0.0, 0.0, 1.0,
            )));
        });
        r.render(
            &[
                Change::AllAttributes(attrs),
                Change::Text("squiggle".into()),
            ],
            &mut out,
        );
        assert!(out.contains("4:3m"), "curly underline on the wire: {out:?}");
        assert!(
            out.contains("58:2:"),
            "underline color on the wire: {out:?}"
        );
        let back = sgrs(&parse(&out));
        assert!(back.contains(&Sgr::Underline(Underline::Curly)), "{back:?}");
        assert!(
            back.iter().any(|s| matches!(s, Sgr::UnderlineColor(_))),
            "{back:?}"
        );
        assert!(out.ends_with("squiggle"));
    }

    #[test]
    fn truecolor_strike_italic_bold_round_trip() {
        let mut r = WireRenderer::new();
        let mut out = String::new();
        let attrs = attrs_with(|a| {
            a.set_foreground(ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                0.5, 0.5, 0.5, 1.0,
            )));
            a.set_background(ColorAttribute::PaletteIndex(4));
            a.set_intensity(Intensity::Bold);
            a.set_italic(true);
            a.set_strikethrough(true);
        });
        r.render(&[Change::AllAttributes(attrs)], &mut out);
        let back = sgrs(&parse(&out));
        assert!(back.contains(&Sgr::Intensity(Intensity::Bold)), "{back:?}");
        assert!(back.contains(&Sgr::Italic(true)), "{back:?}");
        assert!(back.contains(&Sgr::StrikeThrough(true)), "{back:?}");
        assert!(
            back.iter().any(|s| matches!(s, Sgr::Foreground(_))),
            "{back:?}"
        );
        assert!(
            back.iter().any(|s| matches!(s, Sgr::Background(_))),
            "{back:?}"
        );
    }

    #[test]
    fn identical_consecutive_attrs_emit_once() {
        let mut r = WireRenderer::new();
        let mut out = String::new();
        let attrs = attrs_with(|a| {
            a.set_italic(true);
        });
        r.render(
            &[
                Change::AllAttributes(attrs.clone()),
                Change::Text("a".into()),
                Change::AllAttributes(attrs),
                Change::Text("b".into()),
            ],
            &mut out,
        );
        let italics = sgrs(&parse(&out))
            .iter()
            .filter(|s| matches!(s, Sgr::Italic(true)))
            .count();
        assert_eq!(italics, 1, "{out:?}");
        assert!(out.contains("ab") || out.ends_with('b'));
    }

    #[test]
    fn cursor_position_is_one_based_cup() {
        let mut r = WireRenderer::new();
        let mut out = String::new();
        r.render(
            &[Change::CursorPosition {
                x: Position::Absolute(3),
                y: Position::Absolute(5),
            }],
            &mut out,
        );
        assert_eq!(out, "\u{1b}[6;4H");
    }

    #[test]
    fn clear_screen_sets_bg_erases_and_homes() {
        let mut r = WireRenderer::new();
        let mut out = String::new();
        r.render(
            &[Change::ClearScreen(
                ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(0.0, 0.0, 0.0, 1.0)),
            )],
            &mut out,
        );
        assert!(out.contains("\u{1b}[2J"), "{out:?}");
        // A full repaint re-asserts autowrap OFF so writing the bottom-right
        // cell can never scroll the alt buffer (the "window jumps up then
        // back down, top bar vanishes" regression).
        assert!(out.contains("\u{1b}[?7l"), "autowrap re-asserted: {out:?}");
        assert!(out.ends_with("\u{1b}[1;1H"), "{out:?}");
        assert!(
            sgrs(&parse(&out))
                .iter()
                .any(|s| matches!(s, Sgr::Background(_))),
            "{out:?}"
        );
    }

    #[test]
    fn cursor_visibility_decset() {
        let mut r = WireRenderer::new();
        let mut out = String::new();
        r.render(
            &[
                Change::CursorVisibility(CursorVisibility::Hidden),
                Change::CursorVisibility(CursorVisibility::Visible),
            ],
            &mut out,
        );
        assert_eq!(out, "\u{1b}[?25l\u{1b}[?25h");
    }

    #[test]
    fn attribute_change_applies_onto_tracked_state() {
        let mut r = WireRenderer::new();
        let mut out = String::new();
        use termwiz::cell::AttributeChange;
        r.render(
            &[
                Change::AllAttributes(attrs_with(|a| {
                    a.set_italic(true);
                })),
                Change::Attribute(AttributeChange::Intensity(Intensity::Bold)),
            ],
            &mut out,
        );
        let back = sgrs(&parse(&out));
        // The incremental change keeps italic and adds bold.
        let last_italic = back.iter().rposition(|s| matches!(s, Sgr::Italic(true)));
        let last_bold = back
            .iter()
            .rposition(|s| matches!(s, Sgr::Intensity(Intensity::Bold)));
        assert!(last_italic.is_some() && last_bold.is_some(), "{back:?}");
    }

    #[test]
    fn undercurl_capability_heuristics() {
        assert!(undercurl_supported_env(Some("xterm-kitty"), None, None));
        assert!(undercurl_supported_env(Some("wezterm"), None, None));
        assert!(undercurl_supported_env(Some("foot"), None, None));
        assert!(undercurl_supported_env(Some("xterm-ghostty"), None, None));
        assert!(undercurl_supported_env(
            Some("xterm-256color"),
            Some("WezTerm"),
            None
        ));
        assert!(undercurl_supported_env(
            Some("xterm-256color"),
            None,
            Some("7800")
        ));
        assert!(!undercurl_supported_env(
            Some("xterm-256color"),
            None,
            Some("5100")
        ));
        assert!(!undercurl_supported_env(Some("xterm-256color"), None, None));
        assert!(!undercurl_supported_env(None, None, None));
        assert!(!undercurl_supported_env(Some("screen"), Some("tmux"), None));
    }

    fn red() -> ColorAttribute {
        ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(1.0, 0.0, 0.0, 1.0))
    }

    #[test]
    fn color_spec_downsamples_per_depth() {
        use termwiz::color::ColorSpec;
        // Truecolor: pass through unchanged.
        assert!(matches!(
            color_spec(red(), ColorDepth::Truecolor),
            ColorSpec::TrueColor(_)
        ));
        // 256: pure red quantizes to the cube corner index 196.
        assert_eq!(
            color_spec(red(), ColorDepth::Ansi256),
            ColorSpec::PaletteIndex(196)
        );
        // 16: pure red -> bright red (9).
        assert_eq!(
            color_spec(red(), ColorDepth::Ansi16),
            ColorSpec::PaletteIndex(9)
        );
        // mono: no color at all.
        assert_eq!(color_spec(red(), ColorDepth::None), ColorSpec::Default);
        // A high palette index is re-quantized to 16 on a 16-color terminal,
        // but passes through on 256.
        assert_eq!(
            color_spec(ColorAttribute::PaletteIndex(231), ColorDepth::Ansi256),
            ColorSpec::PaletteIndex(231)
        );
        assert!(matches!(
            color_spec(ColorAttribute::PaletteIndex(231), ColorDepth::Ansi16),
            ColorSpec::PaletteIndex(i) if i < 16
        ));
    }

    #[test]
    fn mono_depth_emits_no_color_sgrs() {
        let mut r = WireRenderer::new();
        r.set_depth(ColorDepth::None);
        let mut out = String::new();
        let attrs = attrs_with(|a| {
            a.set_foreground(red());
            a.set_background(ColorAttribute::PaletteIndex(4));
            a.set_underline(Underline::Curly);
            a.set_underline_color(red());
            a.set_intensity(Intensity::Bold);
        });
        r.render(
            &[Change::AllAttributes(attrs), Change::Text("x".into())],
            &mut out,
        );
        // No 24-bit (`38;2`/`48;2`), no indexed (`38;5`/`48;5`), no underline
        // color (`58`) — but non-color attributes (bold) still emit.
        assert!(!out.contains("38;"), "no fg color: {out:?}");
        assert!(!out.contains("48;"), "no bg color: {out:?}");
        assert!(
            !out.contains("58:") && !out.contains("58;"),
            "no ul color: {out:?}"
        );
        let back = sgrs(&parse(&out));
        assert!(back.contains(&Sgr::Intensity(Intensity::Bold)), "{back:?}");
        assert!(out.ends_with('x'));
    }

    #[test]
    fn ansi256_depth_emits_indexed_not_truecolor() {
        let mut r = WireRenderer::new();
        r.set_depth(ColorDepth::Ansi256);
        let mut out = String::new();
        let attrs = attrs_with(|a| {
            a.set_foreground(red());
        });
        r.render(&[Change::AllAttributes(attrs)], &mut out);
        let back = sgrs(&parse(&out));
        assert!(
            back.iter().any(|s| matches!(
                s,
                Sgr::Foreground(termwiz::color::ColorSpec::PaletteIndex(_))
            )),
            "fg is indexed under 256: {back:?}"
        );
    }
}
