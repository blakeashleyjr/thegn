//! Pure fold passes over built sidebar rows: batch grouping and the row limit.
use crate::sidebar::{ActivityState, RowKind, SidebarRow, ViewState};

/// A [`RowKind::BatchGroup`] row's rollup over its members.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupStats {
    /// Members in the batch.
    #[allow(dead_code)] // only the fold tests read it; the row shows `child_count`
    pub lanes: usize,
    /// Members whose activity is `Active` or `Loading`.
    pub active: usize,
    /// Members `Waiting` (finished, unread) and not blocked.
    pub done: usize,
    /// Members whose attention tier is `Blocked`, or whose activity is `Failed`.
    pub blocked: usize,
    /// The most urgent member state.
    pub marker: ActivityState,
    /// That member is blocked (red, not amber).
    pub marker_blocked: bool,
}

/// One entry of a workspace's loose run, in emission order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LooseItem {
    /// The loose worktree at this index, emitted as a plain row.
    Single(usize),
    /// A batch row for token `key` holding the loose worktrees at `members`.
    Batch { key: String, members: Vec<usize> },
}

/// The batch token of a branch label (`tg/b158-config-dir-env` -> `b158`).
///
/// The token is the text before the first `-` of the label's last `/`
/// segment, and must be 1-3 ASCII letters followed by 1-6 ASCII digits with
/// nothing else. A segment with no `-` (`main`, `tg/b158`) has no token.
/// Keyed on the worktree's label, never its (drifting) display branch.
pub(crate) fn batch_key(label: &str) -> Option<&str> {
    let tail = label.rsplit('/').next().unwrap_or(label);
    let (token, _) = tail.split_once('-')?;
    let letters = token
        .bytes()
        .take_while(|b| b.is_ascii_alphabetic())
        .count();
    let digits = token.len() - letters;
    let valid = (1..=3).contains(&letters)
        && (1..=6).contains(&digits)
        && token.as_bytes()[letters..].iter().all(u8::is_ascii_digit);
    valid.then_some(token)
}

/// The leading part of a batch member's label hidden on screen: everything
/// through `{key}-` after the last `/`, repeated while the rest starts with
/// `{key}-` again (`tg/b161-b161-spine` -> `tg/b161-b161-`). Never the whole
/// label: a strip that would leave nothing stops short (`tg/b158-b158` ->
/// `tg/b158-`). Empty when the tail does not start with `{key}-`.
pub(crate) fn member_prefix(label: &str, key: &str) -> String {
    let tail_start = label.rfind('/').map_or(0, |i| i + 1);
    let step = key.len() + 1;
    let mut end = tail_start;
    while let Some(rest) = label.get(end..) {
        let starts =
            rest.len() > step && rest.starts_with(key) && rest.as_bytes()[key.len()] == b'-';
        if !starts {
            break;
        }
        end += step;
    }
    if end == tail_start {
        String::new()
    } else {
        label[..end].to_string()
    }
}

/// The `pin_key` / collapse key of a batch row.
pub(crate) fn batch_group_key(slug: &str, key: &str) -> String {
    format!("batch:{slug}/{key}")
}

/// The numeric suffix of a batch token (`b161` -> 161); 0 if unparsable.
fn batch_number(key: &str) -> u32 {
    let digits = key.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    digits.parse::<u32>().unwrap_or(0)
}

