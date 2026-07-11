//! Pane frame rendering: every center pane is a rounded-corner "card" on the
//! darker center background — chrome zones are tinted, terminals are windows.
//! The pane's title (program · worktree) is embedded in the top border; the
//! focused pane's ring renders in the configurable focus color (`[theme]
//! focus_border`, light blue by default). Frames live in the 1-cell ring
//! [`crate::center::CenterTree::layout_framed`] reserves, so the layout never
//! shifts when focus moves.

use termwiz::cell::AttributeChange;
use termwiz::color::ColorAttribute;
use termwiz::surface::{Change, Position, Surface};
use unicode_width::UnicodeWidthStr;

use crate::center::PaneId;
use crate::compositor::Rect;

/// Draw a box ring. Corner/edge glyphs come from the active terminal glyph set
/// ([`crate::caps::active_glyphs`]): rounded Unicode (`╭╮╰╯─│`) on capable
/// terminals, ASCII (`+ - |`) when degraded.
fn draw_box_chars(surface: &mut Surface, rect: Rect, fg: ColorAttribute, bg: ColorAttribute) {
    if rect.cols < 2 || rect.rows < 2 {
        return;
    }
    let g = crate::caps::active_glyphs();
    let right = rect.x + rect.cols - 1;
    let bottom = rect.y + rect.rows - 1;
    let horiz = g.box_h.repeat(rect.cols.saturating_sub(2));

    let mut put = |x: usize, y: usize, text: &str| {
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(x),
            y: Position::Absolute(y),
        });
        surface.add_change(Change::Attribute(AttributeChange::Foreground(fg)));
        surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
        surface.add_change(Change::Text(text.to_string()));
    };

    put(rect.x, rect.y, &format!("{}{horiz}{}", g.box_tl, g.box_tr));
    put(rect.x, bottom, &format!("{}{horiz}{}", g.box_bl, g.box_br));
    for y in (rect.y + 1)..bottom {
        put(rect.x, y, g.box_v);
        put(right, y, g.box_v);
    }
}

/// Colors for one card: ring, embedded title, and the bg behind both.
pub struct CardStyle {
    pub border: ColorAttribute,
    pub title: ColorAttribute,
    pub bg: ColorAttribute,
}

/// A rounded-corner card with `title` embedded in the top border:
/// `╭─ zsh · feat ──────╮`. The title is skipped (ring only) when it is empty
/// or the card is too narrow to keep at least a few dashes around it.
pub fn draw_card(surface: &mut Surface, rect: Rect, title: &str, style: &CardStyle) {
    draw_box_chars(surface, rect, style.border, style.bg);
    if title.is_empty() || rect.cols < 10 || rect.rows < 2 {
        return;
    }
    // `╭─ title ─…`: corner + 1 dash + padded title, and ≥ 3 trailing cells.
    let avail = rect.cols - 6;
    if avail < 4 {
        return;
    }
    let shown: String = if UnicodeWidthStr::width(title) > avail {
        let mut t: String = crate::seg::take_cols(title, avail - 1).to_string();
        t.push('…');
        t
    } else {
        title.to_string()
    };
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(rect.x + 2),
        y: Position::Absolute(rect.y),
    });
    surface.add_change(Change::Attribute(AttributeChange::Foreground(style.title)));
    surface.add_change(Change::Attribute(AttributeChange::Background(style.bg)));
    surface.add_change(Change::Text(format!(" {shown} ")));
}

/// Colors for the pane-frame pass.
pub struct FrameStyle {
    pub border: ColorAttribute,
    pub focus: ColorAttribute,
    pub bg: ColorAttribute,
    pub title: ColorAttribute,
    pub title_focused: ColorAttribute,
}

