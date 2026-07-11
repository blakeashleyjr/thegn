//! The full-screen in-app PR workflow view. Pressing Enter on the panel's
//! `Section::Pr` opens this modal (the panel row stays the at-a-glance summary);
//! `o` still opens the browser as an escape hatch.
//!
//! Lifecycle mirrors the other loop modals ([`crate::detail::DetailOverlay`],
//! the wizards): an `Option<PrView>` slot in the loop, fed `handle_key`, painted
//! last via `render`, dismissed on Esc/q. Unlike `DetailOverlay` it is tabbed,
//! near-full-screen, receives async-loaded data (diff + conversation land after
//! open over `pr_view_tx`), and carries a text-composer sub-mode for writing
//! comments / reviews. Writes never touch the loop: `handle_key` returns a
//! [`PrViewAction`] the loop runs off-thread via `spawn_pr_action`.

use termwiz::input::{KeyCode, Modifiers};
use termwiz::surface::Surface;

use crate::chrome::S;
use crate::compositor::Rect;
use crate::layer::{Anchor, LayerSpec, open_layer};
use crate::panel::{CheckLine, CheckState, PrSummary};
use crate::seg::{Line, Seg, Tok, seg, sp};
use thegn_core::github::{DiffFile, DiffLine, DiffLineKind, PrConversation, PrDiff, ReviewState};

/// The four workflow tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrTab {
    Overview,
    Checks,
    Conversation,
    Files,
}

impl PrTab {
    const ALL: [PrTab; 4] = [
        PrTab::Overview,
        PrTab::Checks,
        PrTab::Conversation,
        PrTab::Files,
    ];
    fn label(self) -> &'static str {
        match self {
            PrTab::Overview => "Overview",
            PrTab::Checks => "Checks",
            PrTab::Conversation => "Conversation",
            PrTab::Files => "Files",
        }
    }
    fn index(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// What a composer, when submitted, produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerTarget {
    /// A PR-level (issue) comment.
    PrComment,
    /// A submitted review with the given state (request-changes / comment).
    Review(ReviewState),
    /// A reply to an existing review thread.
    ThreadReply { thread_id: String, label: String },
    /// An inline review comment anchored to a new-side line.
    LineComment { path: String, line: u64 },
}

impl ComposerTarget {
    fn heading(&self) -> String {
        match self {
            ComposerTarget::PrComment => "New comment".into(),
            ComposerTarget::Review(ReviewState::RequestChanges) => "Request changes".into(),
            ComposerTarget::Review(ReviewState::Comment) => "Review comment".into(),
            ComposerTarget::Review(ReviewState::Approve) => "Approve".into(),
            ComposerTarget::ThreadReply { label, .. } => format!("Reply · {label}"),
            ComposerTarget::LineComment { path, line } => format!("Comment on {path}:{line}"),
        }
    }
}

/// A tiny multi-line text editor for composing comment / review bodies. Append +
/// backspace + newline + paste; the cursor stays at the end (enough for a
/// compose box — mid-string editing can come later).
#[derive(Debug, Default, Clone)]
pub struct TextArea {
    buf: String,
}

impl TextArea {
    fn insert_char(&mut self, c: char) {
        self.buf.push(c);
    }
    /// Paste keeps newlines (a comment body is multi-line), unlike the single
    /// line picker field.
    fn insert_str(&mut self, s: &str) {
        for c in s.chars().filter(|c| *c != '\r') {
            self.buf.push(c);
        }
    }
    fn backspace(&mut self) -> bool {
        self.buf.pop().is_some()
    }
    fn as_str(&self) -> &str {
        &self.buf
    }
    fn is_blank(&self) -> bool {
        self.buf.trim().is_empty()
    }
}

/// The open composer sub-mode.
#[derive(Debug, Clone)]
pub struct Composer {
    pub target: ComposerTarget,
    pub field: TextArea,
}

/// Async-loaded data delivered over `pr_view_tx` after the view opens (or after
/// a write refresh). Stale generations are dropped by the loop.
#[derive(Debug, Clone)]
pub struct PrViewData {
    pub generation: u64,
    pub conversation: Option<PrConversation>,
    pub diff: Option<PrDiff>,
}

/// What a key delivered to the view meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrViewOutcome {
    /// Consumed; nothing else to do.
    Pending,
    /// Close the view.
    Close,
    /// Run this action off the loop (the view stays open).
    Act(PrViewAction),
}

/// A side effect the loop runs off-thread (via `spawn_pr_action`) and then
/// refreshes. Each variant carries everything the executor needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrViewAction {
    /// Open the PR in the system browser.
    OpenUrl(String),
    /// Squash-merge the PR.
    Merge,
    /// Approve (no body).
    Approve,
    /// Re-run failed checks.
    Rerun,
    /// Post a PR-level comment.
    Comment { body: String },
    /// Submit a review with a state + body.
    Review { state: ReviewState, body: String },
    /// Reply to a review thread.
    Reply { thread_id: String, body: String },
    /// Post an inline review comment.
    LineComment {
        owner: String,
        repo: String,
        number: u64,
        commit_id: String,
        path: String,
        line: u64,
        body: String,
    },
}

/// One logical conversation row (for cursor navigation + reply targeting).
#[derive(Debug, Clone, Copy)]
enum ConvRow {
    Comment(usize),
    Review(usize),
    Thread(usize),
}

