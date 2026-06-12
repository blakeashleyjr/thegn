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
//! `SUPERZEJ_RENDERER=termwiz` falls back to the stock renderer.

use std::fmt::Write as _;

use termwiz::cell::{Blink, CellAttributes, Intensity, Underline};
use termwiz::color::ColorAttribute;
use termwiz::escape::csi::{Cursor, DecPrivateMode, DecPrivateModeCode, Mode, Sgr};
use termwiz::escape::{CSI, OneBased};
use termwiz::surface::{Change, CursorVisibility, Position};

fn color_spec(c: ColorAttribute) -> termwiz::color::ColorSpec {
    use termwiz::color::ColorSpec;
    match c {
        ColorAttribute::Default => ColorSpec::Default,
        ColorAttribute::PaletteIndex(i) => ColorSpec::PaletteIndex(i),
        ColorAttribute::TrueColorWithDefaultFallback(t)
        | ColorAttribute::TrueColorWithPaletteFallback(t, _) => ColorSpec::TrueColor(t),
    }
}

/// Serializes `Change`s to escape sequences, tracking SGR state so identical
/// consecutive attribute runs emit nothing.
pub struct WireRenderer {
    cur: CellAttributes,
    /// Force the next attribute emission (start of stream / after reset).
    dirty: bool,
}

impl Default for WireRenderer {
    fn default() -> Self {
        WireRenderer {
            cur: CellAttributes::default(),
            dirty: true,
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

    fn emit_attrs(&mut self, out: &mut String, next: &CellAttributes) {
        if !self.dirty && &self.cur == next {
            return;
        }
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
        if next.underline_color() != ColorAttribute::Default {
            w(Sgr::UnderlineColor(color_spec(next.underline_color())));
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
        if next.foreground() != ColorAttribute::Default {
            w(Sgr::Foreground(color_spec(next.foreground())));
        }
        if next.background() != ColorAttribute::Default {
            w(Sgr::Background(color_spec(next.background())));
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
                    tracing::warn!(target: "szhost::frame", "wire: non-absolute cursor change skipped");
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
                    // ED 2 (erase display) + home.
                    out.push_str("\u{1b}[2J");
                    self.emit_cursor(out, 0, 0);
                }
                Change::CursorVisibility(v) => {
                    let _ = write!(out, "{}", CSI::Mode(vis_mode(*v)));
                }
                other => {
                    tracing::warn!(target: "szhost::frame", ?other, "wire: unhandled change skipped");
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
/// `$TERM` / `$TERM_PROGRAM` / `$VTE_VERSION`. Pure for tests.
pub fn undercurl_supported_env(
    term: Option<&str>,
    term_program: Option<&str>,
    vte_version: Option<&str>,
) -> bool {
    let term = term.unwrap_or("").to_ascii_lowercase();
    let prog = term_program.unwrap_or("").to_ascii_lowercase();
    const TERMS: &[&str] = &[
        "kitty",
        "wezterm",
        "foot",
        "ghostty",
        "alacritty",
        "contour",
        "rio",
    ];
    if TERMS.iter().any(|t| term.contains(t)) {
        return true;
    }
    const PROGS: &[&str] = &[
        "wezterm",
        "kitty",
        "ghostty",
        "iterm.app",
        "rio",
        "alacritty",
    ];
    if PROGS.iter().any(|p| prog.contains(p)) {
        return true;
    }
    // VTE-based terminals support undercurl since 0.52 (VTE_VERSION=5200).
    if let Some(v) = vte_version
        && v.parse::<u32>().is_ok_and(|n| n >= 5200)
    {
        return true;
    }
    false
}

/// Read the environment-sniffed undercurl capability.
pub fn detect_undercurl() -> bool {
    undercurl_supported_env(
        std::env::var("TERM").ok().as_deref(),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("VTE_VERSION").ok().as_deref(),
    )
}

/// Whether the stock termwiz renderer was requested (`SUPERZEJ_RENDERER=termwiz`).
pub fn use_termwiz_renderer() -> bool {
    std::env::var("SUPERZEJ_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("termwiz"))
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
}
