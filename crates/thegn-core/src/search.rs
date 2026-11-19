//! Scope-aware fuzzy search over pane history buffers.
//!
//! [`SearchEngine`] drives the neo_frizbee SIMD fuzzy matcher (fff's matcher
//! core) over one or more [`HistoryBuffer`]s, selected by [`SearchScope`]. The
//! engine is pure data — it holds query + ranked matches; the host layer owns
//! the overlay UI and feeds the engine sources on each keystroke.

use neo_frizbee::{Config, match_list};
use std::collections::HashMap;

use crate::history::HistoryBuffer;

// ── Scope ─────────────────────────────────────────────────────────────────────

/// Which panes contribute to the search.
///
/// The host layer translates each variant into a concrete list of
/// `(pane_id, label, &HistoryBuffer)` sources by walking the session tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// A single pane's history.
    Pane(u32),
    /// All panes in the active tab.
    Tab,
    /// All tabs in the active worktree.
    Worktree,
    /// All worktrees in the active workspace.
    Workspace,
    /// Every worktree in every open workspace (the entire profile).
    Profile,
}

impl SearchScope {
    /// Display label shown in the overlay scope pill.
    pub fn label(self) -> &'static str {
        match self {
            SearchScope::Pane(_) => "pane",
            SearchScope::Tab => "tab",
            SearchScope::Worktree => "worktree",
            SearchScope::Workspace => "workspace",
            SearchScope::Profile => "profile",
        }
    }

    /// Cycle to the next wider scope (wraps from Workspace → Pane). `Profile`
    /// is not part of the cycle (the variant stays; a stray one re-enters at Pane).
    pub fn widen(self, pane_id: u32) -> SearchScope {
        match self {
            SearchScope::Pane(_) => SearchScope::Tab,
            SearchScope::Tab => SearchScope::Worktree,
            SearchScope::Worktree => SearchScope::Workspace,
            SearchScope::Workspace | SearchScope::Profile => SearchScope::Pane(pane_id),
        }
    }

    /// Cycle to the next narrower scope (wraps from Pane → Workspace).
    pub fn narrow(self, pane_id: u32) -> SearchScope {
        match self {
            SearchScope::Pane(_) => SearchScope::Workspace,
            SearchScope::Tab => SearchScope::Pane(pane_id),
            SearchScope::Worktree => SearchScope::Tab,
            SearchScope::Workspace => SearchScope::Worktree,
            SearchScope::Profile => SearchScope::Workspace,
        }
    }
}

// ── Match ─────────────────────────────────────────────────────────────────────

/// A single line that matched the current query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    /// The pane that owns this line.
    pub pane_id: u32,
    /// Index within that pane's `HistoryBuffer` (0 = oldest surviving line).
    pub line_idx: usize,
    /// The plain-text line content (already ANSI-stripped by the buffer).
    pub line: String,
    /// Fuzzy match score (higher = better match). `0` when query is empty.
    pub score: u32,
    /// Human-readable pane label (e.g. `"tab 2 · feat/auth"`) for the result row.
    pub pane_label: String,
    /// Byte range of the hit in `line` (Literal); `None` for Fuzzy / empty query.
    pub span: Option<(usize, usize)>,
    /// Lines between this line and its source's newest line (0 = newest).
    pub from_tail: usize,
    /// Number of identical lines grouped into this result (1 = single line).
    pub repeat: u32,
}

/// How the query is matched against lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// Smart-case substring match (the default).
    #[default]
    Literal,
    /// Fuzzy subsequence match (neo_frizbee).
    Fuzzy,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Smart-case literal search: case-sensitive iff `needle` has an uppercase
/// char, ASCII case-insensitive otherwise. Byte-window scan, no allocation.
/// Returns the byte range of the first hit.
pub fn find_literal(line: &str, needle: &str) -> Option<(usize, usize)> {
    let (h, n) = (line.as_bytes(), needle.as_bytes());
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    let sensitive = needle.chars().any(char::is_uppercase);
    (0..=h.len() - n.len()).find_map(|i| {
        if !line.is_char_boundary(i) {
            return None;
        }
        let w = &h[i..i + n.len()];
        let hit = if sensitive {
            w == n
        } else {
            w.eq_ignore_ascii_case(n)
        };
        (hit && line.is_char_boundary(i + n.len())).then_some((i, i + n.len()))
    })
}

fn on_word_boundaries(line: &str, (s, e): (usize, usize)) -> bool {
    let b = line.as_bytes();
    (s == 0 || !is_word_byte(b[s - 1])) && (e >= b.len() || !is_word_byte(b[e]))
}