/// The full-screen PR view. Identity + checks are snapshotted from the panel
/// cache at open; `conversation`/`diff` load asynchronously.
pub struct PrView {
    // identity (snapshot at open)
    pub number: u64,
    pub url: String,
    pub owner: String,
    pub repo: String,
    pub head_sha: String,
    pub title: String,
    pub state: String,
    pub is_draft: bool,
    pub branch: String,
    pub base: String,
    pub review_decision: Option<String>,
    pub mergeable: String,
    pub merge_state: String,
    pub checks: Vec<CheckLine>,
    // async-loaded
    pub conversation: Option<PrConversation>,
    pub diff: Option<PrDiff>,
    /// The fetch generation — the loop drops data from stale generations.
    pub generation: u64,
    // ui
    tab: PrTab,
    sel: usize,
    scroll: std::cell::Cell<usize>,
    /// The expanded file in the Files tab (`None` = file list).
    open_file: Option<usize>,
    composer: Option<Composer>,
    /// An inline result/status line shown in the footer.
    pub status: Option<String>,
}

impl PrView {
    /// Open the view from the panel's cached PR summary + checks. `head_oid` /
    /// `mergeable` / `merge_state` come from the panel data (they live on
    /// `PanelData`, not the compact `PrSummary`).
    pub fn open(
        pr: &PrSummary,
        checks: &[CheckLine],
        base: &str,
        head_oid: &str,
        mergeable: &str,
        merge_state: &str,
    ) -> Self {
        let (owner, repo) = thegn_core::github::owner_repo_from_url(&pr.url).unwrap_or_default();
        PrView {
            number: pr.number,
            url: pr.url.clone(),
            owner,
            repo,
            head_sha: head_oid.to_string(),
            title: pr.title.clone(),
            state: pr.state.clone(),
            is_draft: pr.is_draft,
            branch: String::new(),
            base: base.to_string(),
            review_decision: pr.review_decision.clone(),
            mergeable: mergeable.to_string(),
            merge_state: merge_state.to_string(),
            checks: checks.to_vec(),
            conversation: None,
            diff: None,
            generation: 0,
            tab: PrTab::Overview,
            sel: 0,
            scroll: std::cell::Cell::new(0),
            open_file: None,
            composer: None,
            status: None,
        }
    }

    /// Apply an async data delivery (from `pr_view_tx`).
    pub fn apply_data(&mut self, data: PrViewData) {
        if let Some(c) = data.conversation {
            self.conversation = Some(c);
        }
        if let Some(d) = data.diff {
            self.diff = Some(d);
        }
    }

    /// Whether the diff + conversation are still loading.
    fn loading(&self) -> bool {
        self.conversation.is_none() || self.diff.is_none()
    }

    // --- navigation model --------------------------------------------------

    /// The number of selectable rows in the current tab.
    fn row_count(&self) -> usize {
        match self.tab {
            PrTab::Overview => 0,
            PrTab::Checks => self.checks.len(),
            PrTab::Conversation => self.conv_rows().len(),
            PrTab::Files => match self.open_file {
                None => self.diff.as_ref().map_or(0, |d| d.files.len()),
                Some(i) => self.open_file_lines(i).len(),
            },
        }
    }

    /// The logical rows of the Conversation tab (timeline then threads).
    fn conv_rows(&self) -> Vec<ConvRow> {
        let mut rows = Vec::new();
        if let Some(c) = &self.conversation {
            for i in 0..c.comments.len() {
                rows.push(ConvRow::Comment(i));
            }
            for i in 0..c.reviews.len() {
                rows.push(ConvRow::Review(i));
            }
            for i in 0..c.threads.len() {
                rows.push(ConvRow::Thread(i));
            }
        }
        rows
    }

    /// The flattened diff lines of file `i` (the Files-open selectable rows).
    fn open_file_lines(&self, i: usize) -> Vec<&DiffLine> {
        self.diff
            .as_ref()
            .and_then(|d| d.files.get(i))
            .map(|f| f.hunks.iter().flat_map(|h| &h.lines).collect())
            .unwrap_or_default()
    }

    fn switch_tab(&mut self, tab: PrTab) {
        self.tab = tab;
        self.sel = 0;
        self.scroll.set(0);
        self.status = None;
    }

    // --- input -------------------------------------------------------------

