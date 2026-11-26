//! The grouped, foldable model behind the unified Notifications overlay.
//!
//! The overlay used to be one flat list: a hundred stale "agent needs input"
//! rows from old pipeline lanes buried everything live. This module folds those
//! rows: by kind (needs input, failed, finished, info), then by source (a
//! pipeline batch, or the project a worktree lives in), with anything older
//! than a day parked in a collapsed "Older" group at the bottom.
//!
//! `layout` is the pure part (leaves + expanded set + filter in, ordered
//! entries out) so the grouping and ordering are unit-testable without a
//! terminal. `NotifTree` and the `impl DetailOverlay` below drive it from key
//! presses and rebuild the overlay's row list.

use std::collections::{BTreeMap, HashSet};

use termwiz::input::KeyCode;

use super::{DetailAction, DetailContent, DetailOutcome, DetailOverlay, DetailRow};
use crate::chrome::S;
use crate::seg::Tok;
use thegn_core::theme::Hue;

/// An item older than this is "stale": it leaves the live groups (and the
/// badge count) and folds into the collapsed "Older" group.
pub(crate) const STALE_AFTER_SECS: i64 = 24 * 60 * 60;

/// Key of the collapsed "Older" group.
const OLDER_KEY: &str = "older";

/// Whether a signal first raised at `ts` (epoch seconds) is stale at `now`.
/// A signal with no timestamp is never stale.
pub(crate) fn is_stale(ts: Option<i64>, now: i64) -> bool {
    ts.is_some_and(|t| now.saturating_sub(t) > STALE_AFTER_SECS)
}

/// What a row asks of the user, most urgent first (the derived order is the
/// display order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum NKind {
    NeedsInput,
    Failed,
    Finished,
    Info,
}

impl NKind {
    pub(crate) const ALL: [NKind; 4] = [
        NKind::NeedsInput,
        NKind::Failed,
        NKind::Finished,
        NKind::Info,
    ];

    /// Lower-case phrase for the count line and group rows ("23 need input").
    fn phrase(self) -> &'static str {
        match self {
            NKind::NeedsInput => "need input",
            NKind::Failed => "failed",
            NKind::Finished => "finished",
            NKind::Info => "info",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            NKind::NeedsInput => "Needs input",
            NKind::Failed => "Failed",
            NKind::Finished => "Finished",
            NKind::Info => "Info",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            NKind::NeedsInput => "needs",
            NKind::Failed => "failed",
            NKind::Finished => "finished",
            NKind::Info => "info",
        }
    }

    fn hue(self) -> Hue {
        match self {
            NKind::NeedsInput | NKind::Failed => Hue::Red,
            NKind::Finished => Hue::Amber,
            NKind::Info => Hue::Blue,
        }
    }
}

/// The identity of one needs-you acknowledgement (what `x` on an attention row
/// carries), so a group dismissal can ack many worktrees in one action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckTarget {
    pub path: String,
    pub reason: thegn_core::attention::AttentionReason,
    pub since: Option<i64>,
    pub episode: thegn_core::attention::Episode,
}

/// One notification or needs-you item, before grouping.
#[derive(Clone)]
pub(crate) struct Leaf {
    /// Unique, stable identity (`a:<path>` / `n:<id>`).
    pub key: String,
    pub kind: NKind,
    /// Fold key ("Pipeline b313", a project name); empty never folds.
    pub source: String,
    /// When it was raised (epoch seconds), for ordering and staleness.
    pub ts: Option<i64>,
    pub stale: bool,
    /// Lower-cased text the `/` filter matches against.
    pub search: String,
    pub row: DetailRow,
    /// Inbox id to mark read on dismiss.
    pub id: Option<i64>,
    /// Needs-you ack to record on dismiss.
    pub ack: Option<AckTarget>,
}