/// Plan a workspace's loose run: which worktrees fold into batch rows.
/// `keep_loose[i]` (pinned) keeps a worktree out of any batch; `min == 0`
/// disables batching.
///
/// Singles come first in input order, then one batch per token with at least
/// `min` members, newest first (numeric suffix descending, then token
/// ascending); members keep input order. Deterministic: no hash order.
pub(crate) fn plan_loose(labels: &[&str], keep_loose: &[bool], min: usize) -> Vec<LooseItem> {
    if min == 0 {
        return (0..labels.len()).map(LooseItem::Single).collect();
    }
    let key_of = |i: usize| {
        if keep_loose.get(i).copied().unwrap_or(false) {
            None
        } else {
            batch_key(labels[i])
        }
    };
    let mut by_key: std::collections::BTreeMap<&str, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..labels.len() {
        if let Some(k) = key_of(i) {
            by_key.entry(k).or_default().push(i);
        }
    }
    by_key.retain(|_, members| members.len() >= min);
    let mut out: Vec<LooseItem> = (0..labels.len())
        .filter(|&i| key_of(i).is_none_or(|k| !by_key.contains_key(k)))
        .map(LooseItem::Single)
        .collect();
    let mut batches: Vec<(&str, Vec<usize>)> = by_key.into_iter().collect();
    batches.sort_by(|a, b| {
        batch_number(b.0)
            .cmp(&batch_number(a.0))
            .then_with(|| a.0.cmp(b.0))
    });
    out.extend(batches.into_iter().map(|(key, members)| LooseItem::Batch {
        key: key.to_string(),
        members,
    }));
    out
}

/// Emit one batch row and its member rows.
///
/// The header is collapsed unless the user opened it (`batch_open`) or it
/// holds the active worktree (then it renders open for the frame, like
/// "Other"); its `active` mirrors that. Members sit at depth 2 under it, keep
/// their `{slug}/{label}` pin_key, and carry the label prefix hidden on screen.
pub(crate) fn push_batch_group(
    rows: &mut Vec<SidebarRow>,
    slug: &str,
    key: &str,
    members: Vec<SidebarRow>,
    parent_collapsed: bool,
    view: &ViewState,
) {
    let pin_key = batch_group_key(slug, key);
    let holds_active = members.iter().any(|m| m.active);
    let open = view.batch_open.contains(&pin_key) || holds_active;
    rows.push(SidebarRow {
        pin_key,
        collapsed: !open,
        visible: !parent_collapsed,
        child_count: members.len(),
        active: holds_active,
        ..SidebarRow::base(RowKind::BatchGroup, 1, key, slug)
    });
    for mut m in members {
        m.depth = 2;
        m.visible = !parent_collapsed && open;
        let prefix = member_prefix(&m.label, key);
        m.batch_prefix = (!prefix.is_empty()).then_some(prefix);
        rows.push(m);
    }
}

/// Whether a row needs the user: attention tier `Blocked`, or a failed env.
fn needs_you(r: &SidebarRow) -> bool {
    r.activity == ActivityState::Failed
        || r.attention
            .as_ref()
            .is_some_and(|a| a.tier == thegn_core::attention::AttentionTier::Blocked)
}

/// Urgency rank of a member for the batch marker (higher wins):
/// Failed > blocked (Waiting/Read) > Waiting > Active > Loading > Read > None.
fn marker_rank(r: &SidebarRow) -> (u8, bool) {
    let blocked = needs_you(r);
    match r.activity {
        ActivityState::Failed => (6, true),
        ActivityState::Waiting | ActivityState::Read if blocked => (5, true),
        ActivityState::Waiting => (4, false),
        ActivityState::Active => (3, false),
        ActivityState::Loading => (2, false),
        ActivityState::Read => (1, false),
        ActivityState::None => (0, false),
    }
}

/// Fill every batch row's [`SidebarRow::group_stats`] from its members (the
/// following rows deeper than it).
pub(crate) fn rollup_batch_stats(rows: &mut [SidebarRow]) {
    for h in 0..rows.len() {
        if rows[h].kind != RowKind::BatchGroup {
            continue;
        }
        let depth = rows[h].depth;
        let mut stats = GroupStats::default();
        let mut best: Option<u8> = None;
        for r in rows[h + 1..].iter().take_while(|r| r.depth > depth) {
            stats.lanes += 1;
            let blocked = needs_you(r);
            if matches!(r.activity, ActivityState::Active | ActivityState::Loading) {
                stats.active += 1;
            }
            if blocked {
                stats.blocked += 1;
            } else if r.activity == ActivityState::Waiting {
                stats.done += 1;
            }
            let (rank, marker_blocked) = marker_rank(r);
            if best.is_none_or(|b| rank > b) {
                best = Some(rank);
                stats.marker = r.activity;
                stats.marker_blocked = marker_blocked;
            }
        }
        rows[h].group_stats = Some(stats);
    }
}