    pub fn handle_key(&mut self, key: &KeyCode, mods: Modifiers) -> PrViewOutcome {
        if self.composer.is_some() {
            return self.composer_key(key, mods);
        }
        let ctrl = mods.contains(Modifiers::CTRL);
        if ctrl && matches!(key, KeyCode::Char('c' | 'C' | 'g' | 'G')) {
            return PrViewOutcome::Close;
        }
        match key {
            KeyCode::Char('q') => return PrViewOutcome::Close,
            KeyCode::Escape => {
                // In an expanded file, Esc collapses back to the file list first.
                if self.tab == PrTab::Files && self.open_file.take().is_some() {
                    self.sel = 0;
                    self.scroll.set(0);
                    return PrViewOutcome::Pending;
                }
                return PrViewOutcome::Close;
            }
            // Tab switching.
            KeyCode::Tab => {
                let next = (self.tab.index() + 1) % PrTab::ALL.len();
                self.switch_tab(PrTab::ALL[next]);
                return PrViewOutcome::Pending;
            }
            KeyCode::Char(c @ '1'..='4') => {
                let idx = (*c as usize) - ('1' as usize);
                self.switch_tab(PrTab::ALL[idx]);
                return PrViewOutcome::Pending;
            }
            KeyCode::Char('o') => {
                return PrViewOutcome::Act(PrViewAction::OpenUrl(self.url.clone()));
            }
            // Navigation.
            KeyCode::Char('j') | KeyCode::DownArrow => {
                self.move_sel(1);
                return PrViewOutcome::Pending;
            }
            KeyCode::Char('k') | KeyCode::UpArrow => {
                self.move_sel(-1);
                return PrViewOutcome::Pending;
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                self.move_sel(10);
                return PrViewOutcome::Pending;
            }
            KeyCode::PageUp => {
                self.move_sel(-10);
                return PrViewOutcome::Pending;
            }
            KeyCode::Char('g') => {
                self.sel = 0;
                self.scroll.set(0);
                return PrViewOutcome::Pending;
            }
            KeyCode::Char('G') => {
                self.sel = self.row_count().saturating_sub(1);
                return PrViewOutcome::Pending;
            }
            _ => {}
        }
        // Global write/action keys (available from any tab where the PR exists).
        match key {
            KeyCode::Char('M') => return PrViewOutcome::Act(PrViewAction::Merge),
            KeyCode::Char('A') => return PrViewOutcome::Act(PrViewAction::Approve),
            KeyCode::Char('R') => {
                self.open_composer(ComposerTarget::Review(ReviewState::RequestChanges));
                return PrViewOutcome::Pending;
            }
            KeyCode::Char('C') => {
                self.open_composer(ComposerTarget::Review(ReviewState::Comment));
                return PrViewOutcome::Pending;
            }
            // 'c' is a PR comment everywhere EXCEPT inside an expanded file,
            // where it means "comment on this line" (handled by `files_key`).
            KeyCode::Char('c') if !(self.tab == PrTab::Files && self.open_file.is_some()) => {
                self.open_composer(ComposerTarget::PrComment);
                return PrViewOutcome::Pending;
            }
            _ => {}
        }
        // Per-tab actions.
        match self.tab {
            PrTab::Checks => self.checks_key(key),
            PrTab::Conversation => self.conversation_key(key),
            PrTab::Files => self.files_key(key),
            PrTab::Overview => PrViewOutcome::Pending,
        }
    }