/// The fold key for a worktree: its pipeline batch (`b313-the-2147` →
/// "Pipeline b313"), else the project directory the worktree lives under, else
/// empty (never folds).
pub(crate) fn source_key(worktree_path: &str) -> String {
    if worktree_path.is_empty() {
        return String::new();
    }
    let path = std::path::Path::new(worktree_path);
    let base = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if let Some(batch) = batch_prefix(&base) {
        return format!("Pipeline {batch}");
    }
    path.parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `b313-the-2147` / `b313` → `Some("b313")`; anything else `None`.
fn batch_prefix(base: &str) -> Option<&str> {
    let rest = base.strip_prefix('b')?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    match rest.as_bytes().get(digits) {
        None | Some(b'-') => Some(&base[..=digits]),
        _ => None,
    }
}

/// One entry of the ordered layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    /// The per-kind count line.
    Summary(String),
    /// A kind heading with its live item count.
    KindHeader(NKind, usize),
    /// A folded group of `members` (leaf indices, newest first).
    Group {
        key: String,
        kind: Option<NKind>,
        source: String,
        members: Vec<usize>,
        expanded: bool,
    },
    /// One item; `group` is the owning group's key when shown inside one.
    Leaf { idx: usize, group: Option<String> },
    /// A fixed row after the tree (merge queue, other repos, logs), by index.
    Static(usize),
}

/// The "N need input · N failed" count line over the *visible* leaves.
fn summary(leaves: &[Leaf], vis: &[usize]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for kind in NKind::ALL {
        let n = vis
            .iter()
            .filter(|&&i| !leaves[i].stale && leaves[i].kind == kind)
            .count();
        if n > 0 {
            parts.push(format!("{n} {}", kind.phrase()));
        }
    }
    let old = vis.iter().filter(|&&i| leaves[i].stale).count();
    if old > 0 {
        parts.push(format!("{old} older"));
    }
    (!parts.is_empty()).then(|| parts.join(" \u{b7} "))
}

fn newest_first(leaves: &[Leaf], members: &mut [usize]) {
    members.sort_by_key(|&i| std::cmp::Reverse(leaves[i].ts.unwrap_or(i64::MIN)));
}

/// Order `leaves` into entries: count line, then per kind (urgent first) the
/// live items newest-source-first with same-source runs of 2+ folded into a
/// group, then the collapsed "Older" group. `filter` is a case-insensitive
/// substring; while one is set every group is shown expanded so matches are
/// visible. Pure.
pub(crate) fn layout(leaves: &[Leaf], expanded: &HashSet<String>, filter: &str) -> Vec<Entry> {
    let needle = filter.trim().to_lowercase();
    let filtering = !needle.is_empty();
    let vis: Vec<usize> = (0..leaves.len())
        .filter(|&i| !filtering || leaves[i].search.contains(&needle))
        .collect();
    let Some(sum) = summary(leaves, &vis) else {
        return Vec::new();
    };
    let mut out = vec![Entry::Summary(sum)];
    for kind in NKind::ALL {
        let live: Vec<usize> = vis
            .iter()
            .copied()
            .filter(|&i| !leaves[i].stale && leaves[i].kind == kind)
            .collect();
        if live.is_empty() {
            continue;
        }
        out.push(Entry::KindHeader(kind, live.len()));
        // Items (a source's members, or one loose leaf) with their newest ts.
        let mut by_source: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        let mut items: Vec<(i64, Vec<usize>, Option<&str>)> = Vec::new();
        for &i in &live {
            let src = leaves[i].source.as_str();
            if src.is_empty() {
                items.push((leaves[i].ts.unwrap_or(i64::MIN), vec![i], None));
            } else {
                by_source.entry(src).or_default().push(i);
            }
        }
        for (src, mut members) in by_source {
            newest_first(leaves, &mut members);
            let newest = leaves[members[0]].ts.unwrap_or(i64::MIN);
            items.push((newest, members, Some(src)));
        }
        items.sort_by_key(|(newest, _, _)| std::cmp::Reverse(*newest));
        for (_, members, src) in items {
            match src {
                Some(source) if members.len() > 1 => {
                    let key = format!("{}/{source}", kind.slug());
                    let open = filtering || expanded.contains(&key);
                    out.push(Entry::Group {
                        key: key.clone(),
                        kind: Some(kind),
                        source: source.to_string(),
                        members: members.clone(),
                        expanded: open,
                    });
                    if open {
                        out.extend(members.into_iter().map(|idx| Entry::Leaf {
                            idx,
                            group: Some(key.clone()),
                        }));
                    }
                }
                _ => out.extend(
                    members
                        .into_iter()
                        .map(|idx| Entry::Leaf { idx, group: None }),
                ),
            }
        }
    }
    let mut old: Vec<usize> = vis.iter().copied().filter(|&i| leaves[i].stale).collect();
    if !old.is_empty() {
        newest_first(leaves, &mut old);
        let open = filtering || expanded.contains(OLDER_KEY);
        out.push(Entry::Group {
            key: OLDER_KEY.into(),
            kind: None,
            source: String::new(),
            members: old.clone(),
            expanded: open,
        });
        if open {
            out.extend(old.into_iter().map(|idx| Entry::Leaf {
                idx,
                group: Some(OLDER_KEY.into()),
            }));
        }
    }
    out
}

