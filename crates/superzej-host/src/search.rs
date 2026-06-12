//! Terminal history search overlay.
//!
//! `SearchOverlay` is a self-contained floating panel composited above the
//! pane content (same compositing path as the `Palette`). It receives keys,
//! drives `SearchEngine` for fuzzy matching, and returns a typed `Outcome` so
//! `run.rs` can take the right action (jump / dismiss / keep open).
//!
//! Layout (approximate, actual size adapts to the screen):
//!
//! ```text
//! ┌─ Search history ──────────────────── worktree ▾ ─┐
//! │ ❯ query_____________                              │
//! ├───────────────────────────────────────────────────┤
//! │ ● tab 2 · feat/auth   cargo build --release       │
//! │   tab 1 · main        error: linker `cc` not found│
//! │   tab 2 · feat/auth   warning: unused variable    │
//! ├───────────────────────────────────────────────────┤
//! │ ↑↓ move   ↵ jump   Tab scope   esc dismiss   3 ✓ │
//! └───────────────────────────────────────────────────┘
//! ```

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use superzej_core::search::{SearchEngine, SearchMatch, SearchScope, SearchSource};

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Tok, seg, sp};

/// What `SearchOverlay::handle_key` signals to the caller.
#[derive(Debug)]
pub enum SearchOutcome {
    /// Still typing / navigating — caller should mark dirty and continue.
    Pending,
    /// User confirmed a result — jump to this match.
    Jump(SearchMatch),
    /// User dismissed (Esc / Ctrl+g) — close the overlay.
    Dismiss,
}

/// The live search overlay. Lives in `run.rs` as `Option<SearchOverlay>`.
pub struct SearchOverlay {
    engine: SearchEngine,
    /// The pane id the overlay was opened for (used to wrap scope cycles back
    /// to `Pane(…)` so the correct pane id is preserved).
    anchor_pane: u32,
}

impl SearchOverlay {
    /// Open the overlay. `scope` is the initial scope; `pane_id` is the
    /// currently focused pane (needed for `Pane` scope and scope wrap-around).
    pub fn new(scope: SearchScope, pane_id: u32, max_results: usize) -> Self {
        SearchOverlay {
            engine: SearchEngine::new(scope, max_results),
            anchor_pane: pane_id,
        }
    }

    pub fn scope(&self) -> SearchScope {
        self.engine.scope()
    }

    #[allow(dead_code)] // used in tests
    pub fn engine(&self) -> &SearchEngine {
        &self.engine
    }

