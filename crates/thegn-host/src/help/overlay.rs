//! The F1 help overlay: a large centered layer, TOC/search on the left,
//! the rendered page on the right. Modal — it owns every key while open —
//! and pure state: no I/O, no timers; every transition just marks the frame
//! dirty (chrome damage → a Full render), so the render-plan invariants are
//! untouched.

use std::sync::Arc;

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Seg, Tok, seg, sp};
use thegn_core::help::{HelpRegistry, LinkTarget, SearchHit, TocNode};

use super::render::{RenderedPage, render_page};

/// What the loop should do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpOutcome {
    Close,
    /// Open the current page in the panel's Help section (the `o` key).
    OpenInPanel,
    Pending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Toc,
    Content,
}

struct SearchUi {
    query: String,
    hits: Vec<SearchHit>,
    sel: usize,
}

pub struct HelpOverlay {
    reg: Arc<HelpRegistry>,
    page: String,
    scroll: usize,
    link_sel: Option<usize>,
    side: Side,
    toc_rows: Vec<(u8, String, String)>, // (depth, id, title)
    toc_sel: usize,
    toc_scroll: usize,
    search: Option<SearchUi>,
    back: Vec<(String, usize)>,
    fwd: Vec<(String, usize)>,
    /// Geometry from the last render: (content width, visible body rows).
    /// Key handling clamps against it; before the first render the defaults
    /// only make scrolling conservative, never wrong.
    last_dims: (usize, usize),
}

fn flatten_toc(
    nodes: &[TocNode],
    depth: u8,
    reg: &HelpRegistry,
    out: &mut Vec<(u8, String, String)>,
) {
    for n in nodes {
        let title = reg
            .page(&n.id)
            .map(|p| p.meta.title.clone())
            .unwrap_or_else(|| n.id.clone());
        out.push((depth, n.id.clone(), title));
        flatten_toc(&n.children, depth + 1, reg, out);
    }
}

impl HelpOverlay {
    pub fn new(reg: Arc<HelpRegistry>, page: String) -> Self {
        let mut toc_rows = Vec::new();
        flatten_toc(reg.toc(), 0, &reg, &mut toc_rows);
        let toc_sel = toc_rows
            .iter()
            .position(|(_, id, _)| *id == page)
            .unwrap_or(0);
        HelpOverlay {
            reg,
            page,
            scroll: 0,
            link_sel: None,
            side: Side::Content,
            toc_rows,
            toc_sel,
            toc_scroll: 0,
            search: None,
            back: Vec::new(),
            fwd: Vec::new(),
            last_dims: (72, 20),
        }
    }

    pub fn page_id(&self) -> &str {
        &self.page
    }

    fn rendered(&self) -> RenderedPage {
        let blocks = self
            .reg
            .page(&self.page)
            .map(|p| p.blocks.as_slice())
            .unwrap_or(&[]);
        render_page(blocks, self.last_dims.0, self.link_sel)
    }

    fn max_scroll(&self, total_lines: usize) -> usize {
        total_lines.saturating_sub(self.last_dims.1)
    }

    /// Jump to `page`, recording the departure point in the back stack.
    fn goto(&mut self, page: String, scroll: usize) {
        if self.reg.page(&page).is_none() {
            return;
        }
        self.back.push((self.page.clone(), self.scroll));
        self.fwd.clear();
        self.set_page(page, scroll);
    }

    fn set_page(&mut self, page: String, scroll: usize) {
        if let Some(i) = self.toc_rows.iter().position(|(_, id, _)| *id == page) {
            self.toc_sel = i;
        }
        self.page = page;
        self.scroll = scroll;
        self.link_sel = None;
    }

    fn back(&mut self) {
        if let Some((page, scroll)) = self.back.pop() {
            self.fwd.push((self.page.clone(), self.scroll));
            self.set_page(page, scroll);
        }
    }

    fn forward(&mut self) {
        if let Some((page, scroll)) = self.fwd.pop() {
            self.back.push((self.page.clone(), self.scroll));
            self.set_page(page, scroll);
        }
    }

    fn follow_link(&mut self) {
        let Some(idx) = self.link_sel else { return };
        let rendered = self.rendered();
        let Some(link) = rendered.links.get(idx) else {
            return;
        };
        match &link.target {
            LinkTarget::Page(id) => self.goto(id.clone(), 0),
            // External URLs can't open from a TUI portably; put the target on
            // the clipboard instead (best-effort, like copy mode).
            LinkTarget::Url(url) => crate::clipboard::copy(url),
        }
    }