// ── Engine ────────────────────────────────────────────────────────────────────

/// A source the engine searches: `(pane_id, label, history_buffer)`.
/// The label is whatever the host wants to display (tab name, worktree path, …).
pub type SearchSource<'a> = (u32, &'a str, &'a HistoryBuffer);

type RankedSearchHit = (usize, u32, Option<(usize, usize)>);

/// Fuzzy search engine over one or more pane histories.
///
/// Call [`SearchEngine::set_query`] (or the incremental `push_char`/`backspace`)
/// after collecting sources; the engine re-scores synchronously. For the default
/// `max_results = 1000`, scoring a single pane's history takes ~1–3 ms on modern
/// hardware (nucleo is SIMD-accelerated).
pub struct SearchEngine {
    query: String,
    pub scope: SearchScope,
    matches: Vec<SearchMatch>,
    selected: usize,
    max_results: usize,
    mode: MatchMode,
}

impl SearchEngine {
    pub fn new(scope: SearchScope, max_results: usize) -> Self {
        SearchEngine {
            query: String::new(),
            scope,
            matches: Vec::new(),
            selected: 0,
            max_results: max_results.max(1),
            mode: MatchMode::default(),
        }
    }

    pub fn mode(&self) -> MatchMode {
        self.mode
    }

    /// Switch match mode; re-scores against `sources`.
    pub fn set_mode(&mut self, m: MatchMode, sources: &[SearchSource<'_>]) {
        self.mode = m;
        self.selected = 0;
        self.recompute(sources);
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn scope(&self) -> SearchScope {
        self.scope
    }

    pub fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    pub fn selected_idx(&self) -> usize {
        self.selected
    }

    pub fn selected(&self) -> Option<&SearchMatch> {
        self.matches.get(self.selected)
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
        }
    }

    /// Replace the query and re-score. `sources` must reflect the current `scope`.
    pub fn set_query(&mut self, q: &str, sources: &[SearchSource<'_>]) {
        self.query = q.to_string();
        self.selected = 0;
        self.recompute(sources);
    }

    /// Append one character and re-score incrementally.
    pub fn push_char(&mut self, c: char, sources: &[SearchSource<'_>]) {
        self.query.push(c);
        self.selected = 0;
        self.recompute(sources);
    }

    /// Delete the last character and re-score.
    pub fn backspace(&mut self, sources: &[SearchSource<'_>]) {
        self.query.pop();
        self.selected = 0;
        self.recompute(sources);
    }

    /// Change scope without changing the query; re-score against new sources.
    pub fn set_scope(&mut self, scope: SearchScope, sources: &[SearchSource<'_>]) {
        self.scope = scope;
        self.selected = 0;
        self.recompute(sources);
    }

    fn recompute(&mut self, sources: &[SearchSource<'_>]) {
        self.matches.clear();
        let trimmed = self.query.trim().to_string();
        let empty = trimmed.is_empty();
        let mode = self.mode;

        // Flatten every source line, keeping back-references.
        // (pane_id, idx, line, label, from_tail)
        let flat: Vec<(u32, usize, &str, &str, usize)> = sources
            .iter()
            .flat_map(|&(pane_id, label, buf)| {
                let n = buf.len();
                buf.iter()
                    .enumerate()
                    .map(move |(idx, line)| (pane_id, idx, line, label, n - 1 - idx))
            })
            .collect();

        let mk = |f: &(u32, usize, &str, &str, usize), score: u32, span, repeat| SearchMatch {
            pane_id: f.0,
            line_idx: f.1,
            line: f.2.to_string(),
            score,
            pane_label: f.3.to_string(),
            span,
            from_tail: f.4,
            repeat,
        };

        let mut hits: Vec<RankedSearchHit> = Vec::new();
        if empty {
            hits.extend(flat.iter().enumerate().map(|(i, _)| (i, 0, None)));
        } else {
            match mode {
                MatchMode::Literal => {
                    for (i, f) in flat.iter().enumerate() {
                        if let Some(span) = find_literal(f.2, &trimmed) {
                            hits.push((
                                i,
                                if on_word_boundaries(f.2, span) { 2 } else { 1 },
                                Some(span),
                            ));
                        }
                    }
                }
                MatchMode::Fuzzy => {
                    let hay: Vec<&str> = flat.iter().map(|f| f.2).collect();
                    for m in match_list(&trimmed, &hay, &Config::default()) {
                        hits.push((m.index as usize, m.score as u32, None));
                    }
                }
            }
        }
        hits.sort_by(|a, b| b.1.cmp(&a.1).then(flat[a.0].4.cmp(&flat[b.0].4)));
        let mut groups: HashMap<(u32, &str), usize> = HashMap::with_capacity(hits.len());
        let mut grouped: Vec<(usize, u32, Option<(usize, usize)>, u32)> =
            Vec::with_capacity(hits.len());
        for (i, score, span) in hits {
            let key = (flat[i].0, flat[i].2);
            if let Some(&group_idx) = groups.get(&key) {
                grouped[group_idx].3 += 1;
                if flat[i].4 < flat[grouped[group_idx].0].4 {
                    grouped[group_idx].0 = i;
                    grouped[group_idx].1 = score;
                    grouped[group_idx].2 = span;
                }
            } else {
                groups.insert(key, grouped.len());
                grouped.push((i, score, span, 1));
            }
        }
        grouped.sort_by(|a, b| b.1.cmp(&a.1).then(flat[a.0].4.cmp(&flat[b.0].4)));
        grouped.truncate(self.max_results);
        self.matches = grouped
            .into_iter()
            .map(|(i, score, span, repeat)| mk(&flat[i], score, span, repeat))
            .collect();
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryBuffer;

    fn buf_from(lines: &[&str]) -> HistoryBuffer {
        let mut b = HistoryBuffer::new(10_000);
        for &l in lines {
            b.push_line(l.to_string());
        }
        b
    }

    fn engine(scope: SearchScope) -> SearchEngine {
        SearchEngine::new(scope, 1_000)
    }

    // ── scope cycling ─────────────────────────────────────────────────────────

    #[test]
    fn scope_widen_cycles_forward() {
        let id = 1;
        assert_eq!(SearchScope::Pane(id).widen(id), SearchScope::Tab);
        assert_eq!(SearchScope::Tab.widen(id), SearchScope::Worktree);
        assert_eq!(SearchScope::Worktree.widen(id), SearchScope::Workspace);
        // Profile is skipped: Workspace wraps to Pane.
        assert_eq!(SearchScope::Workspace.widen(id), SearchScope::Pane(id));
    }

    #[test]
    fn scope_narrow_cycles_backward() {
        let id = 1;
        assert_eq!(SearchScope::Tab.narrow(id), SearchScope::Pane(id));
        assert_eq!(SearchScope::Worktree.narrow(id), SearchScope::Tab);
        assert_eq!(SearchScope::Workspace.narrow(id), SearchScope::Worktree);
        assert_eq!(SearchScope::Pane(id).narrow(id), SearchScope::Workspace);
    }

    #[test]
    fn scope_labels_are_non_empty() {
        for scope in [
            SearchScope::Pane(0),
            SearchScope::Tab,
            SearchScope::Worktree,
            SearchScope::Workspace,
            SearchScope::Profile,
        ] {
            assert!(!scope.label().is_empty());
        }
    }

    // ── empty query ───────────────────────────────────────────────────────────

    #[test]
    fn empty_query_returns_recent_lines_newest_first() {
        let buf = buf_from(&["oldest", "middle", "newest"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("", &sources);
        let lines: Vec<_> = eng.matches().iter().map(|m| m.line.as_str()).collect();
        assert_eq!(lines, ["newest", "middle", "oldest"]);
    }

    #[test]
    fn empty_query_with_whitespace_treated_as_empty() {
        let buf = buf_from(&["a", "b"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("   ", &sources);
        // Whitespace-only → treated as empty → recent lines.
        assert_eq!(eng.matches().len(), 2);
        assert_eq!(eng.matches()[0].score, 0);
    }

    #[test]
    fn identical_lines_from_one_pane_group_with_a_count() {
        let buf = buf_from(&["same", "other", "same"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("same", &sources);
        assert_eq!(eng.matches().len(), 1);
        assert_eq!(eng.matches()[0].repeat, 2);
    }

    #[test]
    fn identical_lines_from_two_panes_do_not_group() {
        let a = buf_from(&["same"]);
        let b = buf_from(&["same"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "a", &a), (2, "b", &b)];
        let mut eng = engine(SearchScope::Worktree);
        eng.set_query("same", &sources);
        assert_eq!(eng.matches().len(), 2);
        assert!(eng.matches().iter().all(|m| m.repeat == 1));
    }

    #[test]
    fn group_keeps_the_newest_occurrence() {
        let buf = buf_from(&["same", "other", "same"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("same", &sources);
        assert_eq!(eng.matches()[0].line_idx, 2);
        assert_eq!(eng.matches()[0].from_tail, 0);
    }

    #[test]
    fn empty_query_groups_spinner_repeats() {
        let buf = buf_from(&["⠋", "⠙", "⠋", "⠋"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("", &sources);
        let spinner = eng.matches().iter().find(|m| m.line == "⠋").unwrap();
        assert_eq!(spinner.repeat, 3);
        assert_eq!(spinner.from_tail, 0);
    }

    // ── fuzzy matching ────────────────────────────────────────────────────────

    #[test]
    fn fuzzy_query_scores_and_ranks_matches() {
        let buf = buf_from(&["cargo build --release", "cargo test", "not a match"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("cargo", &sources);
        let lines: Vec<_> = eng.matches().iter().map(|m| m.line.as_str()).collect();
        // Both cargo lines should match; "not a match" should not.
        assert!(lines.iter().all(|l| l.contains("cargo")));
        assert!(!lines.contains(&"not a match"));
    }

    #[test]
    fn case_insensitive_match() {
        let buf = buf_from(&["Error: file not found", "warning: unused variable"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("error", &sources);
        assert_eq!(eng.matches().len(), 1);
        assert!(eng.matches()[0].line.contains("Error"));
    }

    #[test]
    fn no_query_match_returns_empty_results_for_fuzzy() {
        let buf = buf_from(&["hello world"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("zzzzz_no_match", &sources);
        assert!(eng.matches().is_empty());
    }

    // ── multi-pane ────────────────────────────────────────────────────────────

    #[test]
    fn multi_pane_results_include_pane_label() {
        let buf_a = buf_from(&["build success"]);
        let buf_b = buf_from(&["build failed"]);
        let sources: Vec<SearchSource<'_>> =
            vec![(1, "tab 1 · main", &buf_a), (2, "tab 2 · feat", &buf_b)];
        let mut eng = engine(SearchScope::Worktree);
        eng.set_query("build", &sources);
        assert_eq!(eng.matches().len(), 2);
        let labels: Vec<_> = eng
            .matches()
            .iter()
            .map(|m| m.pane_label.as_str())
            .collect();
        assert!(labels.contains(&"tab 1 · main"));
        assert!(labels.contains(&"tab 2 · feat"));
    }

    #[test]
    fn results_capped_at_max_results() {
        let lines: Vec<String> = (0..200).map(|i| format!("match line {i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let buf = buf_from(&refs);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = SearchEngine::new(SearchScope::Pane(1), 50);
        eng.set_query("match", &sources);
        assert_eq!(eng.matches().len(), 50);
    }

    #[test]
    fn max_results_one_never_panics() {
        let buf = buf_from(&["a", "b", "c"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = SearchEngine::new(SearchScope::Pane(1), 1);
        eng.set_query("a", &sources);
        assert!(eng.matches().len() <= 1);
    }

    #[test]
    fn scope_pane_filters_to_single_pane_via_sources() {
        // The host provides only the relevant pane's buffer for Pane scope.
        let buf_a = buf_from(&["only from pane a"]);
        let sources: Vec<SearchSource<'_>> = vec![(7, "pane a", &buf_a)];
        let mut eng = engine(SearchScope::Pane(7));
        eng.set_query("pane", &sources);
        assert!(eng.matches().iter().all(|m| m.pane_id == 7));
    }

    // ── navigation ────────────────────────────────────────────────────────────

    #[test]
    fn move_up_down_clamps() {
        let buf = buf_from(&["a", "b", "c"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("", &sources);
        assert_eq!(eng.selected_idx(), 0);
        eng.move_up(); // clamp at 0
        assert_eq!(eng.selected_idx(), 0);
        eng.move_down();
        assert_eq!(eng.selected_idx(), 1);
        eng.move_down();
        assert_eq!(eng.selected_idx(), 2);
        eng.move_down(); // clamp at len-1
        assert_eq!(eng.selected_idx(), 2);
    }

    #[test]
    fn move_down_on_empty_results_does_not_panic() {
        let buf = buf_from(&[]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("anything", &sources);
        eng.move_down(); // no panic
        assert_eq!(eng.selected_idx(), 0);
    }

    #[test]
    fn push_char_and_backspace_are_incremental() {
        let buf = buf_from(&["cargo build", "cargo test", "ninja"]);
        let sources: Vec<SearchSource<'_>> = vec![(1, "pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.push_char('c', &sources);
        eng.push_char('a', &sources);
        eng.push_char('r', &sources);
        assert_eq!(eng.query(), "car");
        assert!(
            eng.matches()
                .iter()
                .all(|m| m.line.to_lowercase().contains('c'))
        );
        eng.backspace(&sources);
        assert_eq!(eng.query(), "ca");
    }

    // ── set_scope ─────────────────────────────────────────────────────────────

    #[test]
    fn set_scope_recomputes_against_new_sources() {
        let buf_a = buf_from(&["pane a output"]);
        let buf_b = buf_from(&["pane b output"]);
        let sources_a: Vec<SearchSource<'_>> = vec![(1, "a", &buf_a)];
        let sources_b: Vec<SearchSource<'_>> = vec![(2, "b", &buf_b)];

        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("pane", &sources_a);
        assert_eq!(eng.matches().len(), 1);
        assert_eq!(eng.matches()[0].pane_id, 1);

        eng.set_scope(SearchScope::Tab, &sources_b);
        assert_eq!(eng.matches().len(), 1);
        assert_eq!(eng.matches()[0].pane_id, 2);
    }

    // ── empty buffer ──────────────────────────────────────────────────────────

    #[test]
    fn empty_history_returns_zero_results_without_panic() {
        let buf = HistoryBuffer::new(1_000);
        let sources: Vec<SearchSource<'_>> = vec![(1, "empty pane", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("anything", &sources);
        assert!(eng.matches().is_empty());
        assert!(eng.selected().is_none());
    }

    // ── literal / ranking ─────────────────────────────────────────────────────

    fn lines_of(eng: &SearchEngine) -> Vec<&str> {
        eng.matches().iter().map(|m| m.line.as_str()).collect()
    }

    #[test]
    fn literal_word_boundary_beats_embedded_hit() {
        let buf = buf_from(&["b158_docs_then_gate.sh", "ssh host"]);
        let srcs: Vec<SearchSource<'_>> = vec![(1, "p", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("ssh", &srcs);
        assert_eq!(lines_of(&eng), ["ssh host"]);
        assert_eq!(eng.matches()[0].span, Some((0, 3)));
        assert_eq!(eng.matches()[0].score, 2);
    }

    #[test]
    fn literal_smart_case() {
        let buf = buf_from(&["ssh host", "SSH host"]);
        let srcs: Vec<SearchSource<'_>> = vec![(1, "p", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("SSH", &srcs);
        assert_eq!(lines_of(&eng), ["SSH host"]);
        eng.set_query("ssh", &srcs);
        assert_eq!(eng.matches().len(), 2);
    }

    #[test]
    fn equal_scores_newer_first() {
        let buf = buf_from(&["ssh one", "ssh two"]);
        let srcs: Vec<SearchSource<'_>> = vec![(1, "p", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("ssh", &srcs);
        assert_eq!(lines_of(&eng), ["ssh two", "ssh one"]);
        assert_eq!(eng.matches()[0].from_tail, 0);
    }

    #[test]
    fn fuzzy_mode_restores_subsequence_hits() {
        let buf = buf_from(&["cargo build"]);
        let srcs: Vec<SearchSource<'_>> = vec![(1, "p", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("cgbld", &srcs);
        assert!(eng.matches().is_empty());
        eng.set_mode(MatchMode::Fuzzy, &srcs);
        assert_eq!(eng.mode(), MatchMode::Fuzzy);
        assert_eq!(eng.matches().len(), 1);
        assert_eq!(eng.matches()[0].span, None);
    }

    #[test]
    fn empty_query_merges_sources_by_from_tail() {
        let a = buf_from(&["a0", "a1", "a2"]);
        let b = buf_from(&["b0"]);
        let srcs: Vec<SearchSource<'_>> = vec![(1, "a", &a), (2, "b", &b)];
        let mut eng = engine(SearchScope::Worktree);
        eng.set_query("", &srcs);
        assert_eq!(lines_of(&eng), ["a2", "b0", "a1", "a0"]);
    }

    #[test]
    fn scope_cycle_has_no_profile() {
        let mut s = SearchScope::Pane(1);
        for _ in 0..8 {
            s = s.widen(1);
            assert_ne!(s, SearchScope::Profile);
        }
        let mut s = SearchScope::Pane(1);
        for _ in 0..8 {
            s = s.narrow(1);
            assert_ne!(s, SearchScope::Profile);
        }
    }

    #[test]
    fn ten_thousand_lines_complete() {
        let owned: Vec<String> = (0..10_000).map(|i| format!("line {i} ssh host")).collect();
        let refs: Vec<&str> = owned.iter().map(String::as_str).collect();
        let buf = buf_from(&refs);
        let srcs: Vec<SearchSource<'_>> = vec![(1, "p", &buf)];
        let mut eng = engine(SearchScope::Pane(1));
        eng.set_query("ssh", &srcs);
        assert_eq!(eng.matches().len(), 1_000);
    }
}