/// The dismissal covering `members` of `leaves`.
fn dismissal(leaves: &[Leaf], members: &[usize]) -> DetailAction {
    DetailAction::DismissBatch {
        ids: members.iter().filter_map(|&i| leaves[i].id).collect(),
        acks: members
            .iter()
            .filter_map(|&i| leaves[i].ack.clone())
            .collect(),
        keys: members.iter().map(|&i| leaves[i].key.clone()).collect(),
    }
}

/// Footer keys. `a` (clear all) and the per-row `l`/`r`/`o` actions still work
/// on the rows that carry them; the footer names the fold/dismiss/filter keys
/// this view adds plus the one total clear.
const BASE_HINT: &str = "\u{21b5}/\u{2192} open \u{b7} \u{2190} fold \u{b7} x dismiss \u{b7} D stale \u{b7} / filter \u{b7} a clear all";

/// Live state of the grouped overlay: the items, which groups are open, the
/// filter, and the fixed rows after the tree.
pub(crate) struct NotifTree {
    leaves: Vec<Leaf>,
    expanded: HashSet<String>,
    filter: String,
    editing: bool,
    tail: Vec<DetailRow>,
    /// Parallel to the overlay's rows.
    entries: Vec<Entry>,
}

impl NotifTree {
    pub(crate) fn new(leaves: Vec<Leaf>, tail: Vec<DetailRow>) -> NotifTree {
        NotifTree {
            leaves,
            expanded: HashSet::new(),
            filter: String::new(),
            editing: false,
            tail,
            entries: Vec::new(),
        }
    }

    /// Rows + parallel entries for the current state.
    fn build(&self) -> (Vec<DetailRow>, Vec<Entry>) {
        let mut entries = layout(&self.leaves, &self.expanded, &self.filter);
        entries.extend((0..self.tail.len()).map(Entry::Static));
        let rows = entries.iter().map(|e| self.row_for(e)).collect();
        (rows, entries)
    }

    fn row_for(&self, e: &Entry) -> DetailRow {
        match e {
            Entry::Summary(s) => DetailRow::header(s.clone()),
            Entry::KindHeader(k, n) => DetailRow::header(format!("{} ({n})", k.heading())),
            Entry::Group {
                kind,
                source,
                members,
                expanded,
                ..
            } => {
                let n = members.len();
                let (tone, title) = match kind {
                    Some(k) => (
                        Tok::Hue(k.hue()),
                        format!("{source} \u{2014} {n} {}", k.phrase()),
                    ),
                    None => (Tok::Slot(S::Dim), format!("Older ({n})")),
                };
                let g = crate::caps::active_glyphs();
                let glyph = if *expanded {
                    g.caret_open
                } else {
                    g.caret_closed
                };
                let newest = members.iter().filter_map(|&i| self.leaves[i].ts).max();
                let mut row = DetailRow::new(tone, glyph, title)
                    .action('x', dismissal(&self.leaves, members))
                    .action('a', DetailAction::AckAllAttention);
                if let Some(t) = newest {
                    row = row.note(format!("{} ago", thegn_core::util::age(t)));
                }
                row
            }
            Entry::Leaf { idx, group } => {
                let mut row = self.leaves[*idx].row.clone();
                if group.is_some() {
                    row.text = format!("  {}", row.text);
                }
                row
            }
            Entry::Static(j) => self.tail[*j].clone(),
        }
    }

    fn hint(&self) -> String {
        if self.editing {
            format!("/{}_ \u{b7} \u{21b5} keep \u{b7} Esc clear", self.filter)
        } else if !self.filter.is_empty() {
            format!(
                "filter \"{}\" \u{b7} / edit \u{b7} Esc clear \u{b7} x dismiss \u{b7} a clear all",
                self.filter
            )
        } else {
            BASE_HINT.to_string()
        }
    }

