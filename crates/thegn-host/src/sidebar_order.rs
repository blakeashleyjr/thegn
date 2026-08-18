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
fn locate(runs: &[Run], path: &str) -> Option<(usize, usize)> {
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

/// Land `path` in `dest`'s run, immediately before `before` (or at the end of
/// the run when `before` is `None`) — the mouse drop entry point.
///
/// Returns `None` when the drop is a no-op or would place a worktree above the
/// anchored `home` row.
pub(crate) fn drop_at(
    rows: &[SidebarRow],
    slug: &str,
    path: &str,
    dest: Option<i64>,
    before: Option<&str>,
) -> Option<Plan> {
    let mut runs = runs(rows, slug);
    let prev = flatten(&runs);
    let (ri, mi) = locate(&runs, path)?;
    if runs[ri].members[mi].home {
        return None;
    }
    let target = runs.iter().position(|r| r.folder == dest)?;

    let member = runs[ri].members.remove(mi);
    let at = match before {
        Some(b) => match runs[target].members.iter().position(|m| m.path == b) {
            // Never above home.
            Some(0) if runs[target].members[0].home => {
                runs[ri].members.insert(mi, member);
                return None;
            }
            Some(i) => i,
            // The anchor vanished mid-drag (filed or deleted): drop the move
            // rather than guessing a slot.
            None => {
                runs[ri].members.insert(mi, member);
                return None;
            }
        },
        None => runs[target].members.len(),
    };
    runs[target].members.insert(at, member);

    let refile = (runs[target].folder != runs[ri].folder || target != ri).then_some(dest);
    plan_from(runs, &prev, path, refile)
}

/// The run the worktree at `path` currently belongs to: `Some(None)` for the
/// loose list, `Some(Some(id))` for a folder, `None` when it isn't in `slug`.
pub(crate) fn run_of(rows: &[SidebarRow], slug: &str, path: &str) -> Option<Option<i64>> {
    let runs = runs(rows, slug);
    let (ri, _) = locate(&runs, path)?;
    Some(runs[ri].folder)
}

/// The path of the member after `path` within its own run — the anchor a
/// bottom-half drop inserts before. `None` at the tail of the run.
pub(crate) fn next_in_run(rows: &[SidebarRow], slug: &str, path: &str) -> Option<String> {
    in_run_neighbor(rows, slug, path, false)
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

/// Land folder `fid` immediately before folder `before` (or last when `None`) —
/// the folder-drag drop. Returns the new order, or `None` if nothing moved.
pub(crate) fn drop_folder_at(
    rows: &[SidebarRow],
    slug: &str,
    fid: i64,
    before: Option<i64>,
) -> Option<Vec<i64>> {
    let mut order = folder_order(rows, slug);
    let i = order.iter().position(|f| *f == fid)?;
    let prev = order.clone();
    order.remove(i);
    let at = match before {
        Some(b) => order.iter().position(|f| *f == b)?,
        None => order.len(),
    };
    order.insert(at, fid);
    (order != prev).then_some(order)
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
    fn drop_into_a_folder_before_a_sibling_refiles_and_positions() {
        let p = drop_at(&tree(), "r", "/w/a", Some(1), Some("/w/d")).expect("moved");
        assert_eq!(p.refile, Some(Some(1)));
        assert_eq!(p.order, paths(&["home", "b", "c", "a", "d", "e"]));
    }

    #[test]
    fn drop_at_the_end_of_a_run_appends() {
        let p = drop_at(&tree(), "r", "/w/a", Some(2), None).expect("moved");
        assert_eq!(p.refile, Some(Some(2)));
        assert_eq!(p.order, paths(&["home", "b", "c", "d", "e", "a"]));
    }

    #[test]
    fn drop_never_lands_above_home() {
        assert_eq!(drop_at(&tree(), "r", "/w/c", None, Some("/w/home")), None);
    }

    #[test]
    fn drop_onto_a_vanished_anchor_is_refused() {
        assert_eq!(
            drop_at(&tree(), "r", "/w/a", Some(1), Some("/w/gone")),
            None
        );
    }

    #[test]
    fn drop_within_the_same_run_reorders_without_refiling() {
        let p = drop_at(&tree(), "r", "/w/d", Some(1), Some("/w/c")).expect("moved");
        assert_eq!(p.refile, None);
        assert_eq!(p.order, paths(&["home", "a", "b", "d", "c", "e"]));
    }

    #[test]
    fn folder_steps_and_drops() {
        assert_eq!(folder_order(&tree(), "r"), vec![1, 2]);
        assert_eq!(step_folder(&tree(), "r", 2, true), Some(vec![2, 1]));
        assert_eq!(step_folder(&tree(), "r", 1, true), None);
        assert_eq!(step_folder(&tree(), "r", 2, false), None);
        assert_eq!(drop_folder_at(&tree(), "r", 2, Some(1)), Some(vec![2, 1]));
        assert_eq!(drop_folder_at(&tree(), "r", 1, None), Some(vec![2, 1]));
        assert_eq!(drop_folder_at(&tree(), "r", 2, None), None);
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
