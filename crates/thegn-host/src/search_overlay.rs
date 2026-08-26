//! The Search & Replace overlay surface (THE-5).
//!
//! A dedicated focusable layer (composited like the palette / search overlay,
//! not a panel section): two input fields (query + replacement), literal/regex +
//! case/word/structural/hidden/gitignore options, results streamed in from the
//! off-loop worker and grouped by file with per-match and per-file selection, a
//! before/after preview per visible match, and an apply step through the single
//! guarded write path.
//!
//! The surface holds its own streaming state — an unbounded results channel and
//! a shared generation token that the worker checks between files. Every query
//! or option edit bumps the generation (superseding the previous search) and
//! sets a `dirty` flag; the event loop reads [`take_search_request`] to spawn
//! the new worker, and [`drain`] applies only generation-matched batches. The
//! surface never does I/O itself: rendering + reducers are pure over its state,
//! which keeps the state machine unit-testable (`just quick thegn-host` + tests).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use thegn_core::search_replace::{
    ApplyReport, Edit, Match, SearchMode, SearchSpec, WalkFilter, render_after_line,
};

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::search_worker::SearchBatch;
use crate::seg::{self, Line, Tok, seg, sp};

/// Which input field has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Query,
    Replace,
}

/// One file's grouped matches.
struct FileGroup {
    path: String,
    matches: Vec<MatchRow>,
}

struct MatchRow {
    m: Match,
    /// Whether this match will be applied.
    selected: bool,
}

/// A row in the flattened, navigable result list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowRef {
    File(usize),
    Match(usize, usize),
}

/// What the event loop must do after handing a key to the overlay.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Nothing structural changed.
    None,
    /// A new search should be spawned (`take_search_request`).
    Search,
    /// Apply the accepted edits (`take_apply_request`).
    Apply,
    /// Open the selected match in the editor seam.
    OpenEditor,
    /// Close the overlay.
    Close,
}

/// The parameters the loop needs to spawn a search worker.
pub struct SearchRequest {
    pub spec: SearchSpec,
    pub filter: WalkFilter,
    pub sg: u64,
    pub current: Arc<AtomicU64>,
    pub max_results: usize,
    pub tx: UnboundedSender<SearchBatch>,
}

pub struct SearchReplaceOverlay {
    query: String,
    replace: String,
    focus: Field,
    mode: SearchMode,
    case_sensitive: bool,
    whole_word: bool,
    structural: bool,
    include_hidden: bool,
    respect_gitignore: bool,
    include_glob: String,
    exclude_glob: String,

    files: Vec<FileGroup>,
    index: HashMap<String, usize>,
    selected: usize,
    scroll: usize,
    total_matches: usize,

    search_gen: u64,
    current: Arc<AtomicU64>,
    searching: bool,
    truncated: bool,
    regex_error: Option<String>,
    dirty: bool,
    status: Option<String>,

    tx: UnboundedSender<SearchBatch>,
    rx: UnboundedReceiver<SearchBatch>,
    apply_tx: UnboundedSender<ApplyReport>,
    apply_rx: UnboundedReceiver<ApplyReport>,

    max_results: usize,
    structural_available: bool,
}

impl SearchReplaceOverlay {
    /// Open the surface, optionally seeded with a query (the palette handoff).
    pub fn new(
        seed_query: &str,
        respect_gitignore: bool,
        include_hidden: bool,
        max_results: usize,
        structural_available: bool,
    ) -> Self {
        let (tx, rx) = unbounded_channel();
        let (apply_tx, apply_rx) = unbounded_channel();
        let mut o = SearchReplaceOverlay {
            query: seed_query.to_string(),
            replace: String::new(),
            focus: Field::Query,
            mode: SearchMode::Literal,
            case_sensitive: false,
            whole_word: false,
            structural: false,
            include_hidden,
            respect_gitignore,
            include_glob: String::new(),
            exclude_glob: String::new(),
            files: Vec::new(),
            index: HashMap::new(),
            selected: 0,
            scroll: 0,
            total_matches: 0,
            search_gen: 0,
            current: Arc::new(AtomicU64::new(0)),
            searching: false,
            truncated: false,
            regex_error: None,
            dirty: false,
            status: None,
            tx,
            rx,
            apply_tx,
            apply_rx,
            max_results: max_results.max(1),
            structural_available,
        };
        if !o.query.is_empty() {
            o.bump_search();
        }
        o
    }