/// Whether rows of this kind head a group the row limit applies to: a
/// workspace (its loose rows), a user folder, the derived "Other" group, and
/// the derived pipeline group and lane folders.
pub(crate) fn is_limited_group(kind: RowKind) -> bool {
    matches!(
        kind,
        RowKind::Workspace
            | RowKind::Folder
            | RowKind::OtherGroup
            | RowKind::PipelineGroup
            | RowKind::PipelineLane
    )
}

/// Whether a direct child of a limited group is an *entry* that counts toward
/// (and can be hidden by) the limit. Nested group heads (folders, "Other",
/// `Pipelines`) are structure, not entries: they always show and carry their
/// own limit.
pub(crate) fn counts_toward_limit(kind: RowKind) -> bool {
    !matches!(
        kind,
        RowKind::Folder
            | RowKind::OtherGroup
            | RowKind::PipelineGroup
            | RowKind::SectionHeading
            | RowKind::EmptyHint
            | RowKind::ShowMore
    )
}

/// Whether one row keeps its entry block from folding: the active row, the
/// sidebar cursor's row, a `Blocked` row, a `Failed` row.
fn is_protected_row(r: &SidebarRow, view: &ViewState) -> bool {
    r.active || view.cursor_key.as_ref() == Some(&r.pin_key) || needs_you(r)
}