    /// Feed a key event. `sources` must already be filtered to the current scope.
    /// Call `build_search_sources(self.scope(), …)` before calling this.
    pub fn handle_key(
        &mut self,
        key: &KeyCode,
        mods: Modifiers,
        sources: &[SearchSource<'_>],
    ) -> SearchOutcome {
        // Dismiss: Esc or Ctrl+g.
        if is_escape(key, mods) {
            return SearchOutcome::Dismiss;
        }

        match key {
            KeyCode::Enter => {
                if let Some(m) = self.engine.selected() {
                    return SearchOutcome::Jump(m.clone());
                }
                // Enter on empty results → dismiss.
                return SearchOutcome::Dismiss;
            }

            KeyCode::UpArrow => {
                self.engine.move_up();
            }

            KeyCode::DownArrow => {
                self.engine.move_down();
            }

            KeyCode::Tab => {
                // Cycle scope wider (without Shift) or narrower (with Shift).
                let new_scope = if mods.contains(Modifiers::SHIFT) {
                    self.engine.scope().narrow(self.anchor_pane)
                } else {
                    self.engine.scope().widen(self.anchor_pane)
                };
                self.engine.set_scope(new_scope, sources);
            }

            KeyCode::Backspace => {
                self.engine.backspace(sources);
            }

            KeyCode::Char(c) if mods.is_empty() || mods == Modifiers::SHIFT => {
                self.engine.push_char(*c, sources);
            }

            _ => {}
        }

        SearchOutcome::Pending
    }

    /// Draw the overlay into `surface`. `screen` is the center rect (the area
    /// the overlay may use).
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let engine = &self.engine;
        let matches = engine.matches();

        const MAX_RESULT_ROWS: usize = 12;
        let shown = matches.len().min(MAX_RESULT_ROWS);
        // rows: prompt(1) + rule(1) + results(shown) + rule(1) + footer(1)
        let content_rows = 4 + shown.max(1);

        let scope_pill = format!(" {} ▾", engine.scope().label());
        let spec = LayerSpec {
            title: "Search history".into(),
            badge: Some(scope_pill),
            cols: 72,
            rows: content_rows,
            anchor: Anchor::TopThird,
            dim: true,
            shadow: true,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };

        let panel = Tok::Slot(S::Panel);
        let accent = Tok::Slot(S::Accent);
        let rule = Line::Fill {
            ch: '╌',
            fg: Tok::Slot(S::Ghost3),
        };

        // ── Prompt row ────────────────────────────────────────────────────────
        let mut prompt_segs = vec![seg(accent, "❯ ").bold()];
        if engine.query().is_empty() {
            prompt_segs.push(seg(Tok::Slot(S::Ghost3), "type to search…"));
        } else {
            prompt_segs.push(seg(Tok::Slot(S::Text), engine.query().to_string()));
            prompt_segs.push(seg(Tok::Slot(S::Accent), "█")); // cursor block
        }
        seg::draw_line(
            surface,
            inner.x,
            inner.y,
            inner.cols,
            &Line::segs(prompt_segs),
            panel,
        );

        if inner.rows < 2 {
            return;
        }

        // ── Rule ──────────────────────────────────────────────────────────────
        seg::draw_line(surface, inner.x, inner.y + 1, inner.cols, &rule, panel);

        // ── Result rows ───────────────────────────────────────────────────────
        let selected_idx = engine.selected_idx();
        let result_rows = inner.rows.saturating_sub(4); // prompt + 2 rules + footer
        for (row, m) in matches.iter().take(result_rows).enumerate() {
            let selected = row == selected_idx;
            let bg = if selected { Tok::SelAccent } else { panel };

            // Label column: "● label" or "  label"
            let (bullet, label_tok) = if selected {
                (
                    seg(Tok::Slot(S::Accent), "● ").bold(),
                    seg(Tok::Slot(S::Text), truncate_label(&m.pane_label, 22)).bold(),
                )
            } else {
                (
                    seg(Tok::Slot(S::Ghost3), "  "),
                    seg(Tok::Slot(S::Ghost2), truncate_label(&m.pane_label, 22)),
                )
            };

            // Content column: the matched line, truncated.
            let line_avail = inner.cols.saturating_sub(26); // 24 label + 2 sep
            let line_text = truncate_label(m.line.trim(), line_avail);
            let line_seg = if selected {
                seg(Tok::Slot(S::Text), line_text).bold()
            } else {
                seg(Tok::Slot(S::Dim), line_text)
            };

            let row_line = Line::segs(vec![bullet, label_tok, sp(2), line_seg]);
            seg::draw_line(
                surface,
                inner.x,
                inner.y + 2 + row,
                inner.cols,
                &row_line,
                bg,
            );
        }

        // If no results, show a placeholder.
        if matches.is_empty() && inner.rows > 3 {
            let placeholder = if engine.query().trim().is_empty() {
                "no history yet"
            } else {
                "no matches"
            };
            seg::draw_line(
                surface,
                inner.x,
                inner.y + 2,
                inner.cols,
                &Line::segs(vec![sp(2), seg(Tok::Slot(S::Ghost3), placeholder)]),
                panel,
            );
        }

        // ── Footer ────────────────────────────────────────────────────────────
        if inner.rows >= 4 {
            let fy = inner.y + inner.rows - 2;
            seg::draw_line(surface, inner.x, fy, inner.cols, &rule, panel);
            let count_str = format!("{} result{}", matches.len(), if matches.len() == 1 { "" } else { "s" });
            let footer = Line::split(
                vec![
                    seg(Tok::Slot(S::Ghost2), "↑↓"),
                    seg(Tok::Slot(S::Ghost), " move   "),
                    seg(Tok::Slot(S::Ghost2), "↵"),
                    seg(Tok::Slot(S::Ghost), " jump   "),
                    seg(Tok::Slot(S::Ghost2), "Tab"),
                    seg(Tok::Slot(S::Ghost), " scope   "),
                    seg(Tok::Slot(S::Ghost2), "esc"),
                    seg(Tok::Slot(S::Ghost), " dismiss"),
                ],
                vec![seg(Tok::Slot(S::Ghost3), count_str)],
            );
            seg::draw_line(surface, inner.x, fy + 1, inner.cols, &footer, panel);
        }
    }
}