    fn move_sel(&mut self, delta: isize) {
        let n = self.row_count();
        if n == 0 {
            return;
        }
        let cur = self.sel as isize;
        self.sel = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    fn checks_key(&mut self, key: &KeyCode) -> PrViewOutcome {
        match key {
            KeyCode::Char('r') => PrViewOutcome::Act(PrViewAction::Rerun),
            KeyCode::Enter => match self
                .checks
                .get(self.sel)
                .and_then(|c| c.details_url.clone())
            {
                Some(u) if !u.is_empty() => PrViewOutcome::Act(PrViewAction::OpenUrl(u)),
                _ => PrViewOutcome::Pending,
            },
            _ => PrViewOutcome::Pending,
        }
    }

    fn conversation_key(&mut self, key: &KeyCode) -> PrViewOutcome {
        match key {
            KeyCode::Char('r') | KeyCode::Enter => {
                // Reply to the selected thread (if the cursor is on one).
                if let Some(ConvRow::Thread(i)) = self.conv_rows().get(self.sel).copied()
                    && let Some(t) = self.conversation.as_ref().and_then(|c| c.threads.get(i))
                {
                    let label = format!(
                        "{}:{}",
                        t.path,
                        t.line.map(|l| l.to_string()).unwrap_or_default()
                    );
                    self.open_composer(ComposerTarget::ThreadReply {
                        thread_id: t.id.clone(),
                        label,
                    });
                }
                PrViewOutcome::Pending
            }
            _ => PrViewOutcome::Pending,
        }
    }

    fn files_key(&mut self, key: &KeyCode) -> PrViewOutcome {
        match self.open_file {
            None => match key {
                KeyCode::Enter | KeyCode::RightArrow => {
                    if self.diff.as_ref().is_some_and(|d| self.sel < d.files.len()) {
                        self.open_file = Some(self.sel);
                        self.sel = 0;
                        self.scroll.set(0);
                    }
                    PrViewOutcome::Pending
                }
                _ => PrViewOutcome::Pending,
            },
            Some(fi) => match key {
                KeyCode::LeftArrow => {
                    self.open_file = None;
                    self.sel = 0;
                    self.scroll.set(0);
                    PrViewOutcome::Pending
                }
                KeyCode::Char('c') => {
                    // 'c' is claimed globally as PR-comment above; only reached
                    // for line comments when a file is open. Anchor to the
                    // selected new-side line.
                    let lines = self.open_file_lines(fi);
                    let path = self
                        .diff
                        .as_ref()
                        .and_then(|d| d.files.get(fi))
                        .map(|f| f.path.clone());
                    if let (Some(path), Some(line)) =
                        (path, lines.get(self.sel).and_then(|l| l.new_lineno))
                    {
                        self.open_composer(ComposerTarget::LineComment { path, line });
                    } else {
                        self.status = Some("select an added/context line to comment".into());
                    }
                    PrViewOutcome::Pending
                }
                _ => PrViewOutcome::Pending,
            },
        }
    }

    fn open_composer(&mut self, target: ComposerTarget) {
        self.composer = Some(Composer {
            target,
            field: TextArea::default(),
        });
        self.status = None;
    }

    pub fn handle_paste(&mut self, s: &str) {
        if let Some(c) = self.composer.as_mut() {
            c.field.insert_str(s);
        }
    }

    fn composer_key(&mut self, key: &KeyCode, mods: Modifiers) -> PrViewOutcome {
        let ctrl = mods.contains(Modifiers::CTRL);
        // Submit: Ctrl-D / Ctrl-S.
        if ctrl && matches!(key, KeyCode::Char('d' | 'D' | 's' | 'S')) {
            return self.submit_composer();
        }
        if ctrl && matches!(key, KeyCode::Char('c' | 'C')) {
            self.composer = None;
            return PrViewOutcome::Pending;
        }
        match key {
            KeyCode::Escape => {
                self.composer = None;
                PrViewOutcome::Pending
            }
            KeyCode::Enter => {
                if let Some(c) = self.composer.as_mut() {
                    c.field.insert_char('\n');
                }
                PrViewOutcome::Pending
            }
            KeyCode::Backspace => {
                if let Some(c) = self.composer.as_mut() {
                    c.field.backspace();
                }
                PrViewOutcome::Pending
            }
            KeyCode::Char(ch) if !ctrl && !mods.contains(Modifiers::ALT) => {
                if let Some(c) = self.composer.as_mut() {
                    c.field.insert_char(*ch);
                }
                PrViewOutcome::Pending
            }
            _ => PrViewOutcome::Pending,
        }
    }

    fn submit_composer(&mut self) -> PrViewOutcome {
        let Some(comp) = self.composer.take() else {
            return PrViewOutcome::Pending;
        };
        if comp.field.is_blank() {
            self.status = Some("empty — nothing sent".into());
            return PrViewOutcome::Pending;
        }
        let body = comp.field.as_str().to_string();
        let action = match comp.target {
            ComposerTarget::PrComment => PrViewAction::Comment { body },
            ComposerTarget::Review(state) => PrViewAction::Review { state, body },
            ComposerTarget::ThreadReply { thread_id, .. } => {
                PrViewAction::Reply { thread_id, body }
            }
            ComposerTarget::LineComment { path, line } => PrViewAction::LineComment {
                owner: self.owner.clone(),
                repo: self.repo.clone(),
                number: self.number,
                commit_id: self.head_sha.clone(),
                path,
                line,
                body,
            },
        };
        PrViewOutcome::Act(action)
    }

    // --- rendering ---------------------------------------------------------

    pub fn render(&self, surface: &mut Surface, screen: Rect) {
        let cols = screen.cols.saturating_sub(8).max(24);
        let rows = screen.rows.saturating_sub(4).max(10);
        let draft = if self.is_draft { " · draft" } else { "" };
        let spec = LayerSpec {
            title: format!("PR #{} · {}{}", self.number, self.title, draft),
            badge: Some(" esc ".into()),
            cols,
            rows,
            anchor: Anchor::Center,
            dim: true,
            shadow: true,
            bg: Tok::Slot(S::Panel),
            border: Tok::Slot(S::Accent),
        };
        let Some(inner) = open_layer(surface, screen, &spec) else {
            return;
        };
        let pad = Tok::Slot(S::Panel);
        // Tab bar (row 0).
        crate::seg::draw_line(surface, inner.x, inner.y, inner.cols, &self.tab_bar(), pad);
        // Footer (last row).
        let footer_y = inner.y + inner.rows.saturating_sub(1);
        crate::seg::draw_line(surface, inner.x, footer_y, inner.cols, &self.footer(), pad);

        // Body region between the tab bar and footer.
        let body_top = inner.y + 1;
        let body_rows = inner.rows.saturating_sub(2);
        if body_rows == 0 {
            return;
        }

        // Composer, when open, takes the bottom half of the body.
        let (list_rows, comp_rows) = match &self.composer {
            Some(_) => {
                let c = (body_rows / 2)
                    .clamp(4, 12)
                    .min(body_rows.saturating_sub(1));
                (body_rows - c, c)
            }
            None => (body_rows, 0),
        };

        let body = self.body_lines(inner.cols);
        let sel_line = body.iter().position(|(_, s)| *s).unwrap_or(0);
        let scroll = self.clamp_scroll(sel_line, body.len(), list_rows);
        for row in 0..list_rows {
            let y = body_top + row;
            match body.get(scroll + row) {
                Some((line, selected)) => {
                    let bg = if *selected { Tok::SelAccent } else { pad };
                    crate::seg::draw_line(surface, inner.x, y, inner.cols, line, bg);
                }
                None => crate::seg::draw_line(surface, inner.x, y, inner.cols, &Line::Blank, pad),
            }
        }
        if let Some(comp) = &self.composer {
            let rect = Rect {
                x: inner.x,
                y: body_top + list_rows,
                cols: inner.cols,
                rows: comp_rows,
            };
            self.render_composer(surface, rect, comp);
        }
    }

    /// Keep the selected line in view; returns the (persisted) scroll offset.
    fn clamp_scroll(&self, sel_line: usize, total: usize, visible: usize) -> usize {
        let mut s = self.scroll.get();
        if visible == 0 {
            return 0;
        }
        if sel_line < s {
            s = sel_line;
        } else if sel_line >= s + visible {
            s = sel_line + 1 - visible;
        }
        let max = total.saturating_sub(visible);
        s = s.min(max);
        self.scroll.set(s);
        s
    }

    fn tab_bar(&self) -> Line {
        let mut segs: Vec<Seg> = Vec::new();
        for (i, t) in PrTab::ALL.iter().enumerate() {
            if i > 0 {
                segs.push(seg(Tok::Slot(S::Ghost3), "  "));
            }
            let label = format!(" {} ", t.label());
            if *t == self.tab {
                segs.push(Seg::chip(Tok::Slot(S::Accent), label));
            } else {
                segs.push(seg(Tok::Slot(S::Dim), label));
            }
        }
        Line::segs(segs)
    }

    fn footer(&self) -> Line {
        if let Some(c) = &self.composer {
            return Line::segs(vec![
                seg(Tok::Slot(S::Accent), c.target.heading()),
                seg(
                    Tok::Slot(S::Dim),
                    "   Ctrl-D send · Enter newline · Esc cancel",
                ),
            ]);
        }
        if let Some(s) = &self.status {
            return Line::segs(vec![seg(Tok::Slot(S::Accent), s.clone())]);
        }
        let hint = match self.tab {
            PrTab::Overview => {
                "M merge · A approve · R request-changes · C review · c comment · o browser"
            }
            PrTab::Checks => "↑↓ move · Enter open · r re-run failed · c comment",
            PrTab::Conversation => "↑↓ move · r reply · c comment · A/R/C review",
            PrTab::Files => "↑↓ move · Enter open file · ← back · c comment line",
        };
        Line::segs(vec![
            seg(Tok::Slot(S::Faint), "Tab switch · "),
            seg(Tok::Slot(S::Dim), hint),
        ])
    }

    fn body_lines(&self, cols: usize) -> Vec<(Line, bool)> {
        match self.tab {
            PrTab::Overview => self.overview_lines(),
            PrTab::Checks => self.checks_body(),
            PrTab::Conversation => self.conversation_body(cols),
            PrTab::Files => self.files_body(cols),
        }
    }

    fn overview_lines(&self) -> Vec<(Line, bool)> {
        let t = Tok::Slot(S::Text);
        let d = Tok::Slot(S::Dim);
        let kv = |k: &str, v: String| {
            (
                Line::segs(vec![seg(d, format!("{k:<10}")), seg(t, v)]),
                false,
            )
        };
        let (pass, fail, pend) = self.check_counts();
        let review = self.review_decision.clone().unwrap_or_else(|| "—".into());
        vec![
            (Line::Blank, false),
            kv(
                "state",
                format!(
                    "{}{}",
                    self.state,
                    if self.is_draft { " (draft)" } else { "" }
                ),
            ),
            kv(
                "branch",
                format!("{} → {}", self.branch_or_head(), self.base),
            ),
            kv("review", review),
            kv(
                "mergeable",
                format!("{} · {}", self.mergeable, self.merge_state),
            ),
            kv(
                "checks",
                format!("{pass} ok · {fail} failed · {pend} pending"),
            ),
            (Line::Blank, false),
            (
                Line::segs(vec![seg(
                    d,
                    if self.loading() {
                        "loading conversation + diff…"
                    } else {
                        "Tab → Checks · Conversation · Files"
                    },
                )]),
                false,
            ),
        ]
    }

    fn branch_or_head(&self) -> String {
        if self.branch.is_empty() {
            "HEAD".into()
        } else {
            self.branch.clone()
        }
    }

    fn check_counts(&self) -> (usize, usize, usize) {
        let mut pass = 0;
        let mut fail = 0;
        let mut pend = 0;
        for c in &self.checks {
            match c.state {
                CheckState::Pass => pass += 1,
                CheckState::Fail => fail += 1,
                CheckState::Pending => pend += 1,
            }
        }
        (pass, fail, pend)
    }

    fn checks_body(&self) -> Vec<(Line, bool)> {
        let mut out = vec![(Line::Blank, false)];
        if self.checks.is_empty() {
            out.push((
                Line::segs(vec![seg(Tok::Slot(S::Dim), "No checks reported.")]),
                false,
            ));
            return out;
        }
        for (i, c) in self.checks.iter().enumerate() {
            let selected = i == self.sel;
            let (glyph, tone) = match c.state {
                CheckState::Pass => ("✓", Tok::Hue(thegn_core::theme::Hue::Green)),
                CheckState::Fail => ("✗", Tok::Hue(thegn_core::theme::Hue::Red)),
                CheckState::Pending => ("•", Tok::Hue(thegn_core::theme::Hue::Amber)),
            };
            let marker = if selected { "❯ " } else { "  " };
            let dur = c.duration_secs.map(|s| format!("{s}s")).unwrap_or_default();
            out.push((
                Line::split(
                    vec![
                        seg(Tok::Slot(S::Faint), marker),
                        seg(tone, format!("{glyph} ")),
                        seg(Tok::Slot(S::Text), c.name.clone()),
                    ],
                    vec![seg(Tok::Slot(S::Dim), dur)],
                ),
                selected,
            ));
        }
        out
    }

    fn conversation_body(&self, cols: usize) -> Vec<(Line, bool)> {
        let mut out = vec![(Line::Blank, false)];
        let Some(conv) = &self.conversation else {
            out.push((
                Line::segs(vec![seg(Tok::Slot(S::Dim), "Loading conversation…")]),
                false,
            ));
            return out;
        };
        let rows = self.conv_rows();
        if rows.is_empty() {
            out.push((
                Line::segs(vec![seg(Tok::Slot(S::Dim), "No comments yet.")]),
                false,
            ));
            return out;
        }
        for (ri, row) in rows.iter().enumerate() {
            let selected = ri == self.sel;
            match row {
                ConvRow::Comment(i) => {
                    let c = &conv.comments[*i];
                    self.push_comment_block(&mut out, &c.author, &c.body, "💬", selected, cols);
                }
                ConvRow::Review(i) => {
                    let r = &conv.reviews[*i];
                    let tone = review_tone(&r.state);
                    out.push((
                        Line::segs(vec![
                            seg(Tok::Slot(S::Faint), sel_marker(selected)),
                            seg(tone, format!("{} ", review_glyph(&r.state))),
                            seg(Tok::Slot(S::Text), r.author.clone()).bold(),
                            seg(tone, format!("  {}", r.state)),
                        ]),
                        selected,
                    ));
                    self.push_body_lines(&mut out, &r.body, cols);
                }
                ConvRow::Thread(i) => {
                    let t = &conv.threads[*i];
                    let head = format!(
                        "{}:{}{}",
                        t.path,
                        t.line.map(|l| l.to_string()).unwrap_or_default(),
                        if t.resolved { " (resolved)" } else { "" }
                    );
                    out.push((
                        Line::segs(vec![
                            seg(Tok::Slot(S::Faint), sel_marker(selected)),
                            seg(Tok::Hue(thegn_core::theme::Hue::Blue), "▚ "),
                            seg(Tok::Slot(S::Dim), head),
                        ]),
                        selected,
                    ));
                    for c in &t.comments {
                        self.push_comment_block(&mut out, &c.author, &c.body, "  ↳", false, cols);
                    }
                }
            }
            out.push((Line::Blank, false));
        }
        out
    }

    fn push_comment_block(
        &self,
        out: &mut Vec<(Line, bool)>,
        author: &str,
        body: &str,
        glyph: &str,
        selected: bool,
        cols: usize,
    ) {
        out.push((
            Line::segs(vec![
                seg(Tok::Slot(S::Faint), sel_marker(selected)),
                seg(Tok::Slot(S::Accent), format!("{glyph} ")),
                seg(Tok::Slot(S::Text), author.to_string()).bold(),
            ]),
            selected,
        ));
        self.push_body_lines(out, body, cols);
    }

    /// Wrap + push a comment body as dim indented lines.
    fn push_body_lines(&self, out: &mut Vec<(Line, bool)>, body: &str, cols: usize) {
        let width = cols.saturating_sub(4).max(8);
        for raw in body.lines() {
            if raw.trim().is_empty() {
                continue;
            }
            for chunk in wrap(raw, width) {
                out.push((
                    Line::segs(vec![sp(4), seg(Tok::Slot(S::Dim), chunk)]),
                    false,
                ));
            }
        }
    }

    fn files_body(&self, cols: usize) -> Vec<(Line, bool)> {
        let mut out = vec![(Line::Blank, false)];
        let Some(diff) = &self.diff else {
            out.push((
                Line::segs(vec![seg(Tok::Slot(S::Dim), "Loading diff…")]),
                false,
            ));
            return out;
        };
        if diff.files.is_empty() {
            out.push((
                Line::segs(vec![seg(Tok::Slot(S::Dim), "No file changes.")]),
                false,
            ));
            return out;
        }
        match self.open_file {
            None => {
                for (i, f) in diff.files.iter().enumerate() {
                    let selected = i == self.sel;
                    let (adds, dels) = file_stat(f);
                    out.push((
                        Line::split(
                            vec![
                                seg(Tok::Slot(S::Faint), sel_marker(selected)),
                                seg(Tok::Slot(S::Text), f.path.clone()),
                            ],
                            vec![
                                seg(Tok::Hue(thegn_core::theme::Hue::Green), format!("+{adds} ")),
                                seg(Tok::Hue(thegn_core::theme::Hue::Red), format!("-{dels}")),
                            ],
                        ),
                        selected,
                    ));
                }
            }
            Some(fi) => {
                if let Some(f) = diff.files.get(fi) {
                    out.push((
                        Line::segs(vec![seg(Tok::Slot(S::Text), f.path.clone()).bold()]),
                        false,
                    ));
                    let mut li = 0usize; // index into flattened selectable lines
                    for h in &f.hunks {
                        out.push((
                            Line::segs(vec![seg(
                                Tok::Hue(thegn_core::theme::Hue::Teal),
                                trunc(&h.header, cols),
                            )]),
                            false,
                        ));
                        for dl in &h.lines {
                            let selected = li == self.sel;
                            out.push((diff_line(dl, selected, cols), selected));
                            li += 1;
                        }
                    }
                }
            }
        }
        out
    }

    fn render_composer(&self, surface: &mut Surface, rect: Rect, comp: &Composer) {
        if rect.rows == 0 {
            return;
        }
        let bg = Tok::Slot(S::Bg1);
        // Header rule.
        crate::seg::draw_line(
            surface,
            rect.x,
            rect.y,
            rect.cols,
            &Line::segs(vec![
                seg(Tok::Slot(S::Accent), "▐ "),
                seg(Tok::Slot(S::Text), comp.target.heading()).bold(),
            ]),
            bg,
        );
        // Body: the text field, split into lines, cursor bar on the last.
        let text = comp.field.as_str();
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if let Some(last) = lines.last_mut() {
            last.push('▏');
        }
        let inner_w = rect.cols.saturating_sub(2);
        for row in 1..rect.rows {
            let y = rect.y + row;
            let line = match lines.get(row - 1) {
                Some(s) => Line::segs(vec![sp(1), seg(Tok::Slot(S::Text), trunc(s, inner_w))]),
                None => Line::Blank,
            };
            crate::seg::draw_line(surface, rect.x, y, rect.cols, &line, bg);
        }
    }
}

// --- free helpers ----------------------------------------------------------

fn sel_marker(selected: bool) -> &'static str {
    if selected { "❯ " } else { "  " }
}

