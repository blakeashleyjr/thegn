//! Pure fold passes over built sidebar rows: batch grouping and the row limit.
use crate::sidebar::{ActivityState, RowKind, SidebarRow, ViewState};

/// A [`RowKind::BatchGroup`] row's rollup over its members.
#[allow(dead_code)] // spine stub: lane 2 builds it, lane 4 renders it; drop this allow then
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GroupStats {
    /// Members in the batch.
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
/// Stub (lane 2 fills): never a batch.
#[allow(dead_code)] // spine stub: lane 2 wires it up and drops this allow
pub(crate) fn batch_key(_label: &str) -> Option<&str> {
    None
}

/// The leading part of a batch member's label hidden on screen.
/// Stub (lane 2 fills): nothing hidden.
#[allow(dead_code)] // spine stub: lane 2 wires it up and drops this allow
pub(crate) fn member_prefix(_label: &str, _key: &str) -> String {
    String::new()
}

/// The `pin_key` / collapse key of a batch row.
#[allow(dead_code)] // spine stub: lane 2 wires it up and drops this allow
pub(crate) fn batch_group_key(slug: &str, key: &str) -> String {
    format!("batch:{slug}/{key}")
}

/// Plan a workspace's loose run: which worktrees fold into batch rows.
/// `keep_loose[i]` (pinned) keeps a worktree out of any batch; `min == 0`
/// disables batching. Stub (lane 2 fills): every worktree single, in order.
pub(crate) fn plan_loose(labels: &[&str], _keep_loose: &[bool], _min: usize) -> Vec<LooseItem> {
    (0..labels.len()).map(LooseItem::Single).collect()
}

/// Emit one batch row and its member rows.
/// Stub (lane 2 fills): the members are emitted loose.
pub(crate) fn push_batch_group(
    rows: &mut Vec<SidebarRow>,
    _slug: &str,
    _key: &str,
    members: Vec<SidebarRow>,
    _parent_collapsed: bool,
    _view: &ViewState,
) {
    rows.extend(members);
}

/// Fill every batch row's [`SidebarRow::group_stats`] from its members.
/// Stub (lane 2 fills): no-op.
pub(crate) fn rollup_batch_stats(_rows: &mut [SidebarRow]) {}

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
/// - Pinned entries and the entry holding the active row are never hidden.
///   They count toward the limit: a protected entry past it takes the slot of
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
                    || rows[s..e].iter().any(|r| r.active)
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