    // ── query / spec ────────────────────────────────────────────────────────

    /// Bump the generation, clear results, revalidate, mark dirty — called on
    /// every edit that changes *what* is searched (not the replacement).
    fn bump_search(&mut self) {
        self.search_gen += 1;
        self.current.store(self.search_gen, Ordering::Release);
        self.files.clear();
        self.index.clear();
        self.selected = 0;
        self.scroll = 0;
        self.total_matches = 0;
        self.truncated = false;
        self.status = None;
        self.validate();
        self.searching = !self.query.is_empty() && self.regex_error.is_none();
        self.dirty = self.searching;
    }

    /// Compile-check a regex query; set `regex_error` for an inline message.
    fn validate(&mut self) {
        self.regex_error = None;
        if self.query.is_empty() || self.structural {
            return;
        }
        if self.mode == SearchMode::Regex
            && let Err(e) = thegn_core::search_replace::Matcher::build(&self.spec_raw())
        {
            self.regex_error = Some(e);
        }
    }

    fn spec_raw(&self) -> SearchSpec {
        SearchSpec {
            query: self.query.clone(),
            mode: self.mode,
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
        }
    }

    /// The valid search spec, or `None` when empty / invalid regex / structural
    /// (structural search is not driven by this textual worker).
    pub fn spec(&self) -> Option<SearchSpec> {
        if self.query.is_empty() || self.regex_error.is_some() || self.structural {
            return None;
        }
        Some(self.spec_raw())
    }

    pub fn filter(&self) -> WalkFilter {
        WalkFilter {
            include_globs: split_globs(&self.include_glob),
            exclude_globs: split_globs(&self.exclude_glob),
            respect_gitignore: self.respect_gitignore,
            include_hidden: self.include_hidden,
        }
    }

    /// If a fresh search is pending, hand the loop everything it needs to spawn
    /// the worker (and clear the pending flag). Structural mode yields nothing
    /// here — it routes through the CLI/seam, not this textual worker.
    pub fn take_search_request(&mut self) -> Option<SearchRequest> {
        if !std::mem::take(&mut self.dirty) {
            return None;
        }
        let spec = self.spec()?;
        Some(SearchRequest {
            spec,
            filter: self.filter(),
            sg: self.search_gen,
            current: self.current.clone(),
            max_results: self.max_results,
            tx: self.tx.clone(),
        })
    }

    pub fn apply_sender(&self) -> UnboundedSender<ApplyReport> {
        self.apply_tx.clone()
    }

    // ── key dispatch ────────────────────────────────────────────────────────

