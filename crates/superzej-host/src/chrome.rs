//! In-process chrome: the four surfaces (tabbar, sidebar, panel, statusbar)
//! drawn natively into the back-buffer `Surface` around the center pane. No
//! WASM, no IPC, no broadcast — widgets read state directly and draw cells.
//! This replaces the four zellij plugins.

use termwiz::cell::AttributeChange;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::{Change, Position, Surface};

use crate::compositor::{Rect, compose_pane};
use crate::emulator::PaneEmulator;
use superzej_core::theme;

/// Parse a theme `"r;g;b"` triple into a termwiz color.
pub fn theme_color(triple: &str) -> ColorAttribute {
    let mut it = triple
        .split(';')
        .filter_map(|s| s.trim().parse::<u8>().ok());
    match (it.next(), it.next(), it.next()) {
        (Some(r), Some(g), Some(b)) => ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            1.0,
        )),
        _ => ColorAttribute::Default,
    }
}

/// Write `text` at `(x, y)`, clipped to `max_cols`, with the given colors. Does
/// not fill beyond the text — use [`fill`] first for a solid background.
pub fn draw_text(
    surface: &mut Surface,
    x: usize,
    y: usize,
    text: &str,
    fg: ColorAttribute,
    bg: ColorAttribute,
    max_cols: usize,
) {
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    surface.add_change(Change::Attribute(AttributeChange::Foreground(fg)));
    surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
    let clipped: String = text.chars().take(max_cols).collect();
    surface.add_change(Change::Text(clipped));
}

/// Fill `rect` with spaces on `bg` (a solid background block).
pub fn fill(surface: &mut Surface, rect: Rect, bg: ColorAttribute) {
    let row = " ".repeat(rect.cols);
    for r in 0..rect.rows {
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(rect.x),
            y: Position::Absolute(rect.y + r),
        });
        surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
        surface.add_change(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::Default,
        )));
        surface.add_change(Change::Text(row.clone()));
    }
}

/// What the chrome needs to paint a frame. Populated from session state + DB +
/// git by the host; kept renderer-agnostic so it's unit-testable.
#[derive(Debug, Clone, Default)]
pub struct FrameModel {
    pub tabs: Vec<String>,
    pub active_tab: usize,
    pub sidebar: Vec<String>,
    pub sidebar_selected: usize,
    pub panel: Vec<String>,
    pub status: String,
    pub accent: String,
}

impl FrameModel {
    pub fn accent_or_default(&self) -> &str {
        if self.accent.is_empty() {
            theme::TEAL
        } else {
            &self.accent
        }
    }
}

pub fn draw_tabbar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    if rect.rows == 0 {
        return;
    }
    fill(surface, rect, theme_color(theme::BG1));
    let accent = theme_color(model.accent_or_default());
    let dim = theme_color(theme::DIM);
    let mut x = rect.x + 1;
    for (i, name) in model.tabs.iter().enumerate() {
        if x >= rect.x + rect.cols {
            break;
        }
        let label = format!(" {name} ");
        let fg = if i == model.active_tab { accent } else { dim };
        let max = (rect.x + rect.cols).saturating_sub(x);
        draw_text(surface, x, rect.y, &label, fg, theme_color(theme::BG1), max);
        x += label.chars().count();
    }
}

pub fn draw_statusbar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    if rect.rows == 0 {
        return;
    }
    fill(surface, rect, theme_color(theme::BG1));
    draw_text(
        surface,
        rect.x + 1,
        rect.y,
        &model.status,
        theme_color(theme::FAINT),
        theme_color(theme::BG1),
        rect.cols.saturating_sub(1),
    );
}

pub fn draw_sidebar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    fill(surface, rect, theme_color(theme::BG0));
    let accent = theme_color(model.accent_or_default());
    draw_text(
        surface,
        rect.x,
        rect.y,
        " WORKSPACES",
        accent,
        theme_color(theme::BG0),
        rect.cols,
    );
    for (i, item) in model.sidebar.iter().enumerate() {
        let y = rect.y + 1 + i;
        if y >= rect.y + rect.rows {
            break;
        }
        let (fg, bg) = if i == model.sidebar_selected {
            (theme_color(theme::TEXT), theme_color(theme::PANEL2))
        } else {
            (theme_color(theme::DIM), theme_color(theme::BG0))
        };
        if i == model.sidebar_selected {
            fill(
                surface,
                Rect {
                    x: rect.x,
                    y,
                    cols: rect.cols,
                    rows: 1,
                },
                bg,
            );
        }
        draw_text(
            surface,
            rect.x + 1,
            y,
            item,
            fg,
            bg,
            rect.cols.saturating_sub(1),
        );
    }
}

pub fn draw_panel(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    fill(surface, rect, theme_color(theme::PANEL));
    for (i, line) in model.panel.iter().enumerate() {
        let y = rect.y + i;
        if y >= rect.y + rect.rows {
            break;
        }
        draw_text(
            surface,
            rect.x + 1,
            y,
            line,
            theme_color(theme::TEXT),
            theme_color(theme::PANEL),
            rect.cols.saturating_sub(1),
        );
    }
}

