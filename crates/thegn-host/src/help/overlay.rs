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
    /// First visible hit. Without it the selection could walk past the last
    /// drawn row, leaving nothing highlighted while `↵` still opened the
    /// invisible hit.
    scroll: usize,
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
    /// The chord that opens this overlay, resolved from the keymap at open
    /// time and shown as the layer badge — so a rebind of `help` is reflected
    /// instead of the badge always claiming `F1`.
    badge: String,
    /// Where things were painted on the last render, for mouse hit-testing.
    /// `None` before the first render, when a click can't mean anything yet.
    hits: Option<HitAreas>,
}

/// The last frame's clickable geometry. Recorded by `render` so clicks land on
/// what the user actually sees — the same paint-and-hit-test-from-one-pass rule
/// the chrome follows.
#[derive(Debug, Clone, Default)]
struct HitAreas {
    /// Left pane rows: `(y, toc_row_index)` — TOC rows, or search hits.
    left_rows: Vec<(usize, usize)>,
    /// Left pane column range, `x..x + cols`.
    left_x: (usize, usize),
    /// First body row of the right pane, and its column range.
    content_y: usize,
    content_x: (usize, usize),
    /// Visible body rows in the right pane.
    body_h: usize,
}

/// The dim preview line under a search hit: the matched body line with the
/// query itself inverted. `hl_start`/`hl_len` are **char** offsets (the core
/// matcher folds ASCII case over chars), so slice by chars, not bytes.
fn snippet_segs(sn: &thegn_core::help::Snippet) -> Vec<Seg> {
    let chars: Vec<char> = sn.text.chars().collect();
    let start = sn.hl_start.min(chars.len());
    let end = (start + sn.hl_len).min(chars.len());
    let take = |r: std::ops::Range<usize>| chars[r].iter().collect::<String>();
    let mut out = vec![sp(3)];
    if start > 0 {
        out.push(seg(Tok::Slot(S::Ghost), take(0..start)));
    }
    if end > start {
        out.push(seg(Tok::Slot(S::Text), take(start..end)).bg(Tok::SelAccent));
    }
    if end < chars.len() {
        out.push(seg(Tok::Slot(S::Ghost), take(end..chars.len())));
    }
    out
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
    pub fn new(reg: Arc<HelpRegistry>, page: String, badge: String) -> Self {
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
            badge,
            hits: None,
        }
    }

    pub fn page_id(&self) -> &str {
        &self.page
    }

    fn rendered(&self) -> RenderedPage {
        // `page_blocks` appends the "Referenced by" footer, whose entries are
        // ordinary links — so `n`/`p`/`↵` reach them like any other.
        let blocks = super::render::page_blocks(&self.reg, &self.page);
        render_page(&blocks, self.last_dims.0, self.link_sel)
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
            s.scroll = 0;
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
                    scroll: 0,
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

    fn spec(&self, screen: Rect) -> LayerSpec {
        LayerSpec {
            title: "help".into(),
            badge: Some(format!(" {} ", self.badge)),
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
        crate::layer::box_rect(&self.spec(screen), screen)
    }

    pub fn render(&mut self, surface: &mut Surface, screen: Rect) {
        let panel = Tok::Slot(S::Panel);
        let Some(inner) = open_layer(surface, screen, &self.spec(screen)) else {
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
        let mut hits = HitAreas {
            left_x: (inner.x, inner.x + toc_w),
            content_y: body_y,
            content_x: (content_x, content_x + content_w),
            body_h,
            ..Default::default()
        };

        // Header row: search input, or breadcrumb + title.
        let header = if let Some(s) = &self.search {
            Line::segs(vec![
                seg(Tok::Slot(S::Accent), "❯ ").bold(),
                seg(Tok::Slot(S::Text), s.query.clone()),
                crate::seg::caret(),
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

        // While searching, the results take the whole body: the TOC is
        // irrelevant, and the snippets need the width. Each hit is two rows —
        // its title, then the matched line with the query highlighted.
        const ROWS_PER_HIT: usize = 2;
        let visible = (body_h / ROWS_PER_HIT).max(1);
        // Keep the selection on screen. Persisted, so the window is stable
        // across frames — this is the fix for `sel` walking past the last drawn
        // row with nothing highlighted while `↵` still opened the hidden hit.
        if let Some(s) = self.search.as_mut() {
            s.scroll = s.scroll.min(s.sel);
            if s.sel >= s.scroll + visible {
                s.scroll = s.sel + 1 - visible;
            }
        }
        if let Some(s) = &self.search {
            let scroll = s.scroll;
            for (i, hit) in s.hits.iter().skip(scroll).take(visible).enumerate() {
                let y = body_y + i * ROWS_PER_HIT;
                hits.left_rows.push((y, scroll + i));
                hits.left_rows.push((y + 1, scroll + i));
                let selected = scroll + i == s.sel;
                let mut title = seg(Tok::Slot(S::Text), hit.title.clone());
                if selected {
                    title = title.bg(Tok::SelAccent).bold();
                }
                seg::draw_line(
                    surface,
                    inner.x,
                    y,
                    inner.cols,
                    &Line::segs(vec![sp(1), title]),
                    panel,
                );
                if let Some(sn) = &hit.snippet
                    && y + 1 < body_y + body_h
                {
                    seg::draw_line(
                        surface,
                        inner.x,
                        y + 1,
                        inner.cols,
                        &Line::segs(snippet_segs(sn)),
                        panel,
                    );
                }
            }
            if s.hits.is_empty() && !s.query.is_empty() {
                seg::draw_line(
                    surface,
                    inner.x,
                    body_y,
                    inner.cols,
                    &Line::segs(vec![sp(1), seg(Tok::Slot(S::Ghost), "no matches")]),
                    panel,
                );
            }
            self.draw_footer(surface, inner, panel);
            self.hits = Some(hits);
            return;
        }
        {
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
                hits.left_rows.push((body_y + i, row));
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

        self.draw_footer(surface, inner, panel);
        self.hits = Some(hits);
    }

    /// The bottom hint row, whose set depends on what owns the keyboard.
    fn draw_footer(&self, surface: &mut Surface, inner: Rect, panel: Tok) {
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
            // Seven hints is the most this row holds; labels are kept short so
            // the set survives a typical width (it truncates on a narrow
            // terminal, as it always has).
            &[
                ("↑↓", "scroll"),
                ("n p", "links"),
                ("↵", "follow"),
                ("[ ]", "back"),
                ("/", "search"),
                ("o", "panel"),
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

    /// Handle a left-press inside the overlay box.
    ///
    /// Left pane: a row selects that TOC entry (browsing, so no history — the
    /// same semantics as moving the cursor with `↑↓`) or opens that search hit.
    /// Right pane: clicking a line containing a link follows it.
    pub fn handle_click(&mut self, x: usize, y: usize) -> HelpOutcome {
        let Some(hits) = self.hits.clone() else {
            return HelpOutcome::Pending;
        };
        // Left pane: TOC rows, or search results while searching.
        if x >= hits.left_x.0 && x < hits.left_x.1 {
            if let Some((_, idx)) = hits.left_rows.iter().find(|(row, _)| *row == y) {
                if self.search.is_some() {
                    if let Some(s) = self.search.as_mut() {
                        s.sel = *idx;
                    }
                    self.open_hit();
                } else {
                    self.side = Side::Toc;
                    self.toc_sel = (*idx).min(self.toc_rows.len().saturating_sub(1));
                    self.open_toc_row();
                }
            }
            return HelpOutcome::Pending;
        }
        // Right pane: follow a link on the clicked line, if there is one.
        if x >= hits.content_x.0 && x < hits.content_x.1 && y >= hits.content_y {
            let row = y - hits.content_y;
            if row >= hits.body_h {
                return HelpOutcome::Pending;
            }
            self.side = Side::Content;
            let line = self.scroll + row;
            let rendered = self.rendered();
            if let Some(idx) = rendered.links.iter().position(|l| l.line == line) {
                self.link_sel = Some(idx);
                self.follow_link();
            }
        }
        HelpOutcome::Pending
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
        HelpOverlay::new(registry(), "index".to_string(), "F1".to_string())
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

    /// The bug: `sel` clamped to `hits.len()`, but only `body_h` rows were
    /// drawn — arrowing past the fold left nothing highlighted while `↵` still
    /// opened the invisible hit. The window must follow the selection.
    #[test]
    fn search_results_scroll_to_keep_the_selection_visible() {
        let (mut ov, screen) = rendered_overlay();
        key(&mut ov, KeyCode::Char('/'));
        for c in "the".chars() {
            key(&mut ov, KeyCode::Char(c));
        }
        let n = ov.search.as_ref().unwrap().hits.len();
        assert!(n > 4, "need enough hits to overflow, got {n}");
        for _ in 0..n {
            key(&mut ov, KeyCode::DownArrow);
        }
        let mut s = Surface::new(100, 30);
        ov.render(&mut s, screen);
        let se = ov.search.as_ref().unwrap();
        assert_eq!(se.sel, n - 1, "selection at the last hit");
        // The selected row is inside the drawn window.
        let visible = (ov.last_dims.1 / 2).max(1);
        assert!(
            se.sel >= se.scroll && se.sel < se.scroll + visible,
            "sel {} outside window {}..{}",
            se.sel,
            se.scroll,
            se.scroll + visible
        );
    }

    /// Hits used to render as a bare title list; the snippet was computed,
    /// tested, and thrown away. It is now drawn under each hit.
    #[test]
    fn search_results_show_their_snippet() {
        let (mut ov, screen) = rendered_overlay();
        key(&mut ov, KeyCode::Char('/'));
        for c in "worktree".chars() {
            key(&mut ov, KeyCode::Char(c));
        }
        let snippet = ov
            .search
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .find_map(|h| h.snippet.as_ref())
            .expect("a body match")
            .text
            .clone();
        let mut s = Surface::new(100, 30);
        ov.render(&mut s, screen);
        let text = s.screen_chars_to_string();
        // The snippet's opening words are drawn (the line may clip at width).
        let head: String = snippet.chars().take(20).collect();
        assert!(text.contains(head.trim()), "snippet drawn: {text}");
    }

    #[test]
    fn snippet_segs_highlight_the_match_on_char_boundaries() {
        let sn = thegn_core::help::Snippet {
            text: "naïve — Bold text".to_string(),
            hl_start: 8,
            hl_len: 4,
            section: None,
        };
        let segs = snippet_segs(&sn);
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined.trim_start(), "naïve — Bold text");
        // Out-of-range offsets clamp instead of panicking on a multibyte slice.
        let bad = thegn_core::help::Snippet {
            text: "naïve".to_string(),
            hl_start: 99,
            hl_len: 99,
            section: None,
        };
        let segs = snippet_segs(&bad);
        let joined: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(joined.trim_start(), "naïve");
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

    /// Render once so `hits` is populated, then click.
    fn rendered_overlay() -> (HelpOverlay, Rect) {
        let mut ov = overlay();
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 100,
            rows: 30,
        };
        let mut s = Surface::new(100, 30);
        ov.render(&mut s, screen);
        (ov, screen)
    }

    /// The reported bug, end to end: opening help left the focused pane's
    /// hardware cursor blinking on top of the help box, because the loop decided
    /// caret visibility from a hand-written list of modals that help was never
    /// added to. Rendering must now register a cover all by itself — this
    /// exercises the real wiring (`render` → `layer::open_layer` →
    /// `caret::cover`), not just the arbitration rules.
    #[test]
    fn rendering_help_hides_a_pane_caret_underneath_it() {
        crate::caret::begin_frame();
        assert!(crate::caret::no_covers(), "nothing painted yet");

        let (ov, screen) = rendered_overlay();
        assert!(!crate::caret::no_covers(), "help registered a cover");

        // A caret in the middle of the screen is under the centered box; one in
        // the far corner is not. The point of a geometric test: no popup is
        // enumerated anywhere, and the answer still tracks what was painted.
        let boxr = ov.box_rect(screen).expect("help box placed");
        let inside = (boxr.x + boxr.cols / 2, boxr.y + boxr.rows / 2);
        assert_eq!(crate::caret::resolve_frame(Some(inside)), None);

        let outside = (screen.cols - 1, screen.rows - 1);
        assert!(
            outside.0 >= boxr.x + boxr.cols || outside.1 >= boxr.y + boxr.rows,
            "corner really is outside the box"
        );
        assert_eq!(crate::caret::resolve_frame(Some(outside)), Some(outside));

        crate::caret::begin_frame();
    }

    /// Typing in help's `/` search must put the REAL cursor in the field, not
    /// hide it along with everything else the box covers.
    #[test]
    fn help_search_claims_the_real_cursor() {
        crate::caret::begin_frame();
        let mut ov = overlay();
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 100,
            rows: 30,
        };
        let mut s = Surface::new(100, 30);
        ov.handle_key(&KeyCode::Char('/'), Modifiers::NONE);
        ov.handle_key(&KeyCode::Char('p'), Modifiers::NONE);
        ov.render(&mut s, screen);

        let inner = ov.box_rect(screen).map(|b| (b.x + 2, b.y + 1)).unwrap();
        // `❯ ` then the one-character query, so the caret sits at +3.
        assert_eq!(
            crate::caret::resolve_frame(Some((0, 0))),
            Some((inner.0 + 3, inner.1)),
            "the claim beats both the pane caret and help's own cover"
        );
        crate::caret::begin_frame();
    }

    /// A click before the first render can't mean anything, and must not panic.
    #[test]
    fn click_before_render_is_inert() {
        let mut ov = overlay();
        assert_eq!(ov.handle_click(10, 10), HelpOutcome::Pending);
        assert_eq!(ov.page_id(), "index");
    }

    #[test]
    fn clicking_a_toc_row_opens_that_page() {
        let (mut ov, _) = rendered_overlay();
        let before = ov.page_id().to_string();
        // Second visible TOC row (the first is `index`, already open).
        let (y, _) = ov.hits.as_ref().unwrap().left_rows[1];
        let x = ov.hits.as_ref().unwrap().left_x.0 + 1;
        ov.handle_click(x, y);
        assert_ne!(ov.page_id(), before, "click switched the page");
        assert_eq!(ov.side, Side::Toc);
        assert!(ov.back.is_empty(), "browsing by click is not history");
    }

    #[test]
    fn clicking_a_link_line_follows_it() {
        let (mut ov, _) = rendered_overlay();
        let first = ov.rendered().links.first().map(|l| l.line).unwrap();
        let h = ov.hits.as_ref().unwrap().clone();
        ov.handle_click(h.content_x.0 + 1, h.content_y + first);
        assert_ne!(ov.page_id(), "index", "followed the link");
        assert_eq!(ov.back.len(), 1, "following a link IS history");
    }

    /// A click on a body line with no link only moves focus, and a click past
    /// the last visible row does nothing at all (and never panics).
    #[test]
    fn clicking_content_without_a_link_only_moves_focus() {
        let (mut ov, _) = rendered_overlay();
        let h = ov.hits.as_ref().unwrap().clone();
        // Row 0 of `index` is the H1 — text, no link.
        ov.handle_click(h.content_x.0 + 1, h.content_y);
        assert_eq!(ov.page_id(), "index", "no link on that line");
        assert_eq!(ov.side, Side::Content);
        // Past the body: inert.
        ov.handle_click(h.content_x.0 + 1, h.content_y + h.body_h + 50);
        assert_eq!(ov.page_id(), "index");
    }

    #[test]
    fn clicking_a_search_hit_opens_it() {
        let (mut ov, screen) = rendered_overlay();
        key(&mut ov, KeyCode::Char('/'));
        for c in "merge queue".chars() {
            key(&mut ov, KeyCode::Char(c));
        }
        // Re-render so the hit rows are recorded.
        let mut s = Surface::new(100, 30);
        ov.render(&mut s, screen);
        let (y, _) = ov.hits.as_ref().unwrap().left_rows[0];
        let x = ov.hits.as_ref().unwrap().left_x.0 + 1;
        ov.handle_click(x, y);
        assert!(ov.search.is_none(), "hit opened, search closed");
        assert_eq!(ov.page_id(), "merge-queue");
    }

    /// `o` (dock the page in the panel) is a real feature that used to appear
    /// in no hint row at all. The footer holds the full set at a typical width.
    #[test]
    fn footer_hints_advertise_open_in_panel() {
        let mut ov = overlay();
        let screen = Rect {
            x: 0,
            y: 0,
            cols: 120,
            rows: 34,
        };
        let mut s = Surface::new(120, 34);
        ov.render(&mut s, screen);
        let text = s.screen_chars_to_string();
        for want in ["scroll", "follow", "search", "panel", "close"] {
            assert!(text.contains(want), "footer hint `{want}` shown: {text}");
        }
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