    /// Translate a terminal key event into a reducer call. Keeps `run.rs` thin;
    /// the semantic reducers below stay unit-testable without termwiz.
    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> Outcome {
        let ctrl = mods.contains(Modifiers::CTRL);
        let alt = mods.contains(Modifiers::ALT);
        match key {
            KeyCode::Escape => Outcome::Close,
            KeyCode::Enter => Outcome::Apply,
            KeyCode::Tab => self.toggle_field(),
            KeyCode::UpArrow => self.move_up(),
            KeyCode::DownArrow => self.move_down(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Char('t') if ctrl => self.toggle_selected(),
            KeyCode::Char('o') if ctrl => Outcome::OpenEditor,
            KeyCode::Char('r') if alt => self.toggle_regex(),
            KeyCode::Char('c') if alt => self.toggle_case(),
            KeyCode::Char('w') if alt => self.toggle_word(),
            KeyCode::Char('s') if alt => self.toggle_structural(),
            KeyCode::Char('h') if alt => self.toggle_hidden(),
            KeyCode::Char('i') if alt => self.toggle_gitignore(),
            KeyCode::Char(c) if !ctrl && !alt && !c.is_control() => self.push_char(*c),
            _ => Outcome::None,
        }
    }

    // ── input reducers ──────────────────────────────────────────────────────

    pub fn push_char(&mut self, c: char) -> Outcome {
        match self.focus {
            Field::Query => {
                self.query.push(c);
                self.bump_search();
                Outcome::Search
            }
            Field::Replace => {
                self.replace.push(c);
                Outcome::None // preview-only; no re-search
            }
        }
    }

    pub fn backspace(&mut self) -> Outcome {
        match self.focus {
            Field::Query => {
                self.query.pop();
                self.bump_search();
                Outcome::Search
            }
            Field::Replace => {
                self.replace.pop();
                Outcome::None
            }
        }
    }

    /// Tab toggles between the query and replacement fields.
    pub fn toggle_field(&mut self) -> Outcome {
        self.focus = match self.focus {
            Field::Query => Field::Replace,
            Field::Replace => Field::Query,
        };
        Outcome::None
    }

    pub fn toggle_regex(&mut self) -> Outcome {
        self.mode = match self.mode {
            SearchMode::Literal => SearchMode::Regex,
            SearchMode::Regex => SearchMode::Literal,
        };
        self.bump_search();
        Outcome::Search
    }
    pub fn toggle_case(&mut self) -> Outcome {
        self.case_sensitive = !self.case_sensitive;
        self.bump_search();
        Outcome::Search
    }
    pub fn toggle_word(&mut self) -> Outcome {
        self.whole_word = !self.whole_word;
        self.bump_search();
        Outcome::Search
    }
    pub fn toggle_hidden(&mut self) -> Outcome {
        self.include_hidden = !self.include_hidden;
        self.bump_search();
        Outcome::Search
    }
    pub fn toggle_gitignore(&mut self) -> Outcome {
        self.respect_gitignore = !self.respect_gitignore;
        self.bump_search();
        Outcome::Search
    }
    /// Structural mode is available only when the ast-grep binary is present.
    pub fn toggle_structural(&mut self) -> Outcome {
        if !self.structural_available {
            self.status = Some("structural search needs `ast-grep` on PATH".into());
            return Outcome::None;
        }
        self.structural = !self.structural;
        self.bump_search();
        Outcome::None // structural runs via the seam/CLI, not the textual worker
    }

    pub fn move_up(&mut self) -> Outcome {
        self.selected = self.selected.saturating_sub(1);
        Outcome::None
    }
    pub fn move_down(&mut self) -> Outcome {
        let n = self.rows().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
        Outcome::None
    }

    /// Space toggles the selected match, or a whole file when a header is
    /// selected.
    pub fn toggle_selected(&mut self) -> Outcome {
        let rows = self.rows();
        let Some(&row) = rows.get(self.selected) else {
            return Outcome::None;
        };
        match row {
            RowRef::Match(fi, mi) => {
                if let Some(m) = self.files.get_mut(fi).and_then(|f| f.matches.get_mut(mi)) {
                    m.selected = !m.selected;
                }
            }
            RowRef::File(fi) => {
                if let Some(f) = self.files.get_mut(fi) {
                    // If any match is off, turn all on; else turn all off.
                    let any_off = f.matches.iter().any(|m| !m.selected);
                    for m in &mut f.matches {
                        m.selected = any_off;
                    }
                }
            }
        }
        Outcome::None
    }

    /// The (path, line) of the selected match, for the editor handoff.
    pub fn selected_location(&self) -> Option<(String, usize)> {
        let rows = self.rows();
        match rows.get(self.selected)? {
            RowRef::Match(fi, mi) => {
                let f = self.files.get(*fi)?;
                let m = f.matches.get(*mi)?;
                Some((f.path.clone(), m.m.line))
            }
            RowRef::File(fi) => {
                let f = self.files.get(*fi)?;
                Some((
                    f.path.clone(),
                    f.matches.first().map(|m| m.m.line).unwrap_or(1),
                ))
            }
        }
    }

    // ── results ─────────────────────────────────────────────────────────────

    fn rows(&self) -> Vec<RowRef> {
        let mut out = Vec::new();
        for (fi, f) in self.files.iter().enumerate() {
            out.push(RowRef::File(fi));
            for mi in 0..f.matches.len() {
                out.push(RowRef::Match(fi, mi));
            }
        }
        out
    }

    fn add_matches(&mut self, matches: Vec<Match>) {
        for m in matches {
            let fi = match self.index.get(&m.path) {
                Some(&fi) => fi,
                None => {
                    let fi = self.files.len();
                    self.index.insert(m.path.clone(), fi);
                    self.files.push(FileGroup {
                        path: m.path.clone(),
                        matches: Vec::new(),
                    });
                    fi
                }
            };
            self.files[fi].matches.push(MatchRow { m, selected: true });
            self.total_matches += 1;
        }
    }

    /// Drain the streamed batches + any apply report. Returns `true` if the
    /// surface changed (the loop should repaint).
    pub fn drain(&mut self) -> bool {
        let sg = self.search_gen;
        let mut dirty = false;
        while let Ok(batch) = self.rx.try_recv() {
            if batch.sg != sg {
                continue; // stale generation — discard
            }
            if !batch.matches.is_empty() {
                self.add_matches(batch.matches);
                dirty = true;
            }
            if batch.truncated {
                self.truncated = true;
            }
            if batch.done {
                self.searching = false;
                dirty = true;
            }
        }
        while let Ok(report) = self.apply_rx.try_recv() {
            let summary = report.summary_line();
            // Re-run the search so the results reflect the applied edits (and any
            // drifted matches drop out); `bump_search` clears `status`, so set the
            // apply summary *after* it so it survives the refresh.
            self.bump_search();
            self.status = Some(summary);
            dirty = true;
        }
        let n = self.rows().len();
        if n > 0 && self.selected >= n {
            self.selected = n - 1;
        }
        dirty
    }

    /// The accepted edits, grouped by file, for the guarded write path.
    pub fn accepted_edits(&self) -> Vec<(String, Vec<Edit>)> {
        let mut out = Vec::new();
        for f in &self.files {
            let edits: Vec<Edit> = f
                .matches
                .iter()
                .filter(|m| m.selected)
                .map(|m| Edit::from_match(&m.m, &self.replace, self.mode))
                .collect();
            if !edits.is_empty() {
                out.push((f.path.clone(), edits));
            }
        }
        out
    }

    pub fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
    }