/// Draw the surrounding chrome (sidebar/panel/tabbar/statusbar) — the center is
/// filled separately by [`render_tab`].
pub fn draw_chrome(
    surface: &mut Surface,
    chrome: &crate::layout::ChromeLayout,
    model: &FrameModel,
) {
    if let Some(sb) = chrome.sidebar {
        draw_sidebar(surface, sb, model);
    }
    if let Some(pn) = chrome.panel {
        draw_panel(surface, pn, model);
    }
    draw_tabbar(surface, chrome.tabbar, model);
    draw_statusbar(surface, chrome.statusbar, model);
}

/// Compose a multi-pane tab: lay the `center` tree out within `chrome.center`,
/// paint each visible pane (resolved via `lookup`), draw a 1-row accent header
/// above the focused split when there's more than one pane, then the chrome.
pub fn render_tab<'a>(
    surface: &mut Surface,
    chrome: &crate::layout::ChromeLayout,
    center: &crate::center::CenterTree,
    focused: crate::center::PaneId,
    model: &FrameModel,
    lookup: impl Fn(crate::center::PaneId) -> Option<&'a dyn PaneEmulator>,
) {
    let _ = focused; // a non-destructive focus border is a later polish
    for (id, rect) in center.layout(chrome.center) {
        if let Some(emu) = lookup(id) {
            compose_pane(surface, emu, rect);
        }
    }
    draw_chrome(surface, chrome, model);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emulator::Vt100Emulator;
    use crate::layout;

    fn lines(s: &Surface) -> Vec<String> {
        s.screen_chars_to_string()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn tabbar_shows_tab_names() {
        let mut s = Surface::new(80, 1);
        let model = FrameModel {
            tabs: vec!["app/home".into(), "app/feat".into()],
            active_tab: 1,
            ..Default::default()
        };
        draw_tabbar(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 80,
                rows: 1,
            },
            &model,
        );
        let l = lines(&s);
        assert!(l[0].contains("app/home"));
        assert!(l[0].contains("app/feat"));
    }

    #[test]
    fn render_tab_paints_every_visible_pane() {
        use crate::center::{Branch, CenterTree, Dir};
        let cols = 160usize;
        let rows = 40usize;
        let chrome = layout::compute(cols, rows, false, false); // full-width center

        // Two side-by-side panes (ids 1 and 2).
        let center = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(2),
                },
            ],
        };
        let half = (chrome.center.cols / 2) as u16;
        let mut left = Vt100Emulator::new(chrome.center.rows as u16, half, 0);
        left.advance(b"LEFTPANE");
        let mut right = Vt100Emulator::new(chrome.center.rows as u16, half, 0);
        right.advance(b"RIGHTPANE");

        let model = FrameModel {
            tabs: vec!["repo/home".into()],
            ..Default::default()
        };
        let mut s = Surface::new(cols, rows);
        render_tab(&mut s, &chrome, &center, 1, &model, |id| match id {
            1 => Some(&left as &dyn PaneEmulator),
            2 => Some(&right as &dyn PaneEmulator),
            _ => None,
        });
        let text = s.screen_chars_to_string();
        assert!(text.contains("LEFTPANE"), "left pane painted");
        assert!(text.contains("RIGHTPANE"), "right pane painted");
    }

    #[test]
    fn full_frame_places_chrome_and_center_pane() {
        let cols = 160usize;
        let rows = 40usize;
        let chrome = layout::compute(cols, rows, true, true);

        let mut emu = Vt100Emulator::new(chrome.center.rows as u16, chrome.center.cols as u16, 0);
        emu.advance(b"CENTER-CONTENT");

        let model = FrameModel {
            tabs: vec!["repo/home".into()],
            active_tab: 0,
            sidebar: vec!["repo".into(), "  feat".into()],
            panel: vec!["+12 -3".into(), "#42 open".into()],
            status: "Cmd-K  Alt-w new  Alt-o switch".into(),
            ..Default::default()
        };

        let mut s = Surface::new(cols, rows);
        let center = crate::center::CenterTree::Leaf(1);
        render_tab(&mut s, &chrome, &center, 1, &model, |id| {
            (id == 1).then_some(&emu as &dyn PaneEmulator)
        });
        let l = lines(&s);

        // Tabbar (row 0) carries the tab name; statusbar (last row) the hints.
        assert!(l[0].contains("repo/home"), "tabbar: {:?}", l[0]);
        assert!(l[rows - 1].contains("Cmd-K"), "status: {:?}", l[rows - 1]);
        // Sidebar title and the center content both present somewhere.
        let all = l.join("\n");
        assert!(all.contains("WORKSPACES"));
        assert!(all.contains("CENTER-CONTENT"));
        assert!(all.contains("#42 open"));
    }
}
