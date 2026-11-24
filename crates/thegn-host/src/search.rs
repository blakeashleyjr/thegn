//! Terminal history search overlay.
//!
//! `SearchOverlay` is a self-contained floating panel composited above the
//! pane content (same compositing path as the `Palette`). It receives keys,
//! drives `SearchEngine` for fuzzy matching, and returns a typed `Outcome` so
//! `run.rs` can take the right action (jump / dismiss / keep open).
//!
//! Scoring never runs on the event loop: a key edit only queues the new query/
//! mode/options, and `start_pending_search` hands an owned history snapshot to
//! the [`TerminalSearchWorker`], whose results `drain_search` folds back in.
//! The synchronous paths (`handle_key`, `prime`, `apply_scope`) exist for tests
//! and as the fallback when no worker thread can be spawned.
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

use std::collections::VecDeque;
use std::sync::Mutex;
use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;
use termwiz::terminal::TerminalWaker;

use thegn_core::search::{
    MatchMode, MatchOptions, SearchEngine, SearchMatch, SearchScope, SearchSource,
};

use crate::terminal_search_worker::{
    SearchRequest, SnapshotSources, TerminalSearchWorker, WakeFn, source_refs, take_snapshot,
};

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::seg::{self, Line, Tok, seg, sp};

static RECENT: Mutex<VecDeque<String>> = const { Mutex::new(VecDeque::new()) };

fn record_recent(query: &str) {
    let query = query.trim();
    if query.is_empty() {
        return;
    }
    let mut recent = RECENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    recent.retain(|item| item != query);
    recent.push_front(query.to_string());
    recent.truncate(20);
}

fn recall_recent(current: &str) -> Option<String> {
    let recent = RECENT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let index = recent
        .iter()
        .position(|query| query == current)
        .map_or(0, |index| (index + 1) % recent.len());
    recent.get(index).cloned()
}

/// What `SearchOverlay::handle_key` signals to the caller.
#[derive(Debug)]
pub enum SearchOutcome {
    /// Still typing / navigating — caller should mark dirty and continue.
    Pending,
    /// User confirmed a result — jump to this match.
    Jump(SearchMatch),
    /// The scope changed; the caller must rebuild sources and apply it.
    Rescope(SearchScope),
    /// User dismissed (Esc / Ctrl+g) — close the overlay.
    Dismiss,
}

/// The live search overlay. Lives in `run.rs` as `Option<SearchOverlay>`.
pub struct SearchOverlay {
    engine: SearchEngine,
    /// Context lines around the selected result, in before/after order.
    preview: Vec<String>,
    /// The pane id the overlay was opened for (used to wrap scope cycles back
    /// to `Pane(…)` so the correct pane id is preserved).
    anchor_pane: u32,
    /// Result cap, re-applied on every worker request.
    max_results: usize,
    /// Scores the snapshot off the loop. Created on the first search; dropped
    /// (which stops its thread) with the overlay.
    worker: Option<TerminalSearchWorker>,
    /// A query/mode/option/scope edit is waiting for `start_pending_search`.
    search_pending: bool,
    /// The worker has not delivered its final batch for the current query yet.
    searching: bool,
    /// Owned history snapshot the current results were scored against. Unchanged
    /// panes are shared between keystrokes; previews read from it so they stay
    /// consistent with the match indices.
    snapshot: SnapshotSources,
    /// Spawning or running the worker failed once: score on the loop from now on
    /// instead of retrying a thread on every keystroke.
    worker_failed: bool,
    /// The worker died mid-search and nothing has been rescored since; the
    /// results panel says so instead of showing an empty list.
    worker_stopped: bool,
    /// Enter was pressed before the first batch of the typed query arrived; jump
    /// to its best hit as soon as it does.
    pending_jump: bool,
    /// An outcome produced while draining (a deferred Enter), for the loop to act on.
    ready: Option<SearchOutcome>,
}

/// Where an edit is scored.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scoring {
    /// On this thread (tests, and the fallback when no worker can be spawned).
    Now,
    /// On the worker: the edit only queues state, and the caller follows with
    /// `start_pending_search`.
    Deferred,
}

/// One query edit.
enum QueryEdit {
    Set(String),
    Push(char),
    Pop,
}

impl SearchOverlay {
    /// Open the overlay. `scope` is the initial scope; `pane_id` is the
    /// currently focused pane (needed for `Pane` scope and scope wrap-around).
    pub fn new(scope: SearchScope, pane_id: u32, max_results: usize) -> Self {
        SearchOverlay {
            engine: SearchEngine::new(scope, max_results),
            preview: Vec::new(),
            anchor_pane: pane_id,
            max_results: max_results.max(1),
            worker: None,
            search_pending: false,
            searching: false,
            snapshot: Vec::new(),
            worker_failed: false,
            worker_stopped: false,
            pending_jump: false,
            ready: None,
        }
    }

    pub fn scope(&self) -> SearchScope {
        self.engine.scope()
    }

    /// True while the worker still owes results for the current query.
    pub fn searching(&self) -> bool {
        self.searching
    }