    pub fn is_structural(&self) -> bool {
        self.structural
    }

    fn selected_count(&self) -> usize {
        self.files
            .iter()
            .flat_map(|f| f.matches.iter())
            .filter(|m| m.selected)
            .count()
    }

    // ── render ──────────────────────────────────────────────────────────────

    /// Draw the overlay onto `surface`. Damage is `Full` (chrome), driven by the
    /// existing layer path.
    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        const COLS: usize = 92;
        let body_rows = 16usize;
        let spec = LayerSpec {
            title: "search & replace".into(),
            badge: Some(" ^⇧H ".into()),
            cols: COLS,
            rows: body_rows,
            anchor: Anchor::TopThird,
            ..LayerSpec::default()
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let panel = Tok::Slot(S::Panel);
        let rule = Line::Fill {
            ch: '╌',
            fg: Tok::Slot(S::Ghost3),
        };
        let mut y = inner.y;

        // Query field.
        let q_focus = self.focus == Field::Query;
        let mut qline = vec![seg(Tok::Slot(S::Accent), "search  ").bold()];
        push_field(&mut qline, &self.query, q_focus, "pattern…");
        if self.searching {
            qline.push(seg(Tok::Slot(S::Ghost2), "  …"));
        }
        seg::draw_line(surface, inner.x, y, inner.cols, &Line::segs(qline), panel);
        y += 1;

        // Replace field.
        let r_focus = self.focus == Field::Replace;
        let mut rline = vec![seg(Tok::Slot(S::Accent), "replace ").bold()];
        push_field(&mut rline, &self.replace, r_focus, "replacement…");
        seg::draw_line(surface, inner.x, y, inner.cols, &Line::segs(rline), panel);
        y += 1;

        // Options line.
        let opt = |on: bool, label: &str| -> seg::Seg {
            if on {
                seg(Tok::Slot(S::Accent), format!("[{label}]")).bold()
            } else {
                seg(Tok::Slot(S::Ghost2), format!(" {label} "))
            }
        };
        let opts = Line::segs(vec![
            sp(0),
            opt(self.mode == SearchMode::Regex, "regex"),
            sp(1),
            opt(self.case_sensitive, "case"),
            sp(1),
            opt(self.whole_word, "word"),
            sp(1),
            opt(self.structural, "ast"),
            sp(1),
            opt(self.include_hidden, "hidden"),
            sp(1),
            opt(!self.respect_gitignore, "no-ignore"),
        ]);
        seg::draw_line(surface, inner.x, y, inner.cols, &opts, panel);
        y += 1;

        // Inline regex error.
        if let Some(err) = &self.regex_error {
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![seg(Tok::Slot(S::ActivityWaiting), format!("⚠ {err}"))]),
                panel,
            );
        }
        y += 1;
        seg::draw_line(surface, inner.x, y, inner.cols, &rule, panel);
        y += 1;

        // Results.
        let list_bottom = inner.y + inner.rows.saturating_sub(2);
        let rows = self.rows();
        let visible = list_bottom.saturating_sub(y);
        let offset = self.scroll_offset(visible, rows.len());
        for (ri, row) in rows.iter().enumerate().skip(offset) {
            if y >= list_bottom {
                break;
            }
            let sel = ri == self.selected;
            match row {
                RowRef::File(fi) => {
                    let f = &self.files[*fi];
                    let all_on = f.matches.iter().all(|m| m.selected);
                    let glyph = if all_on { "◆" } else { "◇" };
                    let label = format!("{glyph} {} ({})", f.path, f.matches.len());
                    draw_row(surface, inner.x, y, inner.cols, &label, sel, true);
                    y += 1;
                }
                RowRef::Match(fi, mi) => {
                    let f = &self.files[*fi];
                    let mr = &f.matches[*mi];
                    let mark = if mr.selected { "✔" } else { "·" };
                    let before = mr.m.line_text.trim();
                    let after = render_after_line(&mr.m, &self.replace, self.mode);
                    let after = after.trim();
                    let label = if self.replace.is_empty() {
                        format!("  {mark} {}: {before}", mr.m.line)
                    } else {
                        format!("  {mark} {}: {before} → {after}", mr.m.line)
                    };
                    draw_row(surface, inner.x, y, inner.cols, &label, sel, false);
                    y += 1;
                }
            }
        }
        if rows.is_empty() && !self.searching {
            let msg = if self.query.is_empty() {
                "type a pattern"
            } else {
                "no matches"
            };
            seg::draw_line(
                surface,
                inner.x,
                y,
                inner.cols,
                &Line::segs(vec![sp(1), seg(Tok::Slot(S::Ghost2), msg)]),
                panel,
            );
        }

        // Footer.
        let fy = inner.y + inner.rows - 1;
        let mut foot = format!(
            "{} match(es) in {} file(s) · {} selected",
            self.total_matches,
            self.files.len(),
            self.selected_count()
        );
        if self.truncated {
            foot.push_str(" · truncated");
        }
        if let Some(st) = &self.status {
            foot = st.clone();
        }
        let footer = Line::split(
            vec![
                seg(Tok::Slot(S::Ghost2), "↵"),
                seg(Tok::Slot(S::Ghost), " apply  "),
                seg(Tok::Slot(S::Ghost2), "^t"),
                seg(Tok::Slot(S::Ghost), " toggle  "),
                seg(Tok::Slot(S::Ghost2), "⇥"),
                seg(Tok::Slot(S::Ghost), " field  "),
                seg(Tok::Slot(S::Ghost2), "^o"),
                seg(Tok::Slot(S::Ghost), " editor  "),
                seg(Tok::Slot(S::Ghost2), "esc"),
                seg(Tok::Slot(S::Ghost), " close"),
            ],
            vec![seg(Tok::Slot(S::Ghost3), foot)],
        );
        seg::draw_line(surface, inner.x, fy, inner.cols, &footer, panel);
    }

    fn scroll_offset(&self, visible: usize, total: usize) -> usize {
        if visible == 0 || total <= visible {
            return 0;
        }
        if self.selected < visible {
            0
        } else {
            (self.selected + 1 - visible).min(total - visible)
        }
    }
}