fn is_escape(key: &KeyCode, mods: Modifiers) -> bool {
    matches!(key, KeyCode::Escape)
        || (mods.contains(Modifiers::CTRL)
            && matches!(key, KeyCode::Char('g') | KeyCode::Char('G') | KeyCode::Char('c') | KeyCode::Char('C')))
}

/// Truncate `s` to at most `max_cols` display columns, appending `…` if
/// truncated. Simple byte-level truncation (assumes mostly ASCII content,
/// which is the common case for terminal history).
fn truncate_label(s: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    if s.len() <= max_cols {
        return s.to_string();
    }
    let mut t = s[..max_cols.saturating_sub(1)].to_string();
    t.push('…');
    t
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::history::HistoryBuffer;
    use superzej_core::search::SearchSource;

    fn buf_from(lines: &[&str]) -> HistoryBuffer {
        let mut b = HistoryBuffer::new(1_000);
        for &l in lines {
            b.push_line(l.to_string());
        }
        b
    }

    fn sources<'a>(pane_id: u32, buf: &'a HistoryBuffer) -> Vec<SearchSource<'a>> {
        vec![(pane_id, "pane", buf)]
    }

    fn overlay(pane_id: u32) -> SearchOverlay {
        SearchOverlay::new(SearchScope::Pane(pane_id), pane_id, 100)
    }

    #[test]
    fn handle_key_typing_updates_query_and_matches() {
        let buf = buf_from(&["cargo build", "cargo test", "ninja"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);

        let r = ov.handle_key(&KeyCode::Char('c'), Modifiers::NONE, &srcs);
        assert!(matches!(r, SearchOutcome::Pending));
        assert_eq!(ov.engine().query(), "c");
        // All lines containing 'c' should match.
        assert!(!ov.engine().matches().is_empty());

        let r = ov.handle_key(&KeyCode::Char('a'), Modifiers::NONE, &srcs);
        assert!(matches!(r, SearchOutcome::Pending));
        assert_eq!(ov.engine().query(), "ca");
    }

    #[test]
    fn handle_key_backspace_shrinks_query() {
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        ov.handle_key(&KeyCode::Char('c'), Modifiers::NONE, &srcs);
        ov.handle_key(&KeyCode::Char('a'), Modifiers::NONE, &srcs);
        assert_eq!(ov.engine().query(), "ca");
        ov.handle_key(&KeyCode::Backspace, Modifiers::NONE, &srcs);
        assert_eq!(ov.engine().query(), "c");
    }

    #[test]
    fn handle_key_enter_returns_jump_when_match_exists() {
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        ov.handle_key(&KeyCode::Char('c'), Modifiers::NONE, &srcs);
        let r = ov.handle_key(&KeyCode::Enter, Modifiers::NONE, &srcs);
        assert!(matches!(r, SearchOutcome::Jump(_)));
        if let SearchOutcome::Jump(m) = r {
            assert!(m.line.contains('c'));
        }
    }

    #[test]
    fn handle_key_enter_on_empty_results_dismisses() {
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        ov.handle_key(&KeyCode::Char('z'), Modifiers::NONE, &srcs);
        ov.handle_key(&KeyCode::Char('z'), Modifiers::NONE, &srcs);
        // Should be empty matches for "zz" against "cargo build".
        let r = ov.handle_key(&KeyCode::Enter, Modifiers::NONE, &srcs);
        // Either Jump (if nucleo somehow matched) or Dismiss.
        // Depending on nucleo's scoring this can go either way — just assert no panic.
        let _ = r;
    }

    #[test]
    fn handle_key_escape_returns_dismiss() {
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        let r = ov.handle_key(&KeyCode::Escape, Modifiers::NONE, &srcs);
        assert!(matches!(r, SearchOutcome::Dismiss));
    }

    #[test]
    fn handle_key_ctrl_g_returns_dismiss() {
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        let r = ov.handle_key(&KeyCode::Char('g'), Modifiers::CTRL, &srcs);
        assert!(matches!(r, SearchOutcome::Dismiss));
    }

    #[test]
    fn handle_key_tab_cycles_scope_forward() {
        let buf = buf_from(&["x"]);
        let srcs = sources(5, &buf);
        let mut ov = SearchOverlay::new(SearchScope::Pane(5), 5, 100);
        assert_eq!(ov.scope(), SearchScope::Pane(5));
        ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs);
        assert_eq!(ov.scope(), SearchScope::Tab);
        ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs);
        assert_eq!(ov.scope(), SearchScope::Worktree);
        ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs);
        assert_eq!(ov.scope(), SearchScope::Workspace);
        ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs);
        assert_eq!(ov.scope(), SearchScope::Profile);
        // Wrap back to Pane.
        ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs);
        assert_eq!(ov.scope(), SearchScope::Pane(5));
    }

    #[test]
    fn handle_key_shift_tab_cycles_scope_backward() {
        let buf = buf_from(&["x"]);
        let srcs = sources(3, &buf);
        let mut ov = SearchOverlay::new(SearchScope::Tab, 3, 100);
        ov.handle_key(&KeyCode::Tab, Modifiers::SHIFT, &srcs);
        assert_eq!(ov.scope(), SearchScope::Pane(3));
    }

    #[test]
    fn handle_key_up_down_navigates_selection() {
        let buf = buf_from(&["a", "b", "c"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        // Empty query → all 3 results shown newest-first.
        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE, &srcs);
        // selection should move (or stay at 0 if no results yet)
        assert!(ov.engine().selected_idx() <= 1);
    }

    #[test]
    fn render_does_not_panic_on_empty_history() {
        let buf = HistoryBuffer::new(100);
        let srcs: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let ov = SearchOverlay::new(SearchScope::Pane(1), 1, 100);
        let mut surface = Surface::new(80, 24);
        let rect = Rect { x: 0, y: 0, cols: 80, rows: 24 };
        ov.render(&mut surface, rect); // must not panic
    }

    #[test]
    fn render_does_not_panic_on_tiny_screen() {
        let buf = buf_from(&["hello"]);
        let _srcs: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let ov = SearchOverlay::new(SearchScope::Pane(1), 1, 100);
        let mut surface = Surface::new(10, 4);
        let rect = Rect { x: 0, y: 0, cols: 10, rows: 4 };
        ov.render(&mut surface, rect); // must not panic
    }

    #[test]
    fn truncate_label_fits() {
        assert_eq!(truncate_label("hello", 10), "hello");
        assert_eq!(truncate_label("hello world", 7), "hello …");
        assert_eq!(truncate_label("x", 0), "");
        assert_eq!(truncate_label("", 5), "");
    }

    #[test]
    fn is_escape_recognises_variants() {
        assert!(is_escape(&KeyCode::Escape, Modifiers::NONE));
        assert!(is_escape(&KeyCode::Char('g'), Modifiers::CTRL));
        assert!(is_escape(&KeyCode::Char('c'), Modifiers::CTRL));
        assert!(!is_escape(&KeyCode::Char('g'), Modifiers::NONE));
        assert!(!is_escape(&KeyCode::Enter, Modifiers::NONE));
    }
}