fn review_glyph(state: &str) -> &'static str {
    match state.to_uppercase().as_str() {
        "APPROVED" => "✓",
        "CHANGES_REQUESTED" => "✗",
        "DISMISSED" => "⊘",
        _ => "💬",
    }
}

fn review_tone(state: &str) -> Tok {
    match state.to_uppercase().as_str() {
        "APPROVED" => Tok::Hue(thegn_core::theme::Hue::Green),
        "CHANGES_REQUESTED" => Tok::Hue(thegn_core::theme::Hue::Red),
        _ => Tok::Slot(S::Dim),
    }
}

fn file_stat(f: &DiffFile) -> (usize, usize) {
    let mut adds = 0;
    let mut dels = 0;
    for h in &f.hunks {
        for l in &h.lines {
            match l.kind {
                DiffLineKind::Add => adds += 1,
                DiffLineKind::Del => dels += 1,
                DiffLineKind::Context => {}
            }
        }
    }
    (adds, dels)
}

fn diff_line(dl: &DiffLine, selected: bool, cols: usize) -> Line {
    let (marker, tone) = match dl.kind {
        DiffLineKind::Add => ("+", Tok::Hue(thegn_core::theme::Hue::Green)),
        DiffLineKind::Del => ("-", Tok::Hue(thegn_core::theme::Hue::Red)),
        DiffLineKind::Context => (" ", Tok::Slot(S::Dim)),
    };
    let no = dl
        .new_lineno
        .map(|n| format!("{n:>5} "))
        .unwrap_or_else(|| "      ".into());
    let body = trunc(&dl.text, cols.saturating_sub(9));
    Line::segs(vec![
        seg(Tok::Slot(S::Faint), sel_marker(selected)),
        seg(Tok::Slot(S::Ghost3), no),
        seg(tone, format!("{marker}{body}")),
    ])
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Word-wrap `s` to `width` columns (by char count; good enough for ASCII-ish
/// review text). Never returns an empty vec.
fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut line = String::new();
    for word in s.split_whitespace() {
        if line.is_empty() {
            line = word.to_string();
        } else if line.chars().count() + 1 + word.chars().count() <= width {
            line.push(' ');
            line.push_str(word);
        } else {
            out.push(std::mem::take(&mut line));
            line = word.to_string();
        }
        // A single word longer than the width: hard-split it.
        while line.chars().count() > width {
            let head: String = line.chars().take(width).collect();
            out.push(head);
            line = line.chars().skip(width).collect();
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PrView {
        let pr = PrSummary {
            number: 42,
            title: "Add thing".into(),
            state: "OPEN".into(),
            url: "https://github.com/acme/widget/pull/42".into(),
            is_draft: false,
            review_decision: Some("APPROVED".into()),
        };
        let checks = vec![
            CheckLine {
                name: "build".into(),
                state: CheckState::Pass,
                duration_secs: Some(12),
                details_url: Some("https://ci/build".into()),
            },
            CheckLine {
                name: "test".into(),
                state: CheckState::Fail,
                duration_secs: Some(30),
                details_url: None,
            },
        ];
        PrView::open(&pr, &checks, "main", "deadbeef", "MERGEABLE", "CLEAN")
    }

    #[test]
    fn open_derives_owner_repo_and_sha() {
        let v = sample();
        assert_eq!(v.owner, "acme");
        assert_eq!(v.repo, "widget");
        assert_eq!(v.head_sha, "deadbeef");
        assert_eq!(v.tab, PrTab::Overview);
    }

    #[test]
    fn tab_cycles_and_resets_selection() {
        let mut v = sample();
        v.sel = 1;
        assert_eq!(
            v.handle_key(&KeyCode::Tab, Modifiers::NONE),
            PrViewOutcome::Pending
        );
        assert_eq!(v.tab, PrTab::Checks);
        assert_eq!(v.sel, 0);
        // Digit jump.
        v.handle_key(&KeyCode::Char('4'), Modifiers::NONE);
        assert_eq!(v.tab, PrTab::Files);
    }

    #[test]
    fn checks_navigation_and_open_url() {
        let mut v = sample();
        v.switch_tab(PrTab::Checks);
        // Move down to the second check (has no url → Pending).
        v.handle_key(&KeyCode::Char('j'), Modifiers::NONE);
        assert_eq!(v.sel, 1);
        assert_eq!(
            v.handle_key(&KeyCode::Enter, Modifiers::NONE),
            PrViewOutcome::Pending
        );
        // First check has a url → OpenUrl.
        v.handle_key(&KeyCode::Char('k'), Modifiers::NONE);
        assert_eq!(
            v.handle_key(&KeyCode::Enter, Modifiers::NONE),
            PrViewOutcome::Act(PrViewAction::OpenUrl("https://ci/build".into()))
        );
        // 'r' re-runs.
        assert_eq!(
            v.handle_key(&KeyCode::Char('r'), Modifiers::NONE),
            PrViewOutcome::Act(PrViewAction::Rerun)
        );
    }

    #[test]
    fn global_actions_and_composer_flow() {
        let mut v = sample();
        // Merge / approve are immediate.
        assert_eq!(
            v.handle_key(&KeyCode::Char('M'), Modifiers::NONE),
            PrViewOutcome::Act(PrViewAction::Merge)
        );
        assert_eq!(
            v.handle_key(&KeyCode::Char('A'), Modifiers::NONE),
            PrViewOutcome::Act(PrViewAction::Approve)
        );
        // 'c' opens a comment composer; typing then Ctrl-D submits.
        v.handle_key(&KeyCode::Char('c'), Modifiers::NONE);
        assert!(v.composer.is_some());
        for ch in "hi".chars() {
            v.handle_key(&KeyCode::Char(ch), Modifiers::NONE);
        }
        let out = v.handle_key(&KeyCode::Char('d'), Modifiers::CTRL);
        assert_eq!(
            out,
            PrViewOutcome::Act(PrViewAction::Comment { body: "hi".into() })
        );
        assert!(v.composer.is_none());
    }

    #[test]
    fn empty_composer_submits_nothing() {
        let mut v = sample();
        v.handle_key(&KeyCode::Char('c'), Modifiers::NONE);
        let out = v.handle_key(&KeyCode::Char('d'), Modifiers::CTRL);
        assert_eq!(out, PrViewOutcome::Pending);
        assert!(v.status.is_some());
    }

    #[test]
    fn files_expand_and_line_comment() {
        let mut v = sample();
        v.diff = Some(PrDiff {
            files: vec![DiffFile {
                path: "src/x.rs".into(),
                old_path: Some("src/x.rs".into()),
                hunks: vec![thegn_core::github::DiffHunk {
                    header: "@@ -1,2 +1,3 @@".into(),
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Context,
                            text: "ctx".into(),
                            old_lineno: Some(1),
                            new_lineno: Some(1),
                        },
                        DiffLine {
                            kind: DiffLineKind::Add,
                            text: "added".into(),
                            old_lineno: None,
                            new_lineno: Some(2),
                        },
                    ],
                }],
            }],
        });
        v.switch_tab(PrTab::Files);
        // Expand the file.
        v.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert_eq!(v.open_file, Some(0));
        // Select the added line (row 1) and comment on it.
        v.handle_key(&KeyCode::Char('j'), Modifiers::NONE);
        v.handle_key(&KeyCode::Char('c'), Modifiers::NONE);
        assert!(matches!(
            v.composer.as_ref().map(|c| &c.target),
            Some(ComposerTarget::LineComment { line: 2, .. })
        ));
        // Type + submit → a fully-specified line comment.
        for ch in "nit".chars() {
            v.handle_key(&KeyCode::Char(ch), Modifiers::NONE);
        }
        let out = v.handle_key(&KeyCode::Char('d'), Modifiers::CTRL);
        assert_eq!(
            out,
            PrViewOutcome::Act(PrViewAction::LineComment {
                owner: "acme".into(),
                repo: "widget".into(),
                number: 42,
                commit_id: "deadbeef".into(),
                path: "src/x.rs".into(),
                line: 2,
                body: "nit".into(),
            })
        );
        // Esc from the file list closes the view; Esc in a file collapses first.
        v.switch_tab(PrTab::Files);
        v.handle_key(&KeyCode::Enter, Modifiers::NONE);
        assert_eq!(
            v.handle_key(&KeyCode::Escape, Modifiers::NONE),
            PrViewOutcome::Pending
        );
        assert_eq!(v.open_file, None);
        assert_eq!(
            v.handle_key(&KeyCode::Escape, Modifiers::NONE),
            PrViewOutcome::Close
        );
    }

    #[test]
    fn wrap_splits_long_words_and_never_empty() {
        assert_eq!(wrap("a b c", 3), vec!["a b".to_string(), "c".to_string()]);
        assert_eq!(wrap("", 5), vec![String::new()]);
        let w = wrap("supercalifragilistic", 5);
        assert!(w.iter().all(|l| l.chars().count() <= 5));
    }
}