/// Paint every pane's card: unfocused panes in `border`, then the focused pane
/// last in `focus` (so it always reads on top). `focused` is only honored when
/// the center zone owns focus — otherwise every ring is dim, which is how you
/// see that the sidebar/panel has the keyboard. `title_of` supplies the text
/// embedded in each card's top border (empty → plain ring).
pub fn draw_pane_frames(
    surface: &mut Surface,
    frames: &[(PaneId, Rect, Rect)],
    focused: Option<PaneId>,
    style: &FrameStyle,
    title_of: &dyn Fn(PaneId) -> String,
) {
    for (id, frame, _) in frames {
        if Some(*id) != focused {
            draw_card(
                surface,
                *frame,
                &title_of(*id),
                &CardStyle {
                    border: style.border,
                    title: style.title,
                    bg: style.bg,
                },
            );
        }
    }
    if let Some(f) = focused
        && let Some((_, frame, _)) = frames.iter().find(|(id, _, _)| *id == f)
    {
        draw_card(
            surface,
            *frame,
            &title_of(f),
            &CardStyle {
                border: style.focus,
                title: style.title_focused,
                bg: style.bg,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::center::CenterTree;

    fn cell_text(s: &mut Surface, x: usize, y: usize) -> String {
        s.screen_cells()[y][x].str().to_string()
    }

    fn row_text(s: &mut Surface, y: usize) -> String {
        s.screen_cells()[y].iter().map(|c| c.str()).collect()
    }

    fn style() -> FrameStyle {
        FrameStyle {
            border: ColorAttribute::Default,
            focus: ColorAttribute::Default,
            bg: ColorAttribute::Default,
            title: ColorAttribute::Default,
            title_focused: ColorAttribute::Default,
        }
    }

    #[test]
    fn card_degrades_to_ascii_box() {
        use thegn_core::termcaps::UnicodeLevel;
        crate::caps::test_override::with_unicode(UnicodeLevel::Ascii, || {
            let mut s = Surface::new(20, 4);
            let r = Rect {
                x: 0,
                y: 0,
                cols: 20,
                rows: 4,
            };
            draw_card(
                &mut s,
                r,
                "zsh",
                &CardStyle {
                    border: ColorAttribute::Default,
                    title: ColorAttribute::Default,
                    bg: ColorAttribute::Default,
                },
            );
            assert_eq!(cell_text(&mut s, 0, 0), "+");
            assert_eq!(cell_text(&mut s, 19, 0), "+");
            assert_eq!(cell_text(&mut s, 0, 3), "+");
            assert_eq!(cell_text(&mut s, 19, 3), "+");
            assert_eq!(cell_text(&mut s, 0, 2), "|");
            assert_eq!(cell_text(&mut s, 19, 2), "|");
            assert_eq!(cell_text(&mut s, 1, 0), "-"); // dash before the title
            assert_eq!(cell_text(&mut s, 17, 0), "-"); // trailing dash
            // No Unicode box glyph anywhere on the top row.
            let top = row_text(&mut s, 0);
            assert!(
                !top.contains('╭') && !top.contains('─'),
                "ascii only: {top:?}"
            );
        });
    }

    #[test]
    fn card_has_rounded_corners_and_embedded_title() {
        let mut s = Surface::new(20, 4);
        let r = Rect {
            x: 0,
            y: 0,
            cols: 20,
            rows: 4,
        };
        draw_card(
            &mut s,
            r,
            "zsh · feat",
            &CardStyle {
                border: ColorAttribute::Default,
                title: ColorAttribute::Default,
                bg: ColorAttribute::Default,
            },
        );
        assert_eq!(cell_text(&mut s, 0, 0), "╭");
        assert_eq!(cell_text(&mut s, 19, 0), "╮");
        assert_eq!(cell_text(&mut s, 0, 3), "╰");
        assert_eq!(cell_text(&mut s, 19, 3), "╯");
        assert_eq!(row_text(&mut s, 0), "╭─ zsh · feat ─────╮");
        // Edges + interior.
        assert_eq!(cell_text(&mut s, 0, 2), "│");
        assert_eq!(cell_text(&mut s, 19, 2), "│");
        assert_eq!(cell_text(&mut s, 5, 2), " ");
    }

    #[test]
    fn card_truncates_long_titles_with_ellipsis() {
        let mut s = Surface::new(14, 3);
        let r = Rect {
            x: 0,
            y: 0,
            cols: 14,
            rows: 3,
        };
        draw_card(
            &mut s,
            r,
            "averylongprogramname",
            &CardStyle {
                border: ColorAttribute::Default,
                title: ColorAttribute::Default,
                bg: ColorAttribute::Default,
            },
        );
        // avail = 14 - 6 = 8 → 7 chars + ellipsis.
        assert_eq!(row_text(&mut s, 0), "╭─ averylo… ─╮");
    }

    #[test]
    fn card_skips_title_when_too_narrow_or_empty() {
        for title in ["zsh", ""] {
            let mut s = Surface::new(8, 3);
            let r = Rect {
                x: 0,
                y: 0,
                cols: 8,
                rows: 3,
            };
            draw_card(
                &mut s,
                r,
                title,
                &CardStyle {
                    border: ColorAttribute::Default,
                    title: ColorAttribute::Default,
                    bg: ColorAttribute::Default,
                },
            );
            assert_eq!(row_text(&mut s, 0), "╭──────╮", "title {title:?}");
        }
    }

    #[test]
    fn degenerate_rects_draw_nothing() {
        let mut s = Surface::new(4, 4);
        for (cols, rows) in [(0, 0), (1, 3), (3, 1)] {
            let r = Rect {
                x: 0,
                y: 0,
                cols,
                rows,
            };
            draw_card(
                &mut s,
                r,
                "t",
                &CardStyle {
                    border: ColorAttribute::Default,
                    title: ColorAttribute::Default,
                    bg: ColorAttribute::Default,
                },
            );
        }
        assert_eq!(cell_text(&mut s, 0, 0), " ");
    }

    #[test]
    fn framed_layout_reserves_the_ring_and_frames_render() {
        let tree = CenterTree::single(7);
        let area = Rect {
            x: 2,
            y: 1,
            cols: 20,
            rows: 10,
        };
        let frames = tree.layout_framed(area);
        assert_eq!(frames.len(), 1);
        let (id, frame, content) = frames[0];
        assert_eq!(id, 7);
        assert_eq!(frame, area);
        // Frame ring (1) each side; default pane padding is 0 (flush).
        assert_eq!(
            content,
            Rect {
                x: 3,
                y: 2,
                cols: 18,
                rows: 8
            }
        );

        let mut s = Surface::new(30, 12);
        draw_pane_frames(&mut s, &frames, Some(7), &style(), &|_| String::new());
        assert_eq!(cell_text(&mut s, 2, 1), "╭");
        assert_eq!(cell_text(&mut s, 21, 10), "╯");
    }

    #[test]
    fn focused_pane_card_carries_the_title() {
        let tree = CenterTree::single(3);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 24,
            rows: 6,
        };
        let frames = tree.layout_framed(area);
        let mut s = Surface::new(24, 6);
        draw_pane_frames(&mut s, &frames, Some(3), &style(), &|id| {
            format!("pane-{id}")
        });
        assert!(row_text(&mut s, 0).contains(" pane-3 "));
    }
}