    fn entry_id(&self, e: &Entry) -> Option<String> {
        match e {
            Entry::Group { key, .. } => Some(format!("g:{key}")),
            // `idx` may predate a removal (the cursor anchor is taken from the
            // previous entries after the leaves were already pruned), so an
            // index past the end is "no anchor", not a panic.
            Entry::Leaf { idx, .. } => self.leaves.get(*idx).map(|l| format!("l:{}", l.key)),
            Entry::Static(j) => Some(format!("s:{j}")),
            _ => None,
        }
    }

    /// Indices of every stale leaf (what `D` dismisses).
    fn stale_members(&self) -> Vec<usize> {
        (0..self.leaves.len())
            .filter(|&i| self.leaves[i].stale)
            .collect()
    }
}

impl DetailOverlay {
    /// Build the grouped overlay over `leaves`, with `tail` rows kept after the
    /// tree. `cols`/`height` size the box like `super::list`.
    pub(super) fn notif_overlay(
        title: &str,
        leaves: Vec<Leaf>,
        tail: Vec<DetailRow>,
        cols: usize,
        height: usize,
    ) -> DetailOverlay {
        let mut ov = super::list(title, Vec::new(), "all clear", cols, height);
        ov.notif = Some(NotifTree::new(leaves, tail));
        ov.rebuild_notif();
        ov
    }

    /// Regenerate the visible rows from the tree, keeping the cursor on the
    /// same entry when it still exists (else the nearest selectable row).
    fn rebuild_notif(&mut self) {
        let Some(t) = self.notif.as_mut() else {
            return;
        };
        let anchor = t.entries.get(self.sel).and_then(|e| t.entry_id(e));
        let (rows, entries) = t.build();
        let found = anchor.and_then(|a| {
            entries
                .iter()
                .position(|e| t.entry_id(e).as_deref() == Some(a.as_str()))
        });
        t.entries = entries;
        let hint = t.hint();
        let want = found.unwrap_or(self.sel);
        let DetailContent::List(l) = &mut self.content else {
            return;
        };
        l.rows = rows;
        self.hint = Some(hint);
        let idx = self.selectable_indices();
        self.sel = idx
            .iter()
            .copied()
            .find(|&i| i >= want)
            .or_else(|| idx.last().copied())
            .unwrap_or(0);
        self.scroll = self.scroll.min(self.scroll_max());
        self.scroll_to_sel();
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.notif.as_ref()?.entries.get(self.sel)
    }

    /// Key handling for the grouped list; `None` means "not mine, fall through
    /// to the generic list keys". Called after the Ctrl/Alt guards.
    pub(super) fn notif_key(&mut self, key: &KeyCode) -> Option<DetailOutcome> {
        // Only while the grouped list itself is showing (an in-place drill
        // swaps the content for its own keys).
        if !matches!(self.content, DetailContent::List(_)) {
            return None;
        }
        let editing = self.notif.as_ref()?.editing;
        if editing {
            let t = self.notif.as_mut()?;
            match key {
                KeyCode::Enter => t.editing = false,
                k if crate::input::is_escape_key(k) => {
                    t.editing = false;
                    t.filter.clear();
                }
                KeyCode::Backspace => {
                    t.filter.pop();
                }
                KeyCode::Char(c) => t.filter.push(*c),
                _ => {}
            }
            self.rebuild_notif();
            return Some(DetailOutcome::Pending);
        }
        match key {
            KeyCode::Char('/') => {
                self.notif.as_mut()?.editing = true;
                self.rebuild_notif();
                Some(DetailOutcome::Pending)
            }
            k if crate::input::is_escape_key(k) && !self.notif.as_ref()?.filter.is_empty() => {
                self.notif.as_mut()?.filter.clear();
                self.rebuild_notif();
                Some(DetailOutcome::Pending)
            }
            KeyCode::Char('D') => {
                let t = self.notif.as_ref()?;
                let stale = t.stale_members();
                Some(if stale.is_empty() {
                    DetailOutcome::Pending
                } else {
                    DetailOutcome::Act(dismissal(&t.leaves, &stale))
                })
            }
            KeyCode::Enter | KeyCode::RightArrow => {
                let Some(Entry::Group {
                    key: g, expanded, ..
                }) = self.selected_entry().cloned()
                else {
                    return None;
                };
                let t = self.notif.as_mut()?;
                if !expanded {
                    t.expanded.insert(g);
                } else if key == &KeyCode::Enter {
                    t.expanded.remove(&g);
                }
                self.rebuild_notif();
                Some(DetailOutcome::Pending)
            }
            KeyCode::LeftArrow => {
                let group = match self.selected_entry()? {
                    Entry::Group { key, .. } => key.clone(),
                    Entry::Leaf { group: Some(g), .. } => g.clone(),
                    _ => return None,
                };
                let t = self.notif.as_mut()?;
                t.expanded.remove(&group);
                // Land on the (now collapsed) group row itself.
                let anchor = format!("g:{group}");
                let pos = t
                    .build()
                    .1
                    .iter()
                    .position(|e| t.entry_id(e).as_deref() == Some(anchor.as_str()));
                if let Some(p) = pos {
                    self.sel = p;
                }
                self.rebuild_notif();
                Some(DetailOutcome::Pending)
            }
            _ => None,
        }
    }

