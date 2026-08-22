//! Folder-aware sibling runs: the pure ordering model behind the sidebar's
//! manual reorder (keyboard `Ctrl+Alt+↑/↓` / `Shift+↑/↓` and mouse drag-drop).
//!
//! A worktree's reorder neighbourhood is its **run**, keyed by
//! `(workspace_slug, folder_id)`. Within a workspace the runs are ordered the
//! way [`crate::sidebar::build_rows`] emits them: the loose run first (with
//! `home` anchored at its head), then one run per folder in `folders.position`
//! order. Before this module every ordering path treated a workspace as one
//! flat run, so a `position` swap across a folder boundary changed nothing
//! visible (the renderer re-partitions by folder) while still scrambling the
//! *other* run's order on the way.
//!
//! Everything here is pure over `&[SidebarRow]` — the same rows the renderer
//! painted — so the rules are unit-testable without a terminal or a DB, in the
//! spirit of `render_plan::plan`. The handlers own persistence and the live
//! session permutation; this module only decides *what the new order is*.
//!
//! Run membership is read from the row tree (a depth-2 worktree belongs to the
//! folder header above it) rather than from `pin_key` string surgery, because
//! that is exactly the containment `build_rows` emits and `apply_pins`
//! preserves — pins reorder whole blocks, so a folder's children never escape
//! their header.

use crate::sidebar::{RowKind, SidebarRow};

/// One worktree in a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub path: String,
    /// The workspace's `home` row — a fixed anchor at the head of the loose
    /// run: it never moves and nothing may be placed above it.
    pub home: bool,
}

/// A sibling run: the loose worktrees of a workspace (`folder == None`) or the
/// worktrees filed into one folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Run {
    pub folder: Option<i64>,
    pub members: Vec<Member>,
    /// A collapsed folder is not *enterable*: a step that would cross into it
    /// hops over instead, so pressing ↓ can never hide a worktree inside a
    /// folder the user has closed.
    pub collapsed: bool,
}

/// A resolved reorder: the workspace's full new worktree order plus the
/// membership change, if any.
///
/// `order` covers **every** worktree in the workspace (all runs concatenated in
/// run order), not just the run that changed — including rows hidden inside a
/// collapsed folder. Persisting the whole sequence as `position = index` is what
/// keeps the durable order from drifting away from the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    /// The worktree that moved.
    pub path: String,
    /// The workspace's full new worktree order, by path.
    pub order: Vec<String>,
    /// `Some(folder)` when the move changed runs — the new `folder_id`, or
    /// `Some(None)` when it moved out to the loose run. `None` = same run.
    pub refile: Option<Option<i64>>,
}