fn split_globs(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|g| !g.is_empty())
        .map(str::to_string)
        .collect()
}

fn push_field(segs: &mut Vec<seg::Seg>, text: &str, focused: bool, placeholder: &str) {
    if text.is_empty() {
        let ph = seg(Tok::Slot(S::Ghost3), placeholder);
        segs.push(if focused { ph.into_caret() } else { ph });
    } else {
        segs.push(seg(Tok::Slot(S::Text), text.to_string()));
        if focused {
            segs.push(seg(Tok::Slot(S::Text), " ").into_caret());
        }
    }
}

fn draw_row(
    surface: &mut Surface,
    x: usize,
    y: usize,
    cols: usize,
    label: &str,
    sel: bool,
    header: bool,
) {
    let panel = Tok::Slot(S::Panel);
    let pad = if sel { Tok::SelAccent } else { panel };
    let fg = if header || sel {
        Tok::Slot(S::Text)
    } else {
        Tok::Slot(S::Dim)
    };
    let s = seg(fg, label.to_string());
    let s = if header || sel { s.bold() } else { s };
    seg::draw_line(surface, x, y, cols, &Line::segs(vec![sp(1), s]), pad);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay() -> SearchReplaceOverlay {
        SearchReplaceOverlay::new("", true, false, 1000, true)
    }

    fn feed(o: &mut SearchReplaceOverlay, matches: Vec<Match>) {
        // Inject a batch as if the worker delivered it for the current gen.
        let _ = o.tx.send(SearchBatch {
            sg: o.search_gen,
            matches,
            done: true,
            truncated: false,
        });
        o.drain();
    }

    fn m(path: &str, line: usize, text: &str, start: usize, end: usize) -> Match {
        Match {
            path: path.into(),
            line,
            byte_start: start,
            byte_end: end,
            line_text: text.into(),
            content_hash: thegn_core::search_replace::fnv1a_64(text.as_bytes()),
            captures: vec![text[start..end].to_string()],
        }
    }

    #[test]
    fn typing_query_bumps_gen_and_requests_search() {
        let mut o = overlay();
        assert_eq!(o.push_char('f'), Outcome::Search);
        assert_eq!(o.push_char('o'), Outcome::Search);
        assert!(o.spec().is_some());
        assert_eq!(o.spec().unwrap().query, "fo");
        // Every keystroke supersedes the last generation.
        assert_eq!(o.search_gen, 2);
        assert_eq!(o.current.load(Ordering::Acquire), 2);
        // The loop can pick up exactly one pending request.
        assert!(o.take_search_request().is_some());
        assert!(o.take_search_request().is_none());
    }

    #[test]
    fn editing_replace_field_does_not_research() {
        let mut o = overlay();
        o.push_char('f');
        let _ = o.take_search_request();
        o.toggle_field();
        assert_eq!(o.push_char('X'), Outcome::None);
        assert!(o.take_search_request().is_none());
        assert_eq!(o.replace, "X");
    }

    #[test]
    fn invalid_regex_blocks_search() {
        let mut o = overlay();
        o.toggle_regex();
        for c in "fn (".chars() {
            o.push_char(c);
        }
        assert!(o.regex_error.is_some());
        assert!(o.spec().is_none());
        assert!(o.take_search_request().is_none());
    }

    #[test]
    fn drain_groups_matches_by_file() {
        let mut o = overlay();
        o.push_char('x');
        feed(
            &mut o,
            vec![
                m("a.rs", 1, "let x = 1", 4, 5),
                m("a.rs", 2, "x + x", 0, 1),
                m("b.rs", 9, "xy", 0, 1),
            ],
        );
        assert_eq!(o.files.len(), 2);
        assert_eq!(o.total_matches, 3);
        assert!(!o.searching);
    }

    #[test]
    fn stale_generation_discarded() {
        let mut o = overlay();
        o.push_char('x');
        let stale = o.search_gen;
        o.push_char('y'); // new gen
        let _ = o.tx.send(SearchBatch {
            sg: stale,
            matches: vec![m("a.rs", 1, "x", 0, 1)],
            done: true,
            truncated: false,
        });
        o.drain();
        assert_eq!(o.total_matches, 0);
    }

    #[test]
    fn toggle_deselects_match_and_file() {
        let mut o = overlay();
        o.push_char('x');
        feed(
            &mut o,
            vec![m("a.rs", 1, "x", 0, 1), m("a.rs", 2, "x", 0, 1)],
        );
        // rows: [File(0), Match(0,0), Match(0,1)]
        o.move_down(); // select Match(0,0)
        o.toggle_selected();
        assert_eq!(o.selected_count(), 1);
        // The file header, when partially selected, fills to all-on…
        o.move_up(); // File(0)
        o.toggle_selected();
        assert_eq!(o.selected_count(), 2);
        // …and toggling again (now all-on) clears the whole file.
        o.toggle_selected();
        assert_eq!(o.selected_count(), 0);
    }

    #[test]
    fn accepted_edits_only_selected() {
        let mut o = overlay();
        o.push_char('x');
        o.toggle_field();
        for c in "Y".chars() {
            o.push_char(c);
        }
        feed(
            &mut o,
            vec![m("a.rs", 1, "x", 0, 1), m("a.rs", 2, "x", 0, 1)],
        );
        // Deselect the second match.
        o.selected = 2; // Match(0,1)
        o.toggle_selected();
        let edits = o.accepted_edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].0, "a.rs");
        assert_eq!(edits[0].1.len(), 1);
        assert_eq!(edits[0].1[0].replacement, "Y");
    }

    #[test]
    fn selected_location_for_editor() {
        let mut o = overlay();
        o.push_char('x');
        feed(&mut o, vec![m("a.rs", 7, "x", 0, 1)]);
        o.selected = 1; // the match row
        assert_eq!(o.selected_location(), Some(("a.rs".to_string(), 7)));
    }

    #[test]
    fn apply_report_drains_to_status_and_researches() {
        let mut o = overlay();
        o.push_char('x');
        feed(&mut o, vec![m("a.rs", 1, "x", 0, 1)]);
        let gen_before = o.search_gen;
        let mut r = ApplyReport::default();
        r.push(thegn_core::search_replace::FileApplyResult {
            path: "a.rs".into(),
            applied: 1,
            skipped_drift: 0,
            error: None,
        });
        let _ = o.apply_tx.send(r);
        o.drain();
        assert!(o.status.as_deref().unwrap().contains("1 replacement"));
        assert!(o.search_gen > gen_before); // re-searched
    }

    #[test]
    fn structural_toggle_needs_binary() {
        let mut o = SearchReplaceOverlay::new("x", true, false, 100, false);
        assert_eq!(o.toggle_structural(), Outcome::None);
        assert!(!o.is_structural());
        assert!(o.status.is_some());
        // With the binary available, it toggles.
        let mut o2 = overlay();
        o2.toggle_structural();
        assert!(o2.is_structural());
    }

    #[test]
    fn truncation_flag_set() {
        let mut o = overlay();
        o.push_char('x');
        let _ = o.tx.send(SearchBatch {
            sg: o.search_gen,
            matches: vec![m("a", 1, "x", 0, 1)],
            done: true,
            truncated: true,
        });
        o.drain();
        assert!(o.truncated);
    }
}