    /// Drop the leaves with these keys (a batch dismissal) and rebuild.
    pub(crate) fn remove_notif_leaves(&mut self, keys: &[String]) {
        if let Some(t) = self.notif.as_mut() {
            t.leaves.retain(|l| !keys.contains(&l.key));
            self.rebuild_notif();
        }
    }

    /// `remove_selected` for the grouped list: the selected item, the whole
    /// selected group, or the selected fixed row. Returns false when this
    /// overlay has no tree.
    pub(super) fn notif_remove_selected(&mut self) -> bool {
        if !matches!(self.content, DetailContent::List(_)) {
            return false;
        }
        let Some(t) = self.notif.as_mut() else {
            return false;
        };
        match t.entries.get(self.sel).cloned() {
            Some(Entry::Leaf { idx, .. }) => {
                let key = t.leaves[idx].key.clone();
                t.leaves.retain(|l| l.key != key);
            }
            Some(Entry::Group { members, .. }) => {
                let keys: Vec<String> = members.iter().map(|&i| t.leaves[i].key.clone()).collect();
                t.leaves.retain(|l| !keys.contains(&l.key));
            }
            Some(Entry::Static(j)) => {
                t.tail.remove(j);
                // An emptied heading goes with its last row.
                if j > 0 && t.tail[j - 1].header && t.tail.get(j).is_none_or(|r| r.header) {
                    t.tail.remove(j - 1);
                }
            }
            _ => return true,
        }
        self.rebuild_notif();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn leaf(key: &str, kind: NKind, source: &str, age: i64) -> Leaf {
        Leaf {
            key: key.into(),
            kind,
            source: source.into(),
            ts: Some(NOW - age),
            stale: age > STALE_AFTER_SECS,
            search: format!("{key} {source}").to_lowercase(),
            row: DetailRow::new(Tok::Slot(S::Dim), "", key.to_string())
                .action('x', DetailAction::AckAllAttention),
            id: key.strip_prefix('n').and_then(|n| n.parse().ok()),
            ack: None,
        }
    }

    #[test]
    fn source_key_prefers_batch_then_project_dir() {
        assert_eq!(source_key("/h/wt/thegn/b313-the-2147"), "Pipeline b313");
        assert_eq!(source_key("/h/wt/thegn/b314"), "Pipeline b314");
        assert_eq!(source_key("/h/wt/thegn/bar-baz"), "thegn");
        assert_eq!(source_key("/h/wt/thegn/b12x-y"), "thegn");
        assert_eq!(source_key(""), "");
    }

    #[test]
    fn stale_after_a_day_and_never_without_a_timestamp() {
        assert!(!is_stale(Some(NOW - STALE_AFTER_SECS), NOW));
        assert!(is_stale(Some(NOW - STALE_AFTER_SECS - 1), NOW));
        assert!(!is_stale(None, NOW));
    }

    #[test]
    fn empty_input_has_no_entries() {
        assert!(layout(&[], &HashSet::new(), "").is_empty());
    }

    #[test]
    fn folds_by_kind_then_source_with_stale_last() {
        let leaves = vec![
            leaf("a1", NKind::Info, "", 10),
            leaf("p1", NKind::NeedsInput, "Pipeline b313", 100),
            leaf("p2", NKind::NeedsInput, "Pipeline b313", 50),
            leaf("p3", NKind::NeedsInput, "Pipeline b313", 70),
            leaf("f1", NKind::Failed, "thegn", 5),
            leaf(
                "o1",
                NKind::NeedsInput,
                "Pipeline b200",
                STALE_AFTER_SECS + 5,
            ),
            leaf(
                "o2",
                NKind::NeedsInput,
                "Pipeline b200",
                STALE_AFTER_SECS + 9,
            ),
        ];
        let e = layout(&leaves, &HashSet::new(), "");
        assert_eq!(
            e[0],
            Entry::Summary("3 need input \u{b7} 1 failed \u{b7} 1 info \u{b7} 2 older".into())
        );
        assert_eq!(e[1], Entry::KindHeader(NKind::NeedsInput, 3));
        // One folded group (collapsed), members newest first: p2, p3, p1.
        assert_eq!(
            e[2],
            Entry::Group {
                key: "needs/Pipeline b313".into(),
                kind: Some(NKind::NeedsInput),
                source: "Pipeline b313".into(),
                members: vec![2, 3, 1],
                expanded: false,
            }
        );
        assert_eq!(e[3], Entry::KindHeader(NKind::Failed, 1));
        assert_eq!(
            e[4],
            Entry::Leaf {
                idx: 4,
                group: None
            }
        );
        assert_eq!(e[5], Entry::KindHeader(NKind::Info, 1));
        assert_eq!(
            e[6],
            Entry::Leaf {
                idx: 0,
                group: None
            }
        );
        // Older is a collapsed group at the very bottom.
        assert_eq!(
            e[7],
            Entry::Group {
                key: "older".into(),
                kind: None,
                source: String::new(),
                members: vec![5, 6],
                expanded: false,
            }
        );
        assert_eq!(e.len(), 8);
    }

    #[test]
    fn expanding_a_group_lists_its_members_indented() {
        let leaves = vec![
            leaf("p1", NKind::NeedsInput, "Pipeline b1", 10),
            leaf("p2", NKind::NeedsInput, "Pipeline b1", 20),
        ];
        let open: HashSet<String> = ["needs/Pipeline b1".to_string()].into();
        let e = layout(&leaves, &open, "");
        assert!(matches!(&e[2], Entry::Group { expanded: true, .. }));
        assert_eq!(
            e[3],
            Entry::Leaf {
                idx: 0,
                group: Some("needs/Pipeline b1".into())
            }
        );
        assert_eq!(e.len(), 5);
    }

    #[test]
    fn a_lone_item_from_a_source_is_not_folded() {
        let leaves = vec![leaf("p1", NKind::Failed, "thegn", 10)];
        let e = layout(&leaves, &HashSet::new(), "");
        assert_eq!(
            e[2],
            Entry::Leaf {
                idx: 0,
                group: None
            }
        );
    }

    #[test]
    fn filter_matches_text_and_source_and_opens_groups() {
        let leaves = vec![
            leaf("alpha", NKind::Failed, "thegn", 10),
            leaf("beta", NKind::Failed, "thegn", 20),
            leaf("gamma", NKind::Failed, "other", 30),
        ];
        let e = layout(&leaves, &HashSet::new(), "THEGN");
        assert_eq!(e[0], Entry::Summary("2 failed".into()));
        assert!(
            matches!(&e[2], Entry::Group { expanded: true, members, .. } if members.len() == 2)
        );
        assert_eq!(e.len(), 5, "summary, header, group, two members");
        assert!(layout(&leaves, &HashSet::new(), "zzz").is_empty());
    }

    #[test]
    fn dismissal_collects_ids_acks_and_keys() {
        let mut a = leaf("n7", NKind::Info, "x", 1);
        a.id = Some(7);
        let mut b = leaf("w1", NKind::Failed, "x", 2);
        b.ack = Some(AckTarget {
            path: "/wt/a".into(),
            reason: thegn_core::attention::AttentionReason::ProcessFailed,
            since: Some(3),
            episode: 0,
        });
        let leaves = vec![a, b];
        let DetailAction::DismissBatch { ids, acks, keys } = dismissal(&leaves, &[0, 1]) else {
            panic!("batch");
        };
        assert_eq!(ids, vec![7]);
        assert_eq!(acks.len(), 1);
        assert_eq!(keys, vec!["n7".to_string(), "w1".into()]);
    }
}