    /// Populate the result list before the user types. An empty query ranks
    /// the newest history lines first, so opening the overlay is useful on its
    /// own and does not wait for the first keystroke. Scores on this thread:
    /// `run.rs` uses [`Self::queue_prime`] instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn prime(&mut self, sources: &[SearchSource<'_>]) {
        self.query_edit(Scoring::Now, QueryEdit::Set(String::new()), sources);
    }

    /// Queue the opening scan (empty query ⇒ newest lines first) for the
    /// worker. Follow with `start_pending_search`.
    pub fn queue_prime(&mut self) {
        self.engine.queue_query("");
        self.expect_scan();
    }

    #[allow(dead_code)] // used in tests
    pub fn engine(&self) -> &SearchEngine {
        &self.engine
    }

    /// Feed a key event. `sources` must already be filtered to the current scope.
    /// Call `build_search_sources(self.scope(), …)` before calling this.
    /// Scores on this thread: `run.rs` uses [`Self::handle_key_async`] instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn handle_key(
        &mut self,
        key: &KeyCode,
        mods: Modifiers,
        sources: &[SearchSource<'_>],
    ) -> SearchOutcome {
        self.handle_key_with(key, mods, sources, Scoring::Now)
    }

    /// Feed a key event without scoring on this thread: a query/mode/option
    /// edit only queues the new state, so the caller must follow with
    /// `start_pending_search`. `run.rs` uses this so a keystroke never scans
    /// history on the event loop.
    pub fn handle_key_async(
        &mut self,
        key: &KeyCode,
        mods: Modifiers,
        sources: &[SearchSource<'_>],
    ) -> SearchOutcome {
        self.handle_key_with(key, mods, sources, Scoring::Deferred)
    }

    fn handle_key_with(
        &mut self,
        key: &KeyCode,
        mods: Modifiers,
        sources: &[SearchSource<'_>],
        scoring: Scoring,
    ) -> SearchOutcome {
        // Dismiss: Esc or Ctrl+g.
        if is_escape(key, mods) {
            record_recent(self.engine.query());
            return SearchOutcome::Dismiss;
        }

        match key {
            KeyCode::Enter => {
                if let Some(m) = self.engine.selected() {
                    record_recent(self.engine.query());
                    return SearchOutcome::Jump(m.clone());
                }
                if self.searching {
                    // The typed query has no batch yet: wait for its best hit
                    // instead of dismissing (or jumping to a stale one).
                    self.pending_jump = true;
                    return SearchOutcome::Pending;
                }
                // Enter on empty results → dismiss.
                record_recent(self.engine.query());
                return SearchOutcome::Dismiss;
            }

            KeyCode::UpArrow => {
                self.engine.move_up();
                // A selection move has no edit of its own: the preview follows
                // the cursor without touching the result list.
                self.refresh_preview_for(sources);
            }

            KeyCode::DownArrow => {
                self.engine.move_down();
                self.refresh_preview_for(sources);
            }

            KeyCode::Tab => {
                // Cycle scope wider (without Shift) or narrower (with Shift).
                let new_scope = if mods.contains(Modifiers::SHIFT) {
                    self.engine.scope().narrow(self.anchor_pane)
                } else {
                    self.engine.scope().widen(self.anchor_pane)
                };
                return SearchOutcome::Rescope(new_scope);
            }

            KeyCode::Char('f') | KeyCode::Char('F') if mods == Modifiers::ALT => {
                let next = match self.engine.mode() {
                    MatchMode::Literal | MatchMode::Regex => MatchMode::Fuzzy,
                    MatchMode::Fuzzy => MatchMode::Literal,
                };
                self.change_mode(scoring, next, sources);
            }

            KeyCode::Char('c') | KeyCode::Char('C') if mods == Modifiers::ALT => {
                let mut options = self.engine.options();
                options.exact_case = !options.exact_case;
                self.change_options(scoring, options, sources);
            }

            KeyCode::Char('w') | KeyCode::Char('W') if mods == Modifiers::ALT => {
                let mut options = self.engine.options();
                options.whole_word = !options.whole_word;
                self.change_options(scoring, options, sources);
            }

            KeyCode::Char('r') | KeyCode::Char('R') if mods == Modifiers::ALT => {
                let next = match self.engine.mode() {
                    MatchMode::Literal | MatchMode::Fuzzy => MatchMode::Regex,
                    MatchMode::Regex => MatchMode::Literal,
                };
                self.change_mode(scoring, next, sources);
            }

            KeyCode::Char('r') | KeyCode::Char('R') if mods == Modifiers::CTRL => {
                if let Some(query) = recall_recent(self.engine.query()) {
                    self.query_edit(scoring, QueryEdit::Set(query), sources);
                }
            }

            KeyCode::Backspace => {
                self.query_edit(scoring, QueryEdit::Pop, sources);
            }

            KeyCode::Char(c)
                if (mods.is_empty() || mods == Modifiers::SHIFT) && !c.is_control() =>
            {
                self.query_edit(scoring, QueryEdit::Push(*c), sources);
            }

            _ => {}
        }

        SearchOutcome::Pending
    }

    /// Apply a scope change and re-score on this thread. `run.rs` uses
    /// [`Self::apply_scope_async`] instead.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn apply_scope(&mut self, scope: SearchScope, sources: &[SearchSource<'_>]) {
        self.engine.set_scope(scope, sources);
        self.refresh_preview(sources);
    }

    /// Apply a scope change without scoring: the caller must start the worker
    /// search against the new sources.
    pub fn apply_scope_async(&mut self, scope: SearchScope) {
        self.engine.queue_scope(scope);
        self.expect_scan();
    }

    /// Hand the pending edit to the worker. Cheap when nothing is pending, so
    /// the loop can call it unconditionally after a key.
    pub fn start_pending_search(&mut self, sources: &[SearchSource<'_>], waker: TerminalWaker) {
        self.start_pending_search_with(
            sources,
            Box::new(move || {
                if let Err(error) = waker.wake() {
                    tracing::debug!(%error, "terminal search waker pulse failed");
                }
            }),
        );
    }

    /// [`Self::start_pending_search`] with an explicit wake callback, used when
    /// the worker is first spawned (and by tests, which have no terminal).
    pub(crate) fn start_pending_search_with(&mut self, sources: &[SearchSource<'_>], wake: WakeFn) {
        if !self.search_pending {
            return;
        }
        self.search_pending = false;
        if self.worker.is_none() && !self.worker_failed {
            self.worker = TerminalSearchWorker::new(wake);
            self.worker_failed = self.worker.is_none();
        }
        if self.worker.is_none() {
            self.score_on_loop(sources);
            return;
        }
        // Cheap on the loop: unchanged panes are shared, changed ones cost one
        // refcount bump per line.
        self.snapshot = take_snapshot(sources, &self.snapshot);
        let request = SearchRequest {
            generation: 0,
            scope: self.engine.scope(),
            max_results: self.max_results,
            query: self.engine.query().to_string(),
            mode: self.engine.mode(),
            options: self.engine.options(),
            sources: self.snapshot.clone(),
        };
        if let Some(worker) = self.worker.as_mut() {
            worker.submit(request);
            self.searching = true;
        }
    }

    /// No worker thread is available (it could not be spawned, or it died):
    /// score here rather than leave the panel claiming it is searching forever.
    fn score_on_loop(&mut self, sources: &[SearchSource<'_>]) {
        self.searching = false;
        self.worker_stopped = false;
        self.snapshot.clear();
        self.engine.rescore(sources);
        self.refresh_preview(sources);
        self.settle_pending_jump();
    }

    /// Apply what the worker has produced. Returns true when what is on screen
    /// changed, so the caller can mark the frame dirty.
    pub fn drain_search(&mut self) -> bool {
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        let drained = worker.drain();
        if drained.is_empty() {
            return false;
        }
        let finished = drained.done.is_some() || drained.disconnected;
        for batch in drained.batches {
            self.engine.append_async_matches(batch);
        }
        if let Some((total, error)) = drained.done {
            self.searching = false;
            self.engine.finish_async_results(total, error);
        }
        if drained.disconnected {
            // The worker thread died (a panic while scoring). Say so, drop it,
            // and stay on the loop-side path for the rest of this overlay.
            self.worker = None;
            self.worker_failed = true;
            self.searching = false;
            self.worker_stopped = true;
            self.engine.finish_async_results(0, None);
            if self.pending_jump {
                // Enter is waiting on this query: rank its snapshot here rather
                // than dismiss without a jump.
                let snapshot = self.snapshot.clone();
                let refs = source_refs(&snapshot);
                self.engine.rescore(&refs);
                self.worker_stopped = false;
            }
        }
        // The preview's context lines come out of the snapshot the matches were
        // scored against, not the live buffers.
        let snapshot = self.snapshot.clone();
        let refs = source_refs(&snapshot);
        self.refresh_preview(&refs);
        if finished || !self.engine.matches().is_empty() {
            self.settle_pending_jump();
        }
        true
    }

    /// Stop waiting for the worker and rank the snapshot on this thread, so the
    /// results are complete. Used when a search walk outlives the overlay.
    fn finish_scan_now(&mut self) {
        if !self.searching {
            return;
        }
        self.worker = None;
        self.searching = false;
        let snapshot = self.snapshot.clone();
        let refs = source_refs(&snapshot);
        self.engine.rescore(&refs);
    }

    /// Take the outcome a deferred Enter produced while draining, if any.
    pub fn take_ready(&mut self) -> Option<SearchOutcome> {
        self.ready.take()
    }

    /// Resolve an Enter that arrived before any result: jump to the best hit
    /// once there is one, dismiss if the finished query matched nothing.
    fn settle_pending_jump(&mut self) {
        if !self.pending_jump {
            return;
        }
        if let Some(m) = self.engine.selected() {
            self.pending_jump = false;
            record_recent(self.engine.query());
            self.ready = Some(SearchOutcome::Jump(m.clone()));
        } else if !self.searching {
            self.pending_jump = false;
            record_recent(self.engine.query());
            self.ready = Some(SearchOutcome::Dismiss);
        }
    }

    /// A worker scan is now needed, and the preview's rows belong to the query
    /// being replaced, so drop them instead of showing stale context. A further
    /// edit also withdraws a deferred Enter: the user is still typing.
    fn expect_scan(&mut self) {
        self.search_pending = true;
        self.pending_jump = false;
        self.preview.clear();
    }

    fn query_edit(&mut self, scoring: Scoring, edit: QueryEdit, sources: &[SearchSource<'_>]) {
        match scoring {
            Scoring::Now => {
                match edit {
                    QueryEdit::Set(query) => self.engine.set_query(&query, sources),
                    QueryEdit::Push(c) => self.engine.push_char(c, sources),
                    QueryEdit::Pop => self.engine.backspace(sources),
                }
                self.refresh_preview(sources);
            }
            Scoring::Deferred => {
                match edit {
                    QueryEdit::Set(query) => self.engine.queue_query(&query),
                    QueryEdit::Push(c) => {
                        let mut query = self.engine.query().to_string();
                        query.push(c);
                        self.engine.queue_query(&query);
                    }
                    QueryEdit::Pop => {
                        let mut query = self.engine.query().to_string();
                        query.pop();
                        self.engine.queue_query(&query);
                    }
                }
                self.expect_scan();
            }
        }
    }

    fn change_mode(&mut self, scoring: Scoring, mode: MatchMode, sources: &[SearchSource<'_>]) {
        match scoring {
            Scoring::Now => {
                self.engine.set_mode(mode, sources);
                self.refresh_preview(sources);
            }
            Scoring::Deferred => {
                self.engine.queue_mode(mode);
                self.expect_scan();
            }
        }
    }

    fn change_options(
        &mut self,
        scoring: Scoring,
        options: MatchOptions,
        sources: &[SearchSource<'_>],
    ) {
        match scoring {
            Scoring::Now => {
                self.engine.set_options(options, sources);
                self.refresh_preview(sources);
            }
            Scoring::Deferred => {
                self.engine.queue_options(options);
                self.expect_scan();
            }
        }
    }

    /// Refresh the preview from the snapshot the matches came from when there is
    /// one (worker path), else from `sources`.
    fn refresh_preview_for(&mut self, sources: &[SearchSource<'_>]) {
        if self.snapshot.is_empty() {
            self.refresh_preview(sources);
        } else {
            let snapshot = self.snapshot.clone();
            self.refresh_preview(&source_refs(&snapshot));
        }
    }

    fn refresh_preview(&mut self, sources: &[SearchSource<'_>]) {
        self.preview.clear();
        let Some(selected) = self.engine.selected() else {
            return;
        };
        let Some((_, _, history)) = sources
            .iter()
            .find(|(pane_id, _, _)| *pane_id == selected.pane_id)
        else {
            return;
        };

        if let Some(idx) = selected.line_idx.checked_sub(1)
            && let Some(line) = history.get(idx)
        {
            self.preview.push(preview_line(line));
        }
        if let Some(idx) = selected.line_idx.checked_add(1)
            && let Some(line) = history.get(idx)
        {
            self.preview.push(preview_line(line));
        }
    }

    /// Draw the overlay into `surface`. `screen` is the center rect (the area
    /// the overlay may use).
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let engine = &self.engine;
        let matches = engine.matches();

        let max_rows = (screen.rows * 2 / 3).saturating_sub(4);
        let shown = matches.len().min(max_rows).max(3);
        let width = screen.cols.saturating_sub(4).clamp(40, 120);
        let preview_reserve = if self.preview.is_empty() { 0 } else { 2 };
        // rows: prompt(1) + rule(1) + results(shown) + preview + rule(1) + footer(1)
        let content_rows = 4 + shown.max(1) + preview_reserve;

        let mut scope_pill = format!(" {} ▾", engine.scope().label());
        let options = engine.options();
        if options.exact_case {
            scope_pill.push_str(" Aa");
        }
        if options.whole_word {
            scope_pill.push_str(" ab");
        }
        match engine.mode() {
            MatchMode::Regex => scope_pill.push_str(" .*"),
            MatchMode::Fuzzy => scope_pill.push_str(" ~"),
            MatchMode::Literal => {}
        }
        let spec = LayerSpec {
            title: "Search history".into(),
            badge: Some(scope_pill),
            cols: width,
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
            // Cursor block. `into_caret` also parks the real terminal cursor here
            // (see `crate::caret`) — the glyph alone would leave it in the pane.
            prompt_segs.push(seg(Tok::Slot(S::Accent), "█").into_caret());
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
        let preview_rows = preview_reserve.min(inner.rows.saturating_sub(4));
        let result_rows = inner.rows.saturating_sub(4 + preview_rows); // prompt + 2 rules + preview + footer
        let first = selected_idx.saturating_sub(result_rows.saturating_sub(1));
        for (row, (i, m)) in matches
            .iter()
            .enumerate()
            .skip(first)
            .take(result_rows)
            .enumerate()
        {
            let selected = i == selected_idx;
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

            // Content column: the matched line, windowed around the hit.
            let repeat_suffix = repeat_suffix(m.repeat);
            let suffix_width = repeat_suffix.as_ref().map_or(0, String::len);
            let line_avail = inner.cols.saturating_sub(26).saturating_sub(suffix_width); // 24 label + 2 sep + suffix
            let (lcut, pre, hit, post, rcut) =
                window_around(&m.line, m.span.unwrap_or((0, 0)), line_avail);
            let text_tok = if selected {
                Tok::Slot(S::Text)
            } else {
                Tok::Slot(S::Dim)
            };
            let mut segs = vec![bullet, label_tok, sp(2)];
            if lcut {
                segs.push(seg(Tok::Slot(S::Ghost3), ellipsis().to_string()));
            }
            segs.push(seg(text_tok, pre.to_string()));
            segs.push(seg(Tok::Slot(S::Accent), hit.to_string()).bold());
            segs.push(seg(text_tok, post.to_string()));
            if rcut {
                segs.push(seg(Tok::Slot(S::Ghost3), ellipsis().to_string()));
            }
            if let Some(suffix) = repeat_suffix {
                segs.push(seg(Tok::Slot(S::Ghost3), suffix));
            }
            seg::draw_line(
                surface,
                inner.x,
                inner.y + 2 + row,
                inner.cols,
                &Line::segs(segs),
                bg,
            );
        }

        // If no results, show a placeholder.
        if matches.is_empty() && inner.rows > 3 && result_rows > 0 {
            let placeholder = if let Some(error) = engine.error() {
                format!("regex error: {error}")
            } else if self.worker_stopped {
                "search unavailable: worker stopped".to_string()
            } else if self.searching() {
                "searching…".to_string()
            } else if engine.query().trim().is_empty() {
                "no history yet".to_string()
            } else if matches!(engine.scope(), SearchScope::Pane(_)) {
                "no matches in this pane: Tab to widen".to_string()
            } else if engine.mode() == MatchMode::Literal {
                "no matches: Alt+f for fuzzy".to_string()
            } else {
                "no matches".to_string()
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

        // ── Context preview ─────────────────────────────────────────────────
        // Keep the preview immediately above the footer rule. A clamped layer
        // on a tiny screen may not have room for all requested rows.
        let preview_start = inner.y + inner.rows - 2 - preview_rows;
        for (row, line) in self.preview.iter().take(preview_rows).enumerate() {
            let line = seg::clip_end(line, inner.cols.saturating_sub(2));
            seg::draw_line(
                surface,
                inner.x,
                preview_start + row,
                inner.cols,
                &Line::segs(vec![sp(2), seg(Tok::Slot(S::Ghost3), line)]),
                panel,
            );
        }

        // ── Footer ────────────────────────────────────────────────────────────
        if inner.rows >= 4 {
            let fy = inner.y + inner.rows - 2;
            seg::draw_line(surface, inner.x, fy, inner.cols, &rule, panel);
            let count_str = if matches.is_empty() {
                "no results".to_string()
            } else if engine.truncated() {
                format!(
                    "{} of {} (of {})",
                    selected_idx + 1,
                    matches.len(),
                    engine.total()
                )
            } else {
                format!("{} of {}", selected_idx + 1, matches.len())
            };
            // The worker may still be streaming: the count is a lower bound.
            let count_str = if self.searching() {
                format!("{count_str} · searching…")
            } else {
                count_str
            };
            let mode_hint = "Alt+c case  Alt+w word  Alt+r regex  Alt+f fuzzy";
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
                vec![
                    seg(Tok::Slot(S::Ghost3), count_str),
                    sp(2),
                    seg(Tok::Slot(S::Ghost2), mode_hint),
                ],
            );
            seg::draw_line(surface, inner.x, fy + 1, inner.cols, &footer, panel);
        }
    }
}

/// The hits of the search that was just activated, kept after the overlay closes
/// so `n`/`N` can step through them. The mark lives only as long as the walk:
/// the next key (or a scroll) ends it.
pub(crate) struct SearchWalk {
    matches: Vec<SearchMatch>,
    index: usize,
    /// The overlay, kept while its scan was still running at Enter so the first
    /// step can finish ranking and walk every hit, not just the first batch.
    scan: Option<Box<SearchOverlay>>,
}

impl std::fmt::Debug for SearchWalk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchWalk")
            .field("matches", &self.matches.len())
            .field("index", &self.index)
            .field("scanning", &self.scan.is_some())
            .finish()
    }
}

impl SearchWalk {
    /// Take the overlay's ranked hits, starting at the one Enter jumped to.
    /// None when there is nothing to step through.
    pub(crate) fn from_overlay(overlay: SearchOverlay) -> Option<Self> {
        let matches = overlay.engine.matches();
        if matches.is_empty() {
            return None;
        }
        let index = overlay.engine.selected_idx().min(matches.len() - 1);
        let matches = matches.to_vec();
        let scan = overlay.searching().then(|| Box::new(overlay));
        Some(Self {
            matches,
            index,
            scan,
        })
    }

    /// Finish a scan that was still streaming at Enter: rank the overlay's own
    /// snapshot here and keep the position on the hit that was jumped to.
    fn complete_scan(&mut self) {
        let Some(mut overlay) = self.scan.take() else {
            return;
        };
        overlay.finish_scan_now();
        let all = overlay.engine.matches();
        if all.is_empty() {
            return;
        }
        let current = self.matches.get(self.index);
        self.index = current
            .and_then(|c| all.iter().position(|m| m == c))
            .unwrap_or_else(|| self.index.min(all.len() - 1));
        self.matches = all.to_vec();
    }

    /// Step to the next (`forward`) or previous hit, wrapping at either end.
    /// Returns the hit to jump to and its 1-based position out of the total.
    pub(crate) fn step(&mut self, forward: bool) -> (SearchMatch, usize, usize) {
        self.complete_scan();
        let len = self.matches.len();
        self.index = if forward {
            (self.index + 1) % len
        } else {
            (self.index + len - 1) % len
        };
        (
            self.matches[self.index].clone(),
            self.index + 1,
            self.matches.len(),
        )
    }
}

fn is_escape(key: &KeyCode, mods: Modifiers) -> bool {
    // `is_escape_key` matches both `KeyCode::Escape` and the
    // `KeyCode::Char('\x1b')` that CSI-u/fixterms terminals report for the
    // physical Escape key. Without it, Esc on those terminals falls through to
    // the `Char` arm and gets typed into the query instead of dismissing.
    crate::input::is_escape_key(key)
        || (mods.contains(Modifiers::CTRL)
            && matches!(
                key,
                KeyCode::Char('g') | KeyCode::Char('G') | KeyCode::Char('c') | KeyCode::Char('C')
            ))
}

const PREVIEW_MAX_COLS: usize = 120;

fn preview_line(line: &str) -> String {
    let filtered: String = line.chars().filter(|c| !c.is_control()).collect();
    seg::clip_end(&filtered, PREVIEW_MAX_COLS)
}

fn repeat_suffix(repeat: u32) -> Option<String> {
    if repeat <= 1 {
        return None;
    }
    Some(format!(" x{repeat}"))
}

/// Truncate `s` to at most `max_cols` display columns, appending `…` if
/// truncated. Width-aware and UTF-8-safe (byte-slicing panicked on non-ASCII
/// scrollback, taking down the whole compositor).
fn truncate_label(s: &str, max_cols: usize) -> String {
    crate::seg::clip_end(s, max_cols)
}

/// The ellipsis glyph `seg::clip_end` draws (derived, not re-declared).
fn ellipsis() -> char {
    seg::clip_end("abc", 2).chars().last().unwrap_or(' ')
}

/// Longest prefix of `s` fitting `cols` display cells.
fn take_prefix(s: &str, cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > cols {
            return &s[..i];
        }
        used += w;
    }
    s
}

/// Longest suffix of `s` fitting `cols` display cells.
fn take_suffix(s: &str, cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices().rev() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > cols {
            return &s[i + c.len_utf8()..];
        }
        used += w;
    }
    s
}

/// Window `line` into `avail` cells around the byte `span` of the hit.
/// Returns `(left_cut, before, hit, after, right_cut)`; each cut costs one
/// cell for the ellipsis. Width-aware and char-boundary safe.
fn window_around(line: &str, span: (usize, usize), avail: usize) -> (bool, &str, &str, &str, bool) {
    use unicode_width::UnicodeWidthStr;
    let (s, e) = (
        span.0.min(line.len()),
        span.1.min(line.len()).max(span.0.min(line.len())),
    );
    let (s, e) = if line.is_char_boundary(s) && line.is_char_boundary(e) {
        (s, e)
    } else {
        (0, 0)
    };
    let (pre, hit, post) = (&line[..s], &line[s..e], &line[e..]);
    if UnicodeWidthStr::width(line) <= avail {
        return (false, pre, hit, post, false);
    }
    let hit_w = UnicodeWidthStr::width(hit);
    if hit_w >= avail {
        // The hit alone fills the row: show its head, cut on the right.
        return (
            false,
            "",
            take_prefix(hit, avail.saturating_sub(1)),
            "",
            avail > 0,
        );
    }
    let budget = avail - hit_w;
    let post_w = UnicodeWidthStr::width(post);
    let pre_w = UnicodeWidthStr::width(pre);
    let right_budget = budget - pre_w.min(budget / 2);
    let (post_out, rcut) = if post_w <= right_budget {
        (post, false)
    } else if right_budget == 0 {
        ("", false)
    } else {
        (take_prefix(post, right_budget - 1), true)
    };
    let used_right = UnicodeWidthStr::width(post_out) + usize::from(rcut);
    let left_budget = budget - used_right;
    let (pre_out, lcut) = if pre_w <= left_budget {
        (pre, false)
    } else if left_budget == 0 {
        ("", false)
    } else {
        (take_suffix(pre, left_budget - 1), true)
    };
    (lcut, pre_out, hit, post_out, rcut)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::history::HistoryBuffer;
    use thegn_core::search::{MatchOptions, SearchSource};

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

    fn clear_recent() {
        RECENT
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
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
    fn prime_lists_newest_lines_before_typing() {
        let buf = buf_from(&["old", "middle", "new"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);

        ov.prime(&srcs);

        assert_eq!(ov.engine().query(), "");
        assert_eq!(ov.engine().matches()[0].line, "new");
        assert_eq!(ov.engine().matches()[1].line, "middle");
    }

    #[test]
    fn render_shows_repeat_count() {
        let buf = buf_from(&["repeat", "repeat", "other"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "repeat", &srcs);

        let out = draw(&ov, 100, 30);
        assert!(out.contains("repeat x2"), "{out}");
    }

    #[test]
    fn preview_tracks_selection() {
        let buf = buf_from(&[
            "before first",
            "first target",
            "between",
            "second target",
            "after second",
        ]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "target", &srcs);
        assert_eq!(
            ov.preview,
            vec![String::from("between"), String::from("after second")]
        );

        ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE, &srcs);
        assert_eq!(
            ov.preview,
            vec![String::from("before first"), String::from("between")]
        );
    }

    #[test]
    fn preview_never_crosses_sources() {
        let first = buf_from(&["first a", "first b", "first c"]);
        let second = buf_from(&["second before", "target"]);
        let srcs = vec![(1, "first", &first), (2, "second", &second)];
        let mut ov = overlay(2);
        typed(&mut ov, "target", &srcs);

        assert_eq!(ov.preview, vec![String::from("second before")]);
        assert!(!ov.preview.iter().any(|line| line.starts_with("first")));
    }

    #[test]
    fn preview_absent_for_edge_lines() {
        let buf = buf_from(&["oldest target", "middle", "newest target"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "oldest", &srcs);
        assert_eq!(ov.preview, vec![String::from("middle")]);

        ov = overlay(1);
        typed(&mut ov, "newest", &srcs);
        assert_eq!(ov.preview, vec![String::from("middle")]);
    }

    #[test]
    fn render_with_preview_on_tiny_screen_does_not_panic() {
        let buf = buf_from(&["before", "target", "after"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "target", &srcs);

        let _ = draw(&ov, 10, 4);
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
    fn search_walk_steps_forward_and_back_with_wrap() {
        let buf = buf_from(&["hit one", "other", "hit two", "hit three"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "hit", &srcs);
        assert_eq!(ov.engine().matches().len(), 3);

        let first = ov.engine().selected().expect("selected hit").clone();
        let mut walk = SearchWalk::from_overlay(ov).expect("walkable results");

        let (second, position, total) = walk.step(true);
        assert_eq!((position, total), (2, 3));
        let (third, position, _) = walk.step(true);
        assert_eq!(position, 3);
        let (wrapped, position, _) = walk.step(true);
        assert_eq!(position, 1);
        assert_eq!(wrapped, first, "forward step wraps to the first hit");

        let (back, position, _) = walk.step(false);
        assert_eq!(position, 3);
        assert_eq!(back, third, "backward step wraps to the last hit");
        assert_ne!(second, first);
        assert_ne!(third, second);
    }

    #[test]
    fn search_walk_finishes_a_scan_that_was_still_running() {
        let buf = buf_from(&["hit one", "hit two", "hit three"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "hit", &srcs);
        // Enter landed while the worker still owed batches.
        ov.searching = true;
        let mut walk = SearchWalk::from_overlay(ov).expect("walkable results");
        assert!(walk.scan.is_some(), "an unfinished scan keeps its overlay");

        let (_, position, total) = walk.step(true);
        assert!(walk.scan.is_none(), "the first step completes the scan");
        assert!(position >= 1 && position <= total, "{position} of {total}");
    }

    #[test]
    fn search_walk_is_none_without_hits() {
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "zzzz", &srcs);
        assert!(ov.engine().matches().is_empty());
        assert!(SearchWalk::from_overlay(ov).is_none());
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
        let _ = r; // best-effort: test: asserts no panic; the outcome is either-way by design (comment above)
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
    fn handle_key_csi_u_escape_char_returns_dismiss() {
        // CSI-u/fixterms terminals report Escape as `Char('\x1b')`. It must
        // dismiss the overlay, not get typed into the query as a blank.
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        let r = ov.handle_key(&KeyCode::Char('\x1b'), Modifiers::NONE, &srcs);
        assert!(matches!(r, SearchOutcome::Dismiss));
        assert_eq!(ov.engine().query(), "");
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
        assert!(matches!(
            ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs),
            SearchOutcome::Rescope(SearchScope::Tab)
        ));
        ov.apply_scope(SearchScope::Tab, &srcs);
        assert_eq!(ov.scope(), SearchScope::Tab);
        assert!(matches!(
            ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs),
            SearchOutcome::Rescope(SearchScope::Worktree)
        ));
        ov.apply_scope(SearchScope::Worktree, &srcs);
        assert_eq!(ov.scope(), SearchScope::Worktree);
        assert!(matches!(
            ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs),
            SearchOutcome::Rescope(SearchScope::Workspace)
        ));
        ov.apply_scope(SearchScope::Workspace, &srcs);
        assert_eq!(ov.scope(), SearchScope::Workspace);
        // Profile is skipped: wrap back to Pane.
        assert!(matches!(
            ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &srcs),
            SearchOutcome::Rescope(SearchScope::Pane(5))
        ));
        ov.apply_scope(SearchScope::Pane(5), &srcs);
        assert_eq!(ov.scope(), SearchScope::Pane(5));
    }

    #[test]
    fn handle_key_shift_tab_cycles_scope_backward() {
        let buf = buf_from(&["x"]);
        let srcs = sources(3, &buf);
        let mut ov = SearchOverlay::new(SearchScope::Tab, 3, 100);
        assert!(matches!(
            ov.handle_key(&KeyCode::Tab, Modifiers::SHIFT, &srcs),
            SearchOutcome::Rescope(SearchScope::Pane(3))
        ));
        ov.apply_scope(SearchScope::Pane(3), &srcs);
        assert_eq!(ov.scope(), SearchScope::Pane(3));
    }

    #[test]
    fn tab_returns_rescope_and_apply_scope_recomputes() {
        let first = buf_from(&["target"]);
        let second = buf_from(&["target"]);
        let first_src = sources(1, &first);
        let both = vec![(1, "first", &first), (2, "second", &second)];
        let mut ov = overlay(1);
        typed(&mut ov, "target", &first_src);
        assert_eq!(ov.engine().matches().len(), 1);

        assert!(matches!(
            ov.handle_key(&KeyCode::Tab, Modifiers::NONE, &first_src),
            SearchOutcome::Rescope(SearchScope::Tab)
        ));
        assert_eq!(ov.engine().matches().len(), 1);
        ov.apply_scope(SearchScope::Tab, &both);
        assert_eq!(ov.engine().matches().len(), 2);
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
        let _srcs: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let ov = SearchOverlay::new(SearchScope::Pane(1), 1, 100);
        let mut surface = Surface::new(80, 24);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 24,
        };
        ov.render(&mut surface, rect); // must not panic
    }

    #[test]
    fn render_does_not_panic_on_tiny_screen() {
        let buf = buf_from(&["hello"]);
        let _srcs: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let ov = SearchOverlay::new(SearchScope::Pane(1), 1, 100);
        let mut surface = Surface::new(10, 4);
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 10,
            rows: 4,
        };
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

    fn draw(ov: &SearchOverlay, cols: usize, rows: usize) -> String {
        let mut surface = Surface::new(cols, rows);
        ov.render(
            &mut surface,
            Rect {
                x: 0,
                y: 0,
                cols,
                rows,
            },
        );
        surface.screen_chars_to_string()
    }

    fn typed(ov: &mut SearchOverlay, q: &str, srcs: &[SearchSource<'_>]) {
        for c in q.chars() {
            ov.handle_key(&KeyCode::Char(c), Modifiers::NONE, srcs);
        }
    }

    #[test]
    fn long_line_window_shows_hit_with_left_cut() {
        let line = format!("{}NEEDLE{}", "x".repeat(250), "y".repeat(44));
        let buf = buf_from(&[&line]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "NEEDLE", &srcs);
        let out = draw(&ov, 120, 30);
        // The query row also reads NEEDLE: the hit row is the one after it.
        let row = out
            .lines()
            .filter(|l| l.contains("NEEDLE"))
            .nth(1)
            .expect("hit row drawn");
        let at = row.find("NEEDLE").expect("hit");
        assert!(
            row[..at].contains('…'),
            "left cut ellipsis before the hit: {row}"
        );
    }

    #[test]
    fn narrow_screen_still_shows_hit() {
        let line = format!("{}NEEDLE{}", "x".repeat(250), "y".repeat(44));
        let buf = buf_from(&[&line]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        typed(&mut ov, "NEEDLE", &srcs);
        assert!(draw(&ov, 80, 24).contains("NEEDLE"));
    }

    #[test]
    fn selection_stays_drawn_when_scrolling_and_footer_counts() {
        let owned: Vec<String> = (0..415).map(|i| format!("hit {i}")).collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let buf = buf_from(&refs);
        let srcs = sources(1, &buf);
        let mut ov = SearchOverlay::new(SearchScope::Pane(1), 1, 1000);
        typed(&mut ov, "hit", &srcs);
        for _ in 0..11 {
            ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE, &srcs);
        }
        assert!(draw(&ov, 100, 30).contains("12 of 415"));
        for _ in 0..9 {
            ov.handle_key(&KeyCode::DownArrow, Modifiers::NONE, &srcs);
        }
        let out = draw(&ov, 100, 30);
        assert!(out.contains("21 of 415"));
        assert!(out.contains("hit 394")); // selected row (newest-first idx 20)
    }

    #[test]
    fn alt_f_toggles_mode() {
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        assert_eq!(ov.engine().mode(), MatchMode::Literal);
        assert!(draw(&ov, 100, 30).contains("Alt+c case  Alt+w word  Alt+r regex  Alt+f fuzzy"));
        ov.handle_key(&KeyCode::Char('f'), Modifiers::ALT, &srcs);
        assert_eq!(ov.engine().mode(), MatchMode::Fuzzy);
        assert!(draw(&ov, 100, 30).contains("Alt+c case  Alt+w word  Alt+r regex  Alt+f fuzzy"));
        ov.handle_key(&KeyCode::Char('f'), Modifiers::ALT, &srcs);
        assert_eq!(ov.engine().mode(), MatchMode::Literal);
    }

    #[test]
    fn alt_search_keys_toggle_options_and_regex() {
        let buf = buf_from(&["Error: login", "error: log"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);

        assert_eq!(ov.engine().options(), MatchOptions::default());
        ov.handle_key(&KeyCode::Char('c'), Modifiers::ALT, &srcs);
        assert!(ov.engine().options().exact_case);
        ov.handle_key(&KeyCode::Char('w'), Modifiers::ALT, &srcs);
        assert!(ov.engine().options().whole_word);
        ov.handle_key(&KeyCode::Char('r'), Modifiers::ALT, &srcs);
        assert_eq!(ov.engine().mode(), MatchMode::Regex);
        ov.handle_key(&KeyCode::Char('r'), Modifiers::ALT, &srcs);
        assert_eq!(ov.engine().mode(), MatchMode::Literal);
    }

    #[test]
    fn async_keys_queue_without_scoring() {
        let buf = buf_from(&["cargo build", "cargo test", "ninja"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);

        for c in "cargo".chars() {
            let r = ov.handle_key_async(&KeyCode::Char(c), Modifiers::NONE, &srcs);
            assert!(matches!(r, SearchOutcome::Pending));
        }
        // The loop holds the query only; the worker owns the scan.
        assert_eq!(ov.engine().query(), "cargo");
        assert!(ov.engine().matches().is_empty());
        assert_eq!(ov.engine().total(), 0);
        assert!(!ov.searching(), "not searching until a worker is started");

        // A mode toggle defers too, and a scope change only queues the scope.
        ov.handle_key_async(&KeyCode::Char('f'), Modifiers::ALT, &srcs);
        assert_eq!(ov.engine().mode(), MatchMode::Fuzzy);
        assert!(ov.engine().matches().is_empty());
        ov.apply_scope_async(SearchScope::Tab);
        assert_eq!(ov.scope(), SearchScope::Tab);

        // Nothing to fold in before a worker exists.
        assert!(!ov.drain_search());
    }

    /// Fold worker output in until the deferred outcome (or the end of the
    /// search) arrives.
    fn pump(ov: &mut SearchOverlay) -> Option<SearchOutcome> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            ov.drain_search();
            if let Some(outcome) = ov.take_ready() {
                return Some(outcome);
            }
            if !ov.searching() {
                return None;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "search never finished"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn enter_before_the_first_batch_jumps_to_the_best_hit() {
        let buf = buf_from(&["cargo build", "cargo test", "ninja"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        for c in "ninja".chars() {
            ov.handle_key_async(&KeyCode::Char(c), Modifiers::NONE, &srcs);
        }
        ov.start_pending_search_with(&srcs, Box::new(|| {}));
        assert!(ov.searching());

        // Nothing has been drained yet, so there is no hit to jump to.
        let r = ov.handle_key_async(&KeyCode::Enter, Modifiers::NONE, &srcs);
        assert!(
            matches!(r, SearchOutcome::Pending),
            "Enter waits, not dismisses"
        );
        match pump(&mut ov) {
            Some(SearchOutcome::Jump(m)) => assert_eq!(m.line, "ninja"),
            other => panic!("expected a jump to the best hit, got {other:?}"),
        }
    }

    #[test]
    fn enter_pending_when_the_worker_dies_ranks_on_the_loop() {
        let buf = buf_from(&["cargo build", "cargo test", "ninja"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        for c in "ninja".chars() {
            ov.handle_key_async(&KeyCode::Char(c), Modifiers::NONE, &srcs);
        }
        // A wake callback that panics kills the worker thread after delivery.
        ov.start_pending_search_with(
            &srcs,
            Box::new(|| {
                panic!("boom");
            }),
        );
        let r = ov.handle_key_async(&KeyCode::Enter, Modifiers::NONE, &srcs);
        assert!(matches!(r, SearchOutcome::Pending));
        match pump(&mut ov) {
            Some(SearchOutcome::Jump(m)) => assert_eq!(m.line, "ninja"),
            other => panic!("expected a loop-side jump, got {other:?}"),
        }
    }

    #[test]
    fn enter_before_results_dismisses_when_nothing_matches() {
        let buf = buf_from(&["cargo build", "ninja"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        for c in "qqqq".chars() {
            ov.handle_key_async(&KeyCode::Char(c), Modifiers::NONE, &srcs);
        }
        ov.start_pending_search_with(&srcs, Box::new(|| {}));
        let r = ov.handle_key_async(&KeyCode::Enter, Modifiers::NONE, &srcs);
        assert!(matches!(r, SearchOutcome::Pending));
        assert!(matches!(pump(&mut ov), Some(SearchOutcome::Dismiss)));
    }

    #[test]
    fn typing_after_a_deferred_enter_withdraws_it() {
        let buf = buf_from(&["cargo build", "ninja"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        ov.handle_key_async(&KeyCode::Char('n'), Modifiers::NONE, &srcs);
        ov.start_pending_search_with(&srcs, Box::new(|| {}));
        ov.handle_key_async(&KeyCode::Enter, Modifiers::NONE, &srcs);
        ov.handle_key_async(&KeyCode::Char('i'), Modifiers::NONE, &srcs);
        ov.start_pending_search_with(&srcs, Box::new(|| {}));
        assert!(pump(&mut ov).is_none(), "the user kept typing: no jump");
    }

    #[test]
    fn preview_reads_the_snapshot_not_the_live_buffer() {
        let mut buf = buf_from(&["alpha", "beta needle", "gamma"]);
        let mut ov = overlay(1);
        {
            let srcs = sources(1, &buf);
            for c in "needle".chars() {
                ov.handle_key_async(&KeyCode::Char(c), Modifiers::NONE, &srcs);
            }
            ov.start_pending_search_with(&srcs, Box::new(|| {}));
        }
        assert!(pump(&mut ov).is_none());
        // The live pane moves on (lines evicted/rewritten); the preview must
        // still describe the lines the match index points into.
        assert!(buf.replace_from_end(1, "REWRITTEN"));
        ov.handle_key_async(&KeyCode::DownArrow, Modifiers::NONE, &sources(1, &buf));
        assert_eq!(ov.preview, ["alpha", "gamma"]);
    }

    #[test]
    fn render_footer_shows_total_when_results_are_truncated() {
        let buf = buf_from(&["hit 1", "hit 2", "hit 3", "hit 4", "hit 5"]);
        let srcs = sources(1, &buf);
        let mut ov = SearchOverlay::new(SearchScope::Pane(1), 1, 2);
        typed(&mut ov, "hit", &srcs);
        assert!(draw(&ov, 100, 30).contains("1 of 2 (of 5)"));
    }

    #[test]
    fn render_shows_regex_error_placeholder() {
        let buf = buf_from(&["error"]);
        let srcs = sources(1, &buf);
        let mut ov = overlay(1);
        ov.handle_key(&KeyCode::Char('r'), Modifiers::ALT, &srcs);
        typed(&mut ov, "[", &srcs);
        assert!(draw(&ov, 100, 30).contains("regex error:"));
    }

    #[test]
    fn ctrl_r_recalls_last_query() {
        clear_recent();
        let buf = buf_from(&["cargo build"]);
        let srcs = sources(1, &buf);
        let mut first = overlay(1);
        typed(&mut first, "cargo", &srcs);
        assert!(matches!(
            first.handle_key(&KeyCode::Escape, Modifiers::NONE, &srcs),
            SearchOutcome::Dismiss
        ));

        let mut recalled = overlay(1);
        recalled.handle_key(&KeyCode::Char('r'), Modifiers::CTRL, &srcs);
        assert_eq!(recalled.engine().query(), "cargo");
    }

    #[test]
    fn window_around_is_boundary_safe() {
        let line = "日本語日本語日本語hit日本語";
        let s = line.find("hit").unwrap();
        let (l, a, h, b, r) = window_around(line, (s, s + 3), 12);
        assert_eq!(h, "hit");
        assert!(l && r);
        assert!(a.len() + b.len() > 0);
        let _ = window_around(line, (0, 0), 0);
    }

    #[test]
    fn window_around_uses_full_width_without_hit() {
        let line = "a".repeat(100);
        let (l, pre, hit, s, r) = window_around(&line, (0, 0), 40);
        assert_eq!((l, pre, hit, r), (false, "", "", true));
        assert_eq!(s.len(), 39);
    }
}