/// The row-limit pass (`[ui] sidebar_group_row_limit` /
/// `sidebar_group_row_limits`), run on the final order (after sorts and
/// pins). For every group with more entries than its limit, entries past the
/// limit are hidden (`visible = false`, their subtrees with them) and one
/// [`RowKind::ShowMore`] row is inserted after the group's last entry. While
/// the group is expanded (`ViewState::more_open`) every entry shows and the
/// same row reads "show fewer".
///
/// - The limit is the group label's entry in `group_row_limits`, else
///   `group_row_limit`; `0` disables it.
/// - Pinned entries are never hidden, nor is an entry holding the active
///   row, the sidebar cursor's row, a `Blocked` row or a `Failed` row (a batch
///   row holding one is shown, collapsed or not). Finished-unread rows fold.
///   Protected entries count toward the limit: a protected entry past it takes the slot of
///   the last unprotected one, so the group still shows `limit` entries.
/// - A non-empty filter disables the pass: every match shows.
///
/// Pure string/flag work over rows already built; no I/O.
pub(crate) fn apply_row_limits(rows: &mut Vec<SidebarRow>, view: &ViewState) {
    let disp = &view.display;
    if !view.filter.trim().is_empty() {
        return;
    }
    if disp.group_row_limit == 0 && disp.group_row_limits.values().all(|l| *l == 0) {
        return;
    }
    let heads: Vec<usize> = (0..rows.len())
        .filter(|&i| is_limited_group(rows[i].kind))
        .collect();
    // Back to front: an insertion only shifts rows after it, which are
    // already processed, and a nested group's toggle row then sits inside its
    // parent's entry block (so hiding the entry hides it too).
    for &h in heads.iter().rev() {
        let limit = disp
            .group_row_limits
            .get(rows[h].label.as_str())
            .copied()
            .unwrap_or(disp.group_row_limit);
        if limit == 0 {
            continue;
        }
        let depth = rows[h].depth;
        // Direct-child entry blocks as half-open `(start, end)` ranges.
        let mut blocks: Vec<(usize, usize)> = Vec::new();
        let mut j = h + 1;
        while j < rows.len() && rows[j].depth > depth {
            let start = j;
            j += 1;
            while j < rows.len() && rows[j].depth > rows[start].depth {
                j += 1;
            }
            if counts_toward_limit(rows[start].kind) {
                blocks.push((start, j));
            }
        }
        if blocks.len() <= limit {
            continue;
        }
        let protected: Vec<bool> = blocks
            .iter()
            .map(|&(s, e)| {
                (!rows[s].pin_key.is_empty() && view.pins.contains(&rows[s].pin_key))
                    || rows[s..e].iter().any(|r| is_protected_row(r, view))
            })
            .collect();
        let mut budget = limit.saturating_sub(protected.iter().filter(|p| **p).count());
        let mut hide: Vec<bool> = Vec::with_capacity(blocks.len());
        for &p in &protected {
            let shown = if p {
                true
            } else if budget > 0 {
                budget -= 1;
                true
            } else {
                false
            };
            hide.push(!shown);
        }
        let hidden = hide.iter().filter(|h| **h).count();
        if hidden == 0 {
            continue;
        }
        // The toggle shows exactly when the group's shown entries do (a
        // collapsed ancestor hides both).
        let visible = blocks
            .iter()
            .zip(&hide)
            .find(|(_, h)| !**h)
            .is_some_and(|(&(s, _), _)| rows[s].visible);
        let key = rows[h].collapse_key().to_string();
        let open = view.more_open.contains(&key);
        if !open {
            for (&(s, e), _) in blocks.iter().zip(&hide).filter(|(_, h)| **h) {
                for r in &mut rows[s..e] {
                    r.visible = false;
                }
            }
        }
        // Right after the last entry the group still shows (so the toggle
        // never drifts past following folders/sub-groups); when expanded,
        // after the last entry.
        let at = if open {
            blocks.last().map_or(h + 1, |b| b.1)
        } else {
            blocks
                .iter()
                .zip(&hide)
                .rev()
                .find(|(_, h)| !**h)
                .map_or(h + 1, |(b, _)| b.1)
        };
        let slug = rows[h].workspace_slug.clone();
        rows.insert(
            at,
            SidebarRow {
                pin_key: crate::sidebar::show_more_key(&key),
                collapsed: !open,
                child_count: hidden,
                visible,
                ..SidebarRow::base(RowKind::ShowMore, depth + 1, String::new(), slug)
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::attention::{AttentionScore, AttentionTier};

    /// `(label, expected token)` table rows.
    type KeyCase = (&'static str, Option<&'static str>);

    fn wt(label: &str) -> SidebarRow {
        SidebarRow {
            pin_key: format!("app/{label}"),
            ..SidebarRow::base(RowKind::Worktree, 1, label, "app")
        }
    }

    fn ws() -> SidebarRow {
        SidebarRow::base(RowKind::Workspace, 0, "app", "app")
    }

    fn blocked(mut r: SidebarRow) -> SidebarRow {
        r.attention = Some(AttentionScore {
            tier: AttentionTier::Blocked,
            ..Default::default()
        });
        r
    }

    fn with(mut r: SidebarRow, a: ActivityState) -> SidebarRow {
        r.activity = a;
        r
    }

    fn view7() -> ViewState {
        let mut v = ViewState::default();
        v.display.group_row_limit = 7;
        v
    }

    /// The owner's real worktree list (input order).
    fn owner_list() -> Vec<String> {
        let mut out: Vec<String> = vec![
            "merry-stream".into(),
            "tg/b137-the-2001-fix".into(),
            "tg/hardy-onyx".into(),
        ];
        out.extend((0..5).map(|i| format!("tg/b158-lane{i}")));
        out.push("tg/b159-review".into());
        for (b, n) in [(160, 11), (161, 11), (162, 11), (163, 10)] {
            out.extend((0..n).map(|i| format!("tg/b{b}-b{b}-lane{i}")));
        }
        out
    }

    fn plan_of(labels: &[String], min: usize) -> Vec<LooseItem> {
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        plan_loose(&refs, &vec![false; refs.len()], min)
    }

    fn batch_keys(plan: &[LooseItem]) -> Vec<&str> {
        plan.iter()
            .filter_map(|p| match p {
                LooseItem::Batch { key, .. } => Some(key.as_str()),
                LooseItem::Single(_) => None,
            })
            .collect()
    }

    fn singles(plan: &[LooseItem]) -> Vec<usize> {
        plan.iter()
            .filter_map(|p| match p {
                LooseItem::Single(i) => Some(*i),
                LooseItem::Batch { .. } => None,
            })
            .collect()
    }

    #[test]
    fn batch_key_accepts_batch_tokens_only() {
        let cases: Vec<KeyCase> = vec![
            ("tg/b158-config-dir-env", Some("b158")),
            ("b161-spine", Some("b161")),
            ("tg/v2-x", Some("v2")),
            ("tg/hardy-onyx", None),
            ("main", None),
            ("tg/b158", None),
            ("feature/the-2060-x", None),
            ("tg/abcd12-x", None),
            ("tg/b1234567-x", None),
            ("b00", None),
            ("tg/b12a-x", None),
            ("tg/123-x", None),
        ];
        for (label, want) in cases {
            assert_eq!(batch_key(label), want, "{label}");
        }
    }

    #[test]
    fn member_prefix_strips_a_doubled_token_but_never_the_whole_label() {
        assert_eq!(member_prefix("tg/b161-b161-spine", "b161"), "tg/b161-b161-");
        assert_eq!(member_prefix("tg/b158-config-dir-env", "b158"), "tg/b158-");
        assert_eq!(member_prefix("tg/b158-b158", "b158"), "tg/b158-");
        assert_eq!(member_prefix("b161-spine", "b161"), "b161-");
        assert_eq!(member_prefix("tg/b158-", "b158"), "");
        assert_eq!(member_prefix("tg/hardy-onyx", "b158"), "");
    }

    #[test]
    fn plan_loose_puts_singles_first_and_batches_newest_first() {
        let labels = owner_list();
        let plan = plan_of(&labels, 2);
        let single_labels: Vec<&str> = singles(&plan)
            .into_iter()
            .map(|i| labels[i].as_str())
            .collect();
        assert_eq!(
            single_labels,
            vec![
                "merry-stream",
                "tg/b137-the-2001-fix",
                "tg/hardy-onyx",
                "tg/b159-review"
            ]
        );
        assert!(plan[..4].iter().all(|p| matches!(p, LooseItem::Single(_))));
        assert_eq!(
            batch_keys(&plan),
            vec!["b163", "b162", "b161", "b160", "b158"]
        );
        for item in &plan {
            if let LooseItem::Batch { key, members } = item {
                assert!(members.windows(2).all(|w| w[0] < w[1]), "input order");
                assert!(
                    members
                        .iter()
                        .all(|&i| batch_key(&labels[i]) == Some(key.as_str()))
                );
            }
        }
        let LooseItem::Batch { members, .. } = &plan[4] else {
            panic!("b163 batch");
        };
        assert_eq!(members.len(), 10);
        // Ties on the number order by token.
        let ties: Vec<String> = ["tg/v7-a", "tg/v7-b", "tg/b7-a", "tg/b7-b"]
            .iter()
            .map(|l| l.to_string())
            .collect();
        let ties = plan_of(&ties, 2);
        assert_eq!(batch_keys(&ties), vec!["b7", "v7"]);
    }

    #[test]
    fn plan_loose_min_zero_is_off() {
        let labels = owner_list();
        let plan = plan_of(&labels, 0);
        assert_eq!(
            plan,
            (0..labels.len()).map(LooseItem::Single).collect::<Vec<_>>()
        );
    }

    #[test]
    fn plan_loose_min_three_leaves_pairs_loose() {
        let labels: Vec<String> = vec![
            "tg/b1-a".into(),
            "tg/b1-b".into(),
            "tg/b2-a".into(),
            "tg/b2-b".into(),
            "tg/b2-c".into(),
        ];
        let plan = plan_of(&labels, 3);
        assert_eq!(
            plan,
            vec![
                LooseItem::Single(0),
                LooseItem::Single(1),
                LooseItem::Batch {
                    key: "b2".into(),
                    members: vec![2, 3, 4]
                },
            ]
        );
    }

    #[test]
    fn plan_loose_keeps_pinned_rows_loose_and_below_min_stays_loose() {
        let labels = ["tg/b1-a", "tg/b1-b", "tg/b1-c", "tg/b2-a", "tg/b2-b"];
        // b1-a pinned: b1 keeps 2 members; b2-a pinned: b2 drops below min.
        let keep = [true, false, false, true, false];
        let plan = plan_loose(&labels, &keep, 2);
        assert_eq!(
            plan,
            vec![
                LooseItem::Single(0),
                LooseItem::Single(3),
                LooseItem::Single(4),
                LooseItem::Batch {
                    key: "b1".into(),
                    members: vec![1, 2]
                },
            ]
        );
    }

    /// An empty loose run emits nothing, with grouping on or off.
    #[test]
    fn plan_loose_of_no_worktrees_is_empty() {
        let labels: Vec<&str> = Vec::new();
        let keep: Vec<bool> = Vec::new();
        assert!(plan_loose(&labels, &keep, 2).is_empty());
        assert!(plan_loose(&labels, &keep, 0).is_empty());
    }

    fn batch(
        view: &ViewState,
        parent_collapsed: bool,
        members: Vec<SidebarRow>,
    ) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        push_batch_group(&mut rows, "app", "b161", members, parent_collapsed, view);
        rows
    }

    fn b161_members() -> Vec<SidebarRow> {
        vec![wt("tg/b161-b161-spine"), wt("tg/b161-b161-fold")]
    }

    #[test]
    fn push_batch_group_is_collapsed_by_default() {
        let rows = batch(&ViewState::default(), false, b161_members());
        assert_eq!(rows.len(), 3);
        let h = &rows[0];
        assert_eq!(h.kind, RowKind::BatchGroup);
        assert_eq!(h.label, "b161");
        assert_eq!(h.pin_key, "batch:app/b161");
        assert_eq!(h.collapse_key(), "batch:app/b161");
        assert_eq!(h.depth, 1);
        assert_eq!(h.child_count, 2);
        assert!(h.collapsed && h.visible && !h.active);
        assert!(!h.is_pinnable() && !h.is_markable());
        assert!(rows[1..].iter().all(|r| !r.visible));
    }

    #[test]
    fn batch_open_opens_it() {
        let mut view = ViewState::default();
        view.batch_open.insert("batch:app/b161".into());
        let rows = batch(&view, false, b161_members());
        assert!(!rows[0].collapsed);
        assert!(rows.iter().all(|r| r.visible));
    }

    #[test]
    fn a_batch_holding_the_active_row_renders_open() {
        let mut members = b161_members();
        members[1].active = true;
        let rows = batch(&ViewState::default(), false, members);
        assert!(!rows[0].collapsed);
        assert!(rows[0].active, "the header carries the active flag");
        assert!(rows.iter().all(|r| r.visible));
    }

    #[test]
    fn a_collapsed_parent_hides_header_and_members() {
        let mut view = ViewState::default();
        view.batch_open.insert("batch:app/b161".into());
        let rows = batch(&view, true, b161_members());
        assert!(rows.iter().all(|r| !r.visible));
        assert!(!rows[0].collapsed, "its own open state survives");
    }

    #[test]
    fn members_are_depth_two_with_a_prefix() {
        let mut members = b161_members();
        members.push(wt("b161"));
        let rows = batch(&ViewState::default(), false, members);
        assert_eq!(rows[1].depth, 2);
        assert_eq!(rows[1].batch_prefix.as_deref(), Some("tg/b161-b161-"));
        assert_eq!(rows[1].pin_key, "app/tg/b161-b161-spine");
        assert_eq!(rows[1].label, "tg/b161-b161-spine");
        assert_eq!(rows[2].batch_prefix.as_deref(), Some("tg/b161-b161-"));
        assert_eq!(rows[3].batch_prefix, None, "nothing to strip");
    }

    #[test]
    fn rollup_counts_active_done_blocked_and_picks_the_most_urgent_marker() {
        let members = vec![
            with(wt("tg/b1-a"), ActivityState::Active),
            with(wt("tg/b1-b"), ActivityState::Loading),
            with(wt("tg/b1-c"), ActivityState::Waiting),
            blocked(with(wt("tg/b1-d"), ActivityState::Read)),
            with(wt("tg/b1-e"), ActivityState::Failed),
            with(wt("tg/b1-f"), ActivityState::Read),
            wt("tg/b1-g"),
        ];
        let mut rows = vec![ws()];
        push_batch_group(
            &mut rows,
            "app",
            "b1",
            members,
            false,
            &ViewState::default(),
        );
        rows.push(with(wt("loose"), ActivityState::Active));
        rollup_batch_stats(&mut rows);
        let s = rows[1].group_stats.expect("stats");
        assert_eq!(s.lanes, 7, "the trailing loose row is not a member");
        assert_eq!(s.active, 2);
        assert_eq!(s.done, 1);
        assert_eq!(s.blocked, 2);
        assert_eq!(s.marker, ActivityState::Failed);
        assert!(s.marker_blocked);
        assert!(rows[0].group_stats.is_none() && rows[2].group_stats.is_none());

        let ladder = |members: Vec<SidebarRow>| -> GroupStats {
            let mut rows = Vec::new();
            push_batch_group(
                &mut rows,
                "app",
                "b1",
                members,
                false,
                &ViewState::default(),
            );
            rollup_batch_stats(&mut rows);
            rows[0].group_stats.expect("stats")
        };
        let s = ladder(vec![
            with(wt("a"), ActivityState::Waiting),
            blocked(with(wt("b"), ActivityState::Read)),
        ]);
        assert_eq!((s.marker, s.marker_blocked), (ActivityState::Read, true));
        assert_eq!((s.done, s.blocked), (1, 1));
        let s = ladder(vec![
            with(wt("a"), ActivityState::Active),
            with(wt("b"), ActivityState::Waiting),
        ]);
        assert_eq!(
            (s.marker, s.marker_blocked),
            (ActivityState::Waiting, false)
        );
        let s = ladder(vec![
            with(wt("a"), ActivityState::Loading),
            with(wt("b"), ActivityState::Active),
        ]);
        assert_eq!(s.marker, ActivityState::Active);
        let s = ladder(vec![
            with(wt("a"), ActivityState::Read),
            with(wt("b"), ActivityState::Loading),
        ]);
        assert_eq!(s.marker, ActivityState::Loading);
        let s = ladder(vec![wt("a"), with(wt("b"), ActivityState::Read)]);
        assert_eq!(s.marker, ActivityState::Read);
        assert_eq!(ladder(vec![wt("a")]).marker, ActivityState::None);
    }

    fn ten() -> Vec<SidebarRow> {
        let mut rows = vec![ws()];
        rows.extend((0..10).map(|i| wt(&format!("wt{i}"))));
        rows
    }

    fn shown(rows: &[SidebarRow]) -> Vec<String> {
        rows.iter()
            .filter(|r| r.visible && r.depth == 1 && r.kind != RowKind::ShowMore)
            .map(|r| r.label.clone())
            .collect()
    }

    fn more(rows: &[SidebarRow]) -> Option<&SidebarRow> {
        rows.iter().find(|r| r.kind == RowKind::ShowMore)
    }

    #[test]
    fn limit_seven_shows_seven_entries() {
        let mut rows = ten();
        apply_row_limits(&mut rows, &view7());
        assert_eq!(
            shown(&rows),
            (0..7).map(|i| format!("wt{i}")).collect::<Vec<_>>()
        );
        assert_eq!(more(&rows).map(|m| m.child_count), Some(3));
    }

    #[test]
    fn the_cursor_row_is_never_folded() {
        let mut rows = ten();
        let mut view = view7();
        view.cursor_key = Some("app/wt9".into());
        apply_row_limits(&mut rows, &view);
        let s = shown(&rows);
        assert_eq!(s.len(), 7);
        assert!(s.contains(&"wt9".to_string()));
        assert!(!s.contains(&"wt6".to_string()));
        assert_eq!(more(&rows).map(|m| m.child_count), Some(3));
    }

    #[test]
    fn blocked_and_failed_rows_are_never_folded() {
        let mut rows = ten();
        rows[9] = blocked(wt("wt8"));
        rows[10] = with(wt("wt9"), ActivityState::Failed);
        apply_row_limits(&mut rows, &view7());
        assert_eq!(
            shown(&rows),
            vec!["wt0", "wt1", "wt2", "wt3", "wt4", "wt8", "wt9"]
        );
    }

    #[test]
    fn finished_unread_rows_fold() {
        let mut rows = ten();
        rows[10] = with(wt("wt9"), ActivityState::Waiting);
        apply_row_limits(&mut rows, &view7());
        assert!(!shown(&rows).contains(&"wt9".to_string()));
        assert_eq!(shown(&rows).len(), 7);
    }

    /// A workspace of `singles` loose rows and a trailing collapsed batch of
    /// `members` rows.
    fn with_batch(singles: usize, members: Vec<SidebarRow>, view: &ViewState) -> Vec<SidebarRow> {
        let mut rows = vec![ws()];
        rows.extend((0..singles).map(|i| wt(&format!("wt{i}"))));
        push_batch_group(&mut rows, "app", "b1", members, false, view);
        rows
    }

    fn lanes(n: usize) -> Vec<SidebarRow> {
        (0..n).map(|i| wt(&format!("tg/b1-lane{i}"))).collect()
    }

    #[test]
    fn a_batch_row_is_one_entry() {
        let mut rows = with_batch(6, lanes(10), &ViewState::default());
        apply_row_limits(&mut rows, &view7());
        assert!(more(&rows).is_none(), "6 singles + 1 batch = 7 entries");
        assert!(rows[7].visible && rows[7].kind == RowKind::BatchGroup);

        let mut rows = with_batch(7, lanes(10), &ViewState::default());
        apply_row_limits(&mut rows, &view7());
        assert_eq!(more(&rows).map(|m| m.child_count), Some(1));
        let h = rows
            .iter()
            .find(|r| r.kind == RowKind::BatchGroup)
            .expect("batch");
        assert!(!h.visible, "the tail batch folds as one entry");
    }

    #[test]
    fn a_batch_with_a_blocked_member_is_protected() {
        let mut members = lanes(4);
        members[2] = blocked(members[2].clone());
        let mut rows = with_batch(8, members, &ViewState::default());
        apply_row_limits(&mut rows, &view7());
        let h = rows
            .iter()
            .find(|r| r.kind == RowKind::BatchGroup)
            .expect("batch");
        assert!(h.visible && h.collapsed, "shown, still collapsed");
        assert_eq!(shown(&rows).len(), 7);
        assert_eq!(more(&rows).map(|m| m.child_count), Some(2));
    }

    #[test]
    fn an_open_batch_is_not_truncated() {
        let mut view = view7();
        view.batch_open.insert("batch:app/b1".into());
        let mut rows = with_batch(0, lanes(20), &view);
        apply_row_limits(&mut rows, &view);
        assert!(more(&rows).is_none());
        assert_eq!(
            rows.iter().filter(|r| r.visible && r.depth == 2).count(),
            20
        );
    }

    /// Eight members is one past the default limit, but a batch row counts as
    /// a single entry: the limit never reaches inside it.
    #[test]
    fn a_batch_just_over_the_limit_is_one_entry() {
        let mut view = view7();
        view.batch_open.insert("batch:app/b1".into());
        let mut rows = with_batch(0, lanes(8), &view);
        apply_row_limits(&mut rows, &view);
        assert!(more(&rows).is_none());
        let h = rows
            .iter()
            .find(|r| r.kind == RowKind::BatchGroup)
            .expect("batch");
        assert_eq!(h.child_count, 8);
        assert_eq!(rows.iter().filter(|r| r.visible && r.depth == 2).count(), 8);
    }
}