    fn cycle_link(&mut self, delta: isize) {
        let n = self.rendered().links.len();
        if n == 0 {
            return;
        }
        self.link_sel = Some(match self.link_sel {
            None if delta >= 0 => 0,
            None => n - 1,
            Some(i) => (i as isize + delta).rem_euclid(n as isize) as usize,
        });
        // Keep the selected link visible.
        if let Some(idx) = self.link_sel {
            let rendered = self.rendered();
            if let Some(link) = rendered.links.get(idx) {
                let rows = self.last_dims.1;
                if link.line < self.scroll {
                    self.scroll = link.line;
                } else if link.line >= self.scroll + rows {
                    self.scroll = link.line + 1 - rows;
                }
            }
        }
    }

    fn run_search(&mut self) {
        if let Some(s) = self.search.as_mut() {
            s.hits = thegn_core::help::search(
                self.reg.pages(),
                &s.query,
                &crate::fff_backend::fuzzy_rank,
            );
            s.sel = 0;
        }
    }

    /// Open a search hit: jump to its page, scrolled to the matched section.
    fn open_hit(&mut self) {
        let Some(s) = self.search.take() else { return };
        let Some(hit) = s.hits.get(s.sel) else { return };
        let target = hit.page.clone();
        let section = hit.snippet.as_ref().and_then(|sn| sn.section.clone());
        self.goto(target, 0);
        if let Some(section) = section {
            let rendered = self.rendered();
            if let Some((line, _)) = rendered.headings.iter().find(|(_, h)| *h == section) {
                self.scroll = (*line).min(self.max_scroll(rendered.lines.len()));
            }
        }
        self.side = Side::Content;
    }

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> HelpOutcome {
        let ctrl = mods.contains(Modifiers::CTRL);
        // Search owns the keyboard while open.
        if self.search.is_some() {
            match key {
                KeyCode::Escape => self.search = None,
                KeyCode::Char('c') if ctrl => self.search = None,
                KeyCode::Enter => self.open_hit(),
                KeyCode::UpArrow => {
                    if let Some(s) = self.search.as_mut() {
                        s.sel = s.sel.saturating_sub(1);
                    }
                }
                KeyCode::DownArrow => {
                    if let Some(s) = self.search.as_mut() {
                        s.sel = (s.sel + 1).min(s.hits.len().saturating_sub(1));
                    }
                }
                KeyCode::Backspace => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.pop();
                    }
                    self.run_search();
                }
                KeyCode::Char(c) if !ctrl && !mods.contains(Modifiers::ALT) => {
                    if let Some(s) = self.search.as_mut() {
                        s.query.push(*c);
                    }
                    self.run_search();
                }
                _ => {}
            }
            return HelpOutcome::Pending;
        }

        match key {
            KeyCode::Escape | KeyCode::Function(1) => return HelpOutcome::Close,
            KeyCode::Char('q') => return HelpOutcome::Close,
            KeyCode::Char('c') if ctrl => return HelpOutcome::Close,
            KeyCode::Char('/') => {
                self.search = Some(SearchUi {
                    query: String::new(),
                    hits: Vec::new(),
                    sel: 0,
                });
            }
            KeyCode::Char('o') => return HelpOutcome::OpenInPanel,
            KeyCode::Tab => {
                self.side = match self.side {
                    Side::Toc => Side::Content,
                    Side::Content => Side::Toc,
                };
            }
            KeyCode::Char('[') => self.back(),
            KeyCode::Char(']') => self.forward(),
            KeyCode::Backspace => self.back(),
            _ => match self.side {
                Side::Toc => self.toc_key(key),
                Side::Content => self.content_key(key),
            },
        }
        HelpOutcome::Pending
    }

    fn toc_key(&mut self, key: &KeyCode) {
        match key {
            KeyCode::UpArrow | KeyCode::Char('k') => {
                self.toc_sel = self.toc_sel.saturating_sub(1);
                self.open_toc_row();
            }
            KeyCode::DownArrow | KeyCode::Char('j') => {
                self.toc_sel = (self.toc_sel + 1).min(self.toc_rows.len().saturating_sub(1));
                self.open_toc_row();
            }
            KeyCode::Enter | KeyCode::RightArrow | KeyCode::Char('l') => {
                self.side = Side::Content;
            }
            _ => {}
        }
    }

    /// Browsing the TOC previews pages live — no Enter needed. Browsing is
    /// not link-following, so it doesn't touch the back/forward stacks.
    fn open_toc_row(&mut self) {
        if let Some((_, id, _)) = self.toc_rows.get(self.toc_sel)
            && *id != self.page
        {
            let id = id.clone();
            self.set_page(id, 0);
        }
    }

    fn content_key(&mut self, key: &KeyCode) {
        let rendered_len = self.rendered().lines.len();
        let max = self.max_scroll(rendered_len);
        let jump = self.last_dims.1.saturating_sub(2).max(1);
        match key {
            KeyCode::UpArrow | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::DownArrow | KeyCode::Char('j') => self.scroll = (self.scroll + 1).min(max),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(jump),
            KeyCode::PageDown => self.scroll = (self.scroll + jump).min(max),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = max,
            KeyCode::Char('n') => self.cycle_link(1),
            KeyCode::Char('p') => self.cycle_link(-1),
            KeyCode::Enter => self.follow_link(),
            KeyCode::LeftArrow | KeyCode::Char('h') => self.side = Side::Toc,
            _ => {}
        }
    }

    /// Wheel scrolling from the mouse pre-dispatch.
    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll(self.rendered().lines.len());
        self.scroll = self.scroll.saturating_add_signed(delta).min(max);
    }

    fn spec(screen: Rect) -> LayerSpec {
        LayerSpec {
            title: "help".into(),
            badge: Some(" F1 ".into()),
            cols: (screen.cols * 4 / 5).max(60),
            rows: (screen.rows * 4 / 5).max(16),
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            bg: Tok::Slot(S::Panel),
            border: Tok::Slot(S::Faint),
        }
    }

    /// The overlay's outer box for mouse hit-testing (mirrors DetailOverlay).
    pub fn box_rect(&self, screen: Rect) -> Option<Rect> {
        crate::layer::box_rect(&Self::spec(screen), screen)
    }

    pub fn render(&mut self, surface: &mut Surface, screen: Rect) {
        let panel = Tok::Slot(S::Panel);
        let Some(inner) = open_layer(surface, screen, &Self::spec(screen)) else {
            return;
        };
        if inner.rows < 4 || inner.cols < 20 {
            return;
        }
        let toc_w = (inner.cols / 3).clamp(12, 26);
        let content_x = inner.x + toc_w + 2;
        let content_w = inner.cols - toc_w - 2;
        let body_y = inner.y + 2;
        let body_h = inner.rows - 3;
        self.last_dims = (content_w, body_h);

        // Header row: search input, or breadcrumb + title.
        let header = if let Some(s) = &self.search {
            Line::segs(vec![
                seg(Tok::Slot(S::Accent), "❯ ").bold(),
                seg(Tok::Slot(S::Text), s.query.clone()),
                seg(Tok::Slot(S::Ghost), "▏"),
                seg(
                    Tok::Slot(S::Ghost),
                    if s.query.is_empty() {
                        "  search every page…"
                    } else {
                        ""
                    },
                ),
            ])
        } else {
            let title = self
                .reg
                .page(&self.page)
                .map(|p| p.meta.title.clone())
                .unwrap_or_default();
            let crumb = self
                .reg
                .page(&self.page)
                .and_then(|p| p.meta.parent.clone())
                .and_then(|par| self.reg.page(&par).map(|p| p.meta.title.clone()))
                .map(|t| format!("{t} {} ", crate::caps::active_glyphs().chevron))
                .unwrap_or_default();
            Line::segs(vec![
                seg(Tok::Slot(S::Ghost2), crumb),
                seg(Tok::Slot(S::Text), title).bold(),
            ])
        };
        seg::draw_line(surface, inner.x, inner.y, inner.cols, &header, panel);
        seg::draw_line(
            surface,
            inner.x,
            inner.y + 1,
            inner.cols,
            &Line::Fill {
                ch: '─',
                fg: Tok::Slot(S::Ghost3),
            },
            panel,
        );

        // Left pane: search results while searching, else the TOC.
        if let Some(s) = &self.search {
            for (i, hit) in s.hits.iter().take(body_h).enumerate() {
                let selected = i == s.sel;
                let mut segs = vec![sp(1)];
                let mut title = seg(Tok::Slot(S::Text), hit.title.clone());
                if selected {
                    title = title.bg(Tok::SelAccent).bold();
                }
                segs.push(title);
                seg::draw_line(
                    surface,
                    inner.x,
                    body_y + i,
                    toc_w,
                    &Line::segs(segs),
                    panel,
                );
            }
            if s.hits.is_empty() && !s.query.is_empty() {
                seg::draw_line(
                    surface,
                    inner.x,
                    body_y,
                    toc_w,
                    &Line::segs(vec![sp(1), seg(Tok::Slot(S::Ghost), "no matches")]),
                    panel,
                );
            }
        } else {
            // Keep the cursor row visible.
            if self.toc_sel < self.toc_scroll {
                self.toc_scroll = self.toc_sel;
            } else if self.toc_sel >= self.toc_scroll + body_h {
                self.toc_scroll = self.toc_sel + 1 - body_h;
            }
            for (i, (depth, id, title)) in self
                .toc_rows
                .iter()
                .skip(self.toc_scroll)
                .take(body_h)
                .enumerate()
            {
                let row = self.toc_scroll + i;
                let current = *id == self.page;
                let cursor = row == self.toc_sel;
                let mut label = seg(
                    if current {
                        Tok::Slot(S::Text)
                    } else {
                        Tok::Slot(S::Dim)
                    },
                    title.clone(),
                );
                if current {
                    label = label.bold();
                }
                if cursor && self.side == Side::Toc {
                    label = label.bg(Tok::SelAccent);
                }
                let segs = vec![sp(1 + (*depth as usize) * 2), label];
                seg::draw_line(
                    surface,
                    inner.x,
                    body_y + i,
                    toc_w,
                    &Line::segs(segs),
                    panel,
                );
            }
        }

        // Separator.
        for i in 0..body_h {
            seg::draw_line(
                surface,
                inner.x + toc_w,
                body_y + i,
                1,
                &Line::segs(vec![seg(Tok::Slot(S::Ghost3), "│")]),
                panel,
            );
        }

        // Right pane: the page.
        let rendered = self.rendered();
        let max = self.max_scroll(rendered.lines.len());
        if self.scroll > max {
            self.scroll = max;
        }
        for (i, line) in rendered
            .lines
            .iter()
            .skip(self.scroll)
            .take(body_h)
            .enumerate()
        {
            seg::draw_line(surface, content_x, body_y + i, content_w, line, panel);
        }

        // Footer hints.
        let hints: &[(&str, &str)] = if self.search.is_some() {
            &[("↑↓", "select"), ("↵", "open"), ("esc", "cancel")]
        } else if self.side == Side::Toc {
            &[
                ("↑↓", "browse"),
                ("↵", "read"),
                ("tab", "page"),
                ("/", "search"),
                ("esc", "close"),
            ]
        } else {
            &[
                ("↑↓", "scroll"),
                ("n p", "links"),
                ("↵", "follow"),
                ("[ ]", "back/fwd"),
                ("/", "search"),
                ("esc", "close"),
            ]
        };
        let mut segs: Vec<Seg> = Vec::new();
        for (i, (k, label)) in hints.iter().enumerate() {
            if i > 0 {
                segs.push(seg(Tok::Slot(S::Ghost3), " · "));
            }
            segs.push(Seg::key(format!(" {k} ")));
            segs.push(seg(Tok::Slot(S::Ghost2), format!(" {label}")));
        }
        seg::draw_line(
            surface,
            inner.x,
            inner.y + inner.rows - 1,
            inner.cols,
            &Line::segs(segs),
            panel,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Arc<HelpRegistry> {
        let (reg, errors) =
            crate::help::pages::build_registry(&thegn_core::config::Config::default());
        assert!(errors.is_empty(), "{errors:?}");
        Arc::new(reg)
    }

    fn overlay() -> HelpOverlay {
        HelpOverlay::new(registry(), "index".to_string())
    }

    fn key(ov: &mut HelpOverlay, k: KeyCode) -> HelpOutcome {
        ov.handle_key(&k, Modifiers::NONE)
    }

    #[test]
    fn esc_q_and_f1_close() {
        for k in [KeyCode::Escape, KeyCode::Char('q'), KeyCode::Function(1)] {
            let mut ov = overlay();
            assert_eq!(key(&mut ov, k), HelpOutcome::Close);
        }
        let mut ov = overlay();
        assert_eq!(
            ov.handle_key(&KeyCode::Char('c'), Modifiers::CTRL),
            HelpOutcome::Close
        );
    }

    #[test]
    fn toc_browsing_previews_pages() {
        let mut ov = overlay();
        key(&mut ov, KeyCode::Tab); // content → toc
        let before = ov.page_id().to_string();
        key(&mut ov, KeyCode::DownArrow);
        assert_ne!(
            ov.page_id(),
            before,
            "moving the TOC cursor switches the page"
        );
        assert!(ov.back.is_empty(), "browsing is not history");
    }

    #[test]
    fn link_follow_and_history() {
        let mut ov = overlay();
        // index's first link exists and is a page link.
        key(&mut ov, KeyCode::Char('n'));
        assert_eq!(ov.link_sel, Some(0));
        key(&mut ov, KeyCode::Enter);
        assert_ne!(ov.page_id(), "index");
        let followed = ov.page_id().to_string();
        key(&mut ov, KeyCode::Char('['));
        assert_eq!(ov.page_id(), "index");
        key(&mut ov, KeyCode::Char(']'));
        assert_eq!(ov.page_id(), followed);
    }

    #[test]
    fn toc_cursor_follows_navigation() {
        let mut ov = overlay();
        key(&mut ov, KeyCode::Char('n'));
        key(&mut ov, KeyCode::Enter);
        let (_, id, _) = &ov.toc_rows[ov.toc_sel];
        assert_eq!(id, ov.page_id());
    }

    #[test]
    fn search_finds_and_jumps() {
        let mut ov = overlay();
        key(&mut ov, KeyCode::Char('/'));
        for c in "merge queue".chars() {
            key(&mut ov, KeyCode::Char(c));
        }
        let hits = ov.search.as_ref().unwrap().hits.clone();
        assert!(!hits.is_empty(), "search should hit the merge-queue page");
        assert!(hits.iter().any(|h| h.page == "merge-queue"));
        key(&mut ov, KeyCode::Enter);
        assert!(ov.search.is_none());
    }

    #[test]
    fn search_esc_cancels_without_moving() {
        let mut ov = overlay();
        key(&mut ov, KeyCode::Char('/'));
        key(&mut ov, KeyCode::Char('x'));
        key(&mut ov, KeyCode::Escape);
        assert!(ov.search.is_none());
        assert_eq!(ov.page_id(), "index");
        // A second Esc closes the overlay.
        assert_eq!(key(&mut ov, KeyCode::Escape), HelpOutcome::Close);
    }

    #[test]
    fn scroll_clamps() {
        let mut ov = overlay();
        for _ in 0..500 {
            key(&mut ov, KeyCode::DownArrow);
        }
        let max = ov.max_scroll(ov.rendered().lines.len());
        assert_eq!(ov.scroll, max);
        key(&mut ov, KeyCode::Char('g'));
        assert_eq!(ov.scroll, 0);
        key(&mut ov, KeyCode::Char('G'));
        assert_eq!(ov.scroll, max);
        ov.scroll_by(-1000);
        assert_eq!(ov.scroll, 0);
    }

    #[test]
    fn o_requests_open_in_panel() {
        let mut ov = overlay();
        assert_eq!(key(&mut ov, KeyCode::Char('o')), HelpOutcome::OpenInPanel);
    }

    #[test]
    fn renders_into_a_surface() {
        let mut ov = overlay();
        let mut s = Surface::new(100, 30);
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 100,
            rows: 30,
        };
        ov.render(&mut s, screen);
        let text = s.screen_chars_to_string();
        assert!(text.contains("thegn"), "page body drawn: {text}");
        assert!(text.contains("Welcome"), "TOC row drawn");
        assert!(text.contains("help"), "layer title");
    }

    #[test]
    fn tiny_screen_never_panics() {
        let mut ov = overlay();
        for (w, h) in [(6, 3), (12, 6), (40, 12), (80, 24)] {
            let mut s = Surface::new(w, h);
            ov.render(
                &mut s,
                Rect {
                    x: 0,
                    y: 0,
                    cols: w,
                    rows: h,
                },
            );
        }
    }
}