/// The sibling runs of `slug`, loose first then folders in header order.
///
/// Includes rows that are currently invisible (filed into a collapsed folder),
/// because a persisted order must account for every member — dropping the
/// hidden ones would leave them with stale positions that interleave on reload.
pub(crate) fn runs(rows: &[SidebarRow], slug: &str) -> Vec<Run> {
    let mut loose = Run {
        folder: None,
        members: Vec::new(),
        collapsed: false,
    };
    let mut folders: Vec<Run> = Vec::new();
    // The folder header we are currently under; cleared by a depth-1 row.
    let mut cur: Option<i64> = None;

    for r in rows.iter().filter(|r| r.workspace_slug == slug) {
        match r.kind {
            RowKind::Workspace => cur = None,
            RowKind::Folder => {
                cur = r.folder_id;
                if let Some(fid) = r.folder_id
                    && !folders.iter().any(|f| f.folder == Some(fid))
                {
                    folders.push(Run {
                        folder: Some(fid),
                        members: Vec::new(),
                        collapsed: r.collapsed,
                    });
                }
            }
            RowKind::Worktree => {
                let Some(path) = r.worktree_path.clone() else {
                    continue;
                };
                let member = Member {
                    path,
                    home: r.label == "home",
                };
                // Filed children render at depth 2 under their header; anything
                // at depth 1 is loose and ends the folder region.
                match cur.filter(|_| r.depth >= 2) {
                    Some(fid) => {
                        if let Some(run) = folders.iter_mut().find(|f| f.folder == Some(fid)) {
                            run.members.push(member);
                        }
                    }
                    None => {
                        cur = None;
                        loose.members.push(member);
                    }
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::with_capacity(folders.len() + 1);
    out.push(loose);
    out.extend(folders);
    out
}

/// Flatten runs back into the workspace's full worktree order.
fn flatten(runs: &[Run]) -> Vec<String> {
    runs.iter()
        .flat_map(|r| r.members.iter().map(|m| m.path.clone()))
        .collect()
}

/// Locate `path`: `(run index, index within that run)`.
pub(crate) fn locate(runs: &[Run], path: &str) -> Option<(usize, usize)> {
    runs.iter().enumerate().find_map(|(ri, run)| {
        run.members
            .iter()
            .position(|m| m.path == path)
            .map(|mi| (ri, mi))
    })
}

/// The next enterable run in `dir` from run `ri`, skipping collapsed folders.
fn adjacent(runs: &[Run], ri: usize, up: bool) -> Option<usize> {
    let mut i = ri;
    loop {
        i = if up { i.checked_sub(1)? } else { i + 1 };
        let run = runs.get(i)?;
        if !run.collapsed {
            return Some(i);
        }
    }
}

/// Build a [`Plan`] from mutated runs, or `None` when nothing actually moved.
fn plan_from(
    runs: Vec<Run>,
    before: &[String],
    path: &str,
    refile: Option<Option<i64>>,
) -> Option<Plan> {
    let order = flatten(&runs);
    if order == before && refile.is_none() {
        return None;
    }
    Some(Plan {
        path: path.to_string(),
        order,
        refile,
    })
}

/// One step up or down from `path`, crossing into the adjacent run at an edge.
///
/// Crossing re-files: moving up off the head of a folder's run lands the
/// worktree at the **end** of the previous run (the loose list, or the folder
/// above); moving down off the tail lands it at the **head** of the next run.
/// Returns `None` when the move is blocked — `home`, the top of the loose run,
/// or the tail of the last run.
pub(crate) fn step(rows: &[SidebarRow], slug: &str, path: &str, up: bool) -> Option<Plan> {
    let mut runs = runs(rows, slug);
    let before = flatten(&runs);
    let (ri, mi) = locate(&runs, path)?;
    if runs[ri].members[mi].home {
        return None; // home is a fixed anchor
    }

    // Within the run: swap with the neighbour, unless that neighbour is home.
    let neighbor = if up {
        mi.checked_sub(1)
    } else {
        (mi + 1 < runs[ri].members.len()).then_some(mi + 1)
    };
    if let Some(ni) = neighbor {
        if runs[ri].members[ni].home {
            return None;
        }
        runs[ri].members.swap(mi, ni);
        return plan_from(runs, &before, path, None);
    }

    // At the edge: cross into the adjacent enterable run.
    let target = adjacent(&runs, ri, up)?;
    let member = runs[ri].members.remove(mi);
    if up {
        runs[target].members.push(member);
    } else {
        runs[target].members.insert(0, member);
    }
    let folder = runs[target].folder;
    plan_from(runs, &before, path, Some(folder))
}

/// The displacement rule, shared by every ordered list the sidebar drags:
/// remove `from`, then insert at the hovered item's **pre-removal** index `h`.
/// The dragged item takes the hovered item's slot; the hovered item shifts one
/// step toward where the source came from.
///
/// This is the whole reason a drop can reach the end of a run. Dragging DOWN,
/// the removal shifts the hovered item to `h - 1`, so inserting at `h` lands the
/// source *after* it — and hovering the LAST row therefore appends. The old
/// "insert before the hovered row" rule could not express the tail at all, which
/// is why the last slot was unreachable.
///
/// It also needs no up/down branch: when `from < h` the post-removal shift makes
/// `h` mean "after the hovered row", and when `from > h` it means "before".
fn displace<T>(items: &mut Vec<T>, from: usize, h: usize) {
    let it = items.remove(from);
    // A clamp, never a behaviour change: same-list with `from < h` gives
    // `h <= len`, and every other case gives `h < len`.
    items.insert(h.min(items.len()), it);
}

/// Where in the destination run a dropped worktree lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Landing<'a> {
    /// Take the slot this member currently occupies (displacement).
    Slot(&'a str),
    /// The end of the run. Produced only by the header affordances
    /// (file / unfile), which mean "land last".
    Tail,
}

/// Land `path` in run `dest` of `slug` at `landing` — the mouse drop entry
/// point.
///
/// Returns `None` when the drop is a no-op, when the anchor vanished or moved
/// runs mid-drag (abandon rather than guess a slot), or when the landing would
/// displace the anchored `home` row.
pub(crate) fn place_at(
    rows: &[SidebarRow],
    slug: &str,
    path: &str,
    dest: Option<i64>,
    landing: Landing<'_>,
) -> Option<Plan> {
    let mut runs = runs(rows, slug);
    let prev = flatten(&runs);
    let (ri, mi) = locate(&runs, path)?;
    if runs[ri].members[mi].home {
        return None; // home is a fixed anchor
    }
    let target = runs.iter().position(|r| r.folder == dest)?;

    // Resolve the landing index BEFORE the removal — that pre-removal index is
    // the displacement rule.
    let at = match landing {
        Landing::Tail => None,
        Landing::Slot(anchor) => {
            // Not in the destination run: the anchor was deleted or re-filed
            // mid-drag. Abandon rather than land the row somewhere the user
            // never aimed.
            let h = runs[target].members.iter().position(|m| m.path == anchor)?;
            if target == ri && h == mi {
                return None; // hovering the source itself
            }
            if h == 0 && runs[target].members[0].home {
                return None; // nothing may displace home
            }
            Some(h)
        }
    };

    match at {
        Some(h) if target == ri => displace(&mut runs[ri].members, mi, h),
        _ => {
            let member = runs[ri].members.remove(mi);
            let len = runs[target].members.len();
            let at = at.unwrap_or(len).min(len);
            runs[target].members.insert(at, member);
        }
    }

    // Run indices are unique per folder id, so a changed run IS a re-file.
    let refile = (target != ri).then_some(dest);
    plan_from(runs, &prev, path, refile)
}

/// The run the worktree at `path` currently belongs to: `Some(None)` for the
/// loose list, `Some(Some(id))` for a folder, `None` when it isn't in `slug`.
pub(crate) fn run_of(rows: &[SidebarRow], slug: &str, path: &str) -> Option<Option<i64>> {
    let runs = runs(rows, slug);
    let (ri, _) = locate(&runs, path)?;
    Some(runs[ri].folder)
}

/// The path of `path`'s immediate neighbour **within its own run**, or `None`
/// at a run edge (where a step would cross into the adjacent run instead).
///
/// Multi-select block moves use this to leave two selected neighbours alone
/// rather than swapping them past each other.
pub(crate) fn in_run_neighbor(
    rows: &[SidebarRow],
    slug: &str,
    path: &str,
    up: bool,
) -> Option<String> {
    let runs = runs(rows, slug);
    let (ri, mi) = locate(&runs, path)?;
    let members = &runs[ri].members;
    let ni = if up {
        mi.checked_sub(1)?
    } else {
        (mi + 1 < members.len()).then_some(mi + 1)?
    };
    Some(members[ni].path.clone())
}

/// The workspace's folder ids in display (header) order.
pub(crate) fn folder_order(rows: &[SidebarRow], slug: &str) -> Vec<i64> {
    rows.iter()
        .filter(|r| r.workspace_slug == slug && r.kind == RowKind::Folder)
        .filter_map(|r| r.folder_id)
        .collect()
}

/// Move folder `fid` one slot among its workspace's folders. Returns the new
/// folder id order, or `None` at either edge. A folder's worktrees follow its
/// header, so nothing about `worktrees.position` changes.
pub(crate) fn step_folder(rows: &[SidebarRow], slug: &str, fid: i64, up: bool) -> Option<Vec<i64>> {
    let mut order = folder_order(rows, slug);
    let i = order.iter().position(|f| *f == fid)?;
    let j = if up {
        i.checked_sub(1)?
    } else {
        (i + 1 < order.len()).then_some(i + 1)?
    };
    order.swap(i, j);
    Some(order)
}

/// Displace folder `anchor` with folder `fid` — the folder-drag drop. Returns
/// the new order, or `None` when nothing moved or the anchor vanished.
///
/// Folder drags are always same-list, so there is no `Tail` case: hovering the
/// last folder's header (or anything in its subtree) resolves to the last index,
/// which appends. That is what makes "move a folder to the bottom" possible —
/// under the old insert-before rule the last slot had no anchor to name.
pub(crate) fn displace_folder(
    rows: &[SidebarRow],
    slug: &str,
    fid: i64,
    anchor: i64,
) -> Option<Vec<i64>> {
    let mut order = folder_order(rows, slug);
    let prev = order.clone();
    let i = order.iter().position(|f| *f == fid)?;
    let h = order.iter().position(|f| *f == anchor)?;
    if h == i {
        return None; // hovering itself
    }
    displace(&mut order, i, h);
    (order != prev).then_some(order)
}

/// The manually orderable workspace slugs, in visible order.
///
/// A DB-backed header carries `worktree_path: Some(_)`; a live-only fallback
/// has no durable `position` to renumber, so it is not part of the order.
pub(crate) fn workspace_order(rows: &[SidebarRow]) -> Vec<String> {
    rows.iter()
        .filter(|r| r.visible && r.kind == RowKind::Workspace && r.worktree_path.is_some())
        .map(|r| r.workspace_slug.clone())
        .collect()
}

/// Displace workspace `anchor` with `slug`. Returns the new slug order, or
/// `None` when nothing moved or either slug is not manually orderable.
pub(crate) fn displace_workspace(
    rows: &[SidebarRow],
    slug: &str,
    anchor: &str,
) -> Option<Vec<String>> {
    let mut order = workspace_order(rows);
    let prev = order.clone();
    let i = order.iter().position(|s| s == slug)?;
    let h = order.iter().position(|s| s == anchor)?;
    if h == i {
        return None;
    }
    displace(&mut order, i, h);
    (order != prev).then_some(order)
}

/// The visible index of the LAST row of the block headed by `visible_index`: a
/// leaf row is its own block; a folder or workspace header extends through its
/// visible subtree.
///
/// [`crate::sidebar_view::DragSpotViz::InsertAfter`] paints its rule *below*
/// the visible row it names, so a header must pass its block end — passing the
/// header index painted the rule under the header while the drop landed after
/// the whole subtree.
pub(crate) fn block_end(rows: &[SidebarRow], visible_index: usize) -> usize {
    let visible: Vec<&SidebarRow> = rows.iter().filter(|r| r.visible).collect();
    let Some(row) = visible.get(visible_index) else {
        return visible_index;
    };
    let depth = row.depth;
    let mut j = visible_index;
    // Depths make this exact: a workspace header is depth 0 and its rows are
    // ≥1; a folder header is depth 1, its children are depth 2, and the next
    // loose worktree is back at depth 1.
    while visible
        .get(j + 1)
        .is_some_and(|n| n.depth > depth && n.kind != RowKind::SectionHeading)
    {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(slug: &str) -> SidebarRow {
        SidebarRow {
            pin_key: slug.into(),
            ..SidebarRow::base(RowKind::Workspace, 0, slug, slug)
        }
    }

    fn wt(slug: &str, label: &str, depth: u8) -> SidebarRow {
        SidebarRow {
            worktree_path: Some(format!("/w/{label}")),
            pin_key: format!("{slug}/{label}"),
            ..SidebarRow::base(RowKind::Worktree, depth, label, slug)
        }
    }

    fn folder(slug: &str, fid: i64, name: &str, collapsed: bool) -> SidebarRow {
        SidebarRow {
            folder_id: Some(fid),
            collapsed,
            pin_key: format!("{slug}/folder:{fid}"),
            ..SidebarRow::base(RowKind::Folder, 1, name, slug)
        }
    }

    /// home, a, b loose; folder 1 { c, d }; folder 2 { e }.
    fn tree() -> Vec<SidebarRow> {
        vec![
            ws("r"),
            wt("r", "home", 1),
            wt("r", "a", 1),
            wt("r", "b", 1),
            folder("r", 1, "One", false),
            wt("r", "c", 2),
            wt("r", "d", 2),
            folder("r", 2, "Two", false),
            wt("r", "e", 2),
        ]
    }

    fn paths(v: &[&str]) -> Vec<String> {
        v.iter().map(|p| format!("/w/{p}")).collect()
    }

    #[test]
    fn runs_partition_loose_and_folders() {
        let r = runs(&tree(), "r");
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].folder, None);
        assert_eq!(
            r[0].members
                .iter()
                .map(|m| m.path.clone())
                .collect::<Vec<_>>(),
            paths(&["home", "a", "b"])
        );
        assert!(r[0].members[0].home);
        assert_eq!(r[1].folder, Some(1));
        assert_eq!(
            r[1].members
                .iter()
                .map(|m| m.path.clone())
                .collect::<Vec<_>>(),
            paths(&["c", "d"])
        );
        assert_eq!(r[2].folder, Some(2));
    }

    #[test]
    fn reorder_within_a_folder_leaves_other_runs_alone() {
        let p = step(&tree(), "r", "/w/d", true).expect("moved");
        assert_eq!(p.refile, None);
        assert_eq!(p.order, paths(&["home", "a", "b", "d", "c", "e"]));
    }

    #[test]
    fn reorder_within_the_loose_run() {
        let p = step(&tree(), "r", "/w/b", true).expect("moved");
        assert_eq!(p.refile, None);
        assert_eq!(p.order, paths(&["home", "b", "a", "c", "d", "e"]));
    }

    #[test]
    fn up_off_a_folder_head_unfiles_to_the_end_of_the_loose_run() {
        let p = step(&tree(), "r", "/w/c", true).expect("moved");
        assert_eq!(p.refile, Some(None));
        assert_eq!(p.order, paths(&["home", "a", "b", "c", "d", "e"]));
    }

    #[test]
    fn down_off_the_loose_tail_files_into_the_first_folder() {
        let p = step(&tree(), "r", "/w/b", false).expect("moved");
        assert_eq!(p.refile, Some(Some(1)));
        assert_eq!(p.order, paths(&["home", "a", "b", "c", "d", "e"]));
    }

    #[test]
    fn down_off_a_folder_tail_enters_the_next_folder() {
        let p = step(&tree(), "r", "/w/d", false).expect("moved");
        assert_eq!(p.refile, Some(Some(2)));
        assert_eq!(p.order, paths(&["home", "a", "b", "c", "d", "e"]));
    }

    #[test]
    fn home_is_anchored_and_never_moves() {
        assert_eq!(step(&tree(), "r", "/w/home", true), None);
        assert_eq!(step(&tree(), "r", "/w/home", false), None);
        // …and nothing may be placed above it.
        assert_eq!(step(&tree(), "r", "/w/a", true), None);
    }

    #[test]
    fn blocked_at_the_outer_edges() {
        // The tail of the last run has nowhere to go.
        assert_eq!(step(&tree(), "r", "/w/e", false), None);
    }

    #[test]
    fn a_collapsed_folder_is_hopped_over_not_entered() {
        let mut rows = tree();
        // Collapse folder 1 and hide its children, as build_rows does.
        rows[4].collapsed = true;
        rows[5].visible = false;
        rows[6].visible = false;
        let p = step(&rows, "r", "/w/b", false).expect("moved");
        assert_eq!(p.refile, Some(Some(2)), "should skip the collapsed folder");
        // c and d keep their slots inside the collapsed folder.
        assert_eq!(p.order, paths(&["home", "a", "c", "d", "b", "e"]));
    }

    #[test]
    fn drop_into_a_folder_takes_the_hovered_siblings_slot() {
        let p = place_at(&tree(), "r", "/w/a", Some(1), Landing::Slot("/w/d")).expect("moved");
        assert_eq!(p.refile, Some(Some(1)));
        assert_eq!(p.order, paths(&["home", "b", "c", "a", "d", "e"]));
    }

    #[test]
    fn drop_at_the_tail_of_a_run_appends() {
        let p = place_at(&tree(), "r", "/w/a", Some(2), Landing::Tail).expect("moved");
        assert_eq!(p.refile, Some(Some(2)));
        assert_eq!(p.order, paths(&["home", "b", "c", "d", "e", "a"]));
    }

    #[test]
    fn drop_never_displaces_home() {
        assert_eq!(
            place_at(&tree(), "r", "/w/c", None, Landing::Slot("/w/home")),
            None
        );
    }

    #[test]
    fn drop_onto_a_vanished_anchor_is_refused() {
        assert_eq!(
            place_at(&tree(), "r", "/w/a", Some(1), Landing::Slot("/w/gone")),
            None
        );
        // An anchor that moved to a DIFFERENT run mid-drag is refused too:
        // `c` is in folder 1, so it cannot name a slot in the loose run.
        assert_eq!(
            place_at(&tree(), "r", "/w/a", None, Landing::Slot("/w/c")),
            None
        );
    }

    #[test]
    fn drop_within_the_same_run_reorders_without_refiling() {
        let p = place_at(&tree(), "r", "/w/d", Some(1), Landing::Slot("/w/c")).expect("moved");
        assert_eq!(p.refile, None);
        assert_eq!(p.order, paths(&["home", "a", "b", "d", "c", "e"]));
    }

    /// The rule, as its three worked cases. Dragging DOWN must land the source
    /// AFTER the hovered row, which is the half the old insert-before rule
    /// could not express — and hovering the last row must append.
    #[test]
    fn displace_is_the_slot_rule() {
        let mut v = vec!["a", "b", "c", "d"];
        displace(&mut v, 0, 2); // drag a onto c
        assert_eq!(v, ["b", "c", "a", "d"]);

        let mut v = vec!["a", "b", "c", "d"];
        displace(&mut v, 3, 1); // drag d onto b
        assert_eq!(v, ["a", "d", "b", "c"]);

        let mut v = vec!["a", "b", "c", "d"];
        displace(&mut v, 0, 3); // drag a onto the LAST row
        assert_eq!(v, ["b", "c", "d", "a"]);
    }

    #[test]
    fn dropping_on_the_last_member_of_a_run_lands_last() {
        // The loose run is [home, a, b]; hovering `b` from `a` must append.
        let p = place_at(&tree(), "r", "/w/a", None, Landing::Slot("/w/b")).expect("moved");
        assert_eq!(p.refile, None);
        assert_eq!(p.order, paths(&["home", "b", "a", "c", "d", "e"]));
        // …and inside a folder: folder 1 is [c, d], so `c` onto `d` appends.
        let p = place_at(&tree(), "r", "/w/c", Some(1), Landing::Slot("/w/d")).expect("moved");
        assert_eq!(p.order, paths(&["home", "a", "b", "d", "c", "e"]));
    }

    #[test]
    fn dropping_from_below_onto_a_member_takes_its_slot() {
        // `b` (loose index 2) onto `a` (index 1): a shifts down.
        let p = place_at(&tree(), "r", "/w/b", None, Landing::Slot("/w/a")).expect("moved");
        assert_eq!(p.order, paths(&["home", "b", "a", "c", "d", "e"]));
    }

    #[test]
    fn hovering_the_source_itself_is_a_no_op() {
        assert_eq!(
            place_at(&tree(), "r", "/w/a", None, Landing::Slot("/w/a")),
            None
        );
    }

    #[test]
    fn folder_steps_and_drops() {
        assert_eq!(folder_order(&tree(), "r"), vec![1, 2]);
        assert_eq!(step_folder(&tree(), "r", 2, true), Some(vec![2, 1]));
        assert_eq!(step_folder(&tree(), "r", 1, true), None);
        assert_eq!(step_folder(&tree(), "r", 2, false), None);
        assert_eq!(displace_folder(&tree(), "r", 2, 1), Some(vec![2, 1]));
        // Folder 1 onto folder 2 (the last one) lands LAST — impossible under
        // the old insert-before rule, which had no anchor for the tail.
        assert_eq!(displace_folder(&tree(), "r", 1, 2), Some(vec![2, 1]));
        assert_eq!(displace_folder(&tree(), "r", 2, 2), None);
        assert_eq!(displace_folder(&tree(), "r", 2, 99), None);
    }

    #[test]
    fn workspace_displacement_reaches_the_last_slot() {
        let mut rows = tree();
        rows.push(ws("s"));
        rows.push(wt("s", "home", 1));
        rows.push(ws("t"));
        // Only DB-backed headers (a `worktree_path`) are orderable.
        for r in rows.iter_mut().filter(|r| r.kind == RowKind::Workspace) {
            r.worktree_path = Some(format!("/repos/{}", r.workspace_slug));
        }
        assert_eq!(workspace_order(&rows), vec!["r", "s", "t"]);
        // First onto last ⇒ lands last.
        assert_eq!(
            displace_workspace(&rows, "r", "t"),
            Some(vec!["s".into(), "t".into(), "r".into()])
        );
        // Last onto first ⇒ lands first.
        assert_eq!(
            displace_workspace(&rows, "t", "r"),
            Some(vec!["t".into(), "r".into(), "s".into()])
        );
        assert_eq!(displace_workspace(&rows, "r", "r"), None);
        assert_eq!(displace_workspace(&rows, "r", "zz"), None);
    }

    #[test]
    fn block_end_spans_a_header_subtree() {
        let rows = tree(); // ws r, home, a, b, folder1, c, d, folder2, e
        assert_eq!(block_end(&rows, 0), 8, "the workspace header spans it all");
        assert_eq!(block_end(&rows, 1), 1, "a leaf worktree is its own block");
        assert_eq!(block_end(&rows, 4), 6, "folder 1 ends at its last child");
        assert_eq!(block_end(&rows, 7), 8, "folder 2 ends at its last child");
    }

    #[test]
    fn other_workspaces_are_untouched() {
        let mut rows = tree();
        rows.push(ws("s"));
        rows.push(wt("s", "home", 1));
        rows.push(wt("s", "z", 1));
        let p = step(&rows, "r", "/w/b", true).expect("moved");
        assert!(!p.order.contains(&"/w/z".to_string()));
        assert_eq!(runs(&rows, "s").len(), 1);
    }
}
