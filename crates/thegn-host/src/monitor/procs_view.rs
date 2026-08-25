//! Pure ordering/filter/tree logic for the monitor's Processes tab.
//!
//! Kept out of both the renderer ([`super::build`]) and the key handler
//! ([`super::MonitorOverlay`]) so there is exactly ONE definition of "which rows,
//! in what order" — the list the user sees and the list the signal action
//! indexes into cannot drift. Everything here is pure over a [`ProcSnapshot`]
//! plus the view toggles, so it is unit-testable without a live process table.

use super::{ProcSnapshotView, ProcSort};
use thegn_metrics::{ProcOwner, ProcSample, ProcSnapshot};

/// Depth cap for the tree walk — cheap insurance against a malformed / cyclic
/// parent chain (a pid whose ancestry loops back on itself).
const MAX_DEPTH: usize = 32;

/// One flattened row ready to render or act on.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcRow {
    pub pid: u32,
    pub name: String,
    pub owner: ProcOwner,
    pub cpu_pct: f32,
    pub rss_bytes: u64,
    /// Tree indent depth; `0` in flat mode and for tree roots.
    pub depth: usize,
    /// True in tree mode when this row's real parent fell outside the kept
    /// top-N set, so it is shown parented to the nearest kept ancestor (here,
    /// hoisted to a root) — the UI marks it so the elision is honest.
    pub elided_parent: bool,
}

/// The owner attribution as filterable keywords.
fn owner_key(o: ProcOwner) -> &'static str {
    match o {
        ProcOwner::Other => "",
        ProcOwner::ThegnSelf => "self thegn",
        ProcOwner::ThegnDaemon => "daemon thegn",
        ProcOwner::Pane(_) => "pane",
    }
}

/// The owner attribution as a display label for the `owner` column and the
/// signal-confirmation prompt. Empty for an unrelated process.
pub fn owner_label(o: ProcOwner) -> String {
    match o {
        ProcOwner::Other => String::new(),
        ProcOwner::ThegnSelf => "thegn".into(),
        ProcOwner::ThegnDaemon => "daemon".into(),
        ProcOwner::Pane(id) => format!("pane {id}"),
    }
}

/// Whether a process matches the (already-lowercased) filter fragment: by name,
/// pid, or owner attribution. An empty filter matches everything.
fn matches(p: &ProcSample, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    if p.name.to_ascii_lowercase().contains(filter) {
        return true;
    }
    if p.pid.to_string().contains(filter) {
        return true;
    }
    // Pane(3) → "pane 3" so both "pane" and the id match.
    let owner = match p.owner {
        ProcOwner::Pane(n) => format!("pane {n}"),
        other => owner_key(other).to_string(),
    };
    !owner.is_empty() && owner.contains(filter)
}

/// Compare two samples by the active sort key (ascending); the caller reverses
/// for descending. Mirrors [`super::build`]'s former inline comparator so the
/// flat and tree paths sort identically.
fn cmp(a: &ProcSample, b: &ProcSample, sort: ProcSort) -> std::cmp::Ordering {
    match sort {
        ProcSort::Cpu => a.cpu_pct.total_cmp(&b.cpu_pct),
        ProcSort::Rss => a.rss_bytes.cmp(&b.rss_bytes),
        // Name/pid invert so the *descending* default reads A→Z / high→low.
        ProcSort::Name => b.name.cmp(&a.name),
        ProcSort::Pid => b.pid.cmp(&a.pid),
    }
}

fn row_of(p: &ProcSample, depth: usize, elided_parent: bool) -> ProcRow {
    ProcRow {
        pid: p.pid,
        name: p.name.clone(),
        owner: p.owner,
        cpu_pct: p.cpu_pct,
        rss_bytes: p.rss_bytes,
        depth,
        elided_parent,
    }
}

/// The ordered, filtered rows for the current view.
///
/// - **Flat** (`view.tree == false`): filter, then sort by the active key.
/// - **Tree**: group by the sampled parent chain within the kept set; a row
///   whose parent fell outside that set is hoisted to a root and flagged
///   `elided_parent`. A non-empty filter keeps a row when it matches *or* has a
///   matching descendant, so filtering never severs a matching child from its
///   visible ancestry.
pub fn rows(snap: &ProcSnapshot, view: ProcSnapshotView) -> Vec<ProcRow> {
    let ProcSnapshotView {
        sort,
        desc,
        filter,
        tree,
    } = view;
    let filter = filter.trim().to_ascii_lowercase();

    if !tree {
        let mut kept: Vec<&ProcSample> =
            snap.procs.iter().filter(|p| matches(p, &filter)).collect();
        kept.sort_by(|a, b| {
            let ord = cmp(a, b, sort);
            if desc { ord.reverse() } else { ord }
        });
        return kept.iter().map(|p| row_of(p, 0, false)).collect();
    }

    tree_rows(snap, sort, desc, &filter)
}

/// Tree flatten: build children lists over the kept set, DFS from roots in sort
/// order, apply the "row or a descendant matches" filter, and cap depth.
fn tree_rows(snap: &ProcSnapshot, sort: ProcSort, desc: bool, filter: &str) -> Vec<ProcRow> {
    use std::collections::HashMap;

    let by_pid: HashMap<u32, &ProcSample> = snap.procs.iter().map(|p| (p.pid, p)).collect();
    let mut children: HashMap<u32, Vec<&ProcSample>> = HashMap::new();
    let mut roots: Vec<&ProcSample> = Vec::new();
    for p in &snap.procs {
        match p.ppid {
            // A parent inside the kept set nests; otherwise this is a root
            // (its real parent was pruned by the top-N cap).
            Some(ppid) if ppid != p.pid && by_pid.contains_key(&ppid) => {
                children.entry(ppid).or_default().push(p);
            }
            _ => roots.push(p),
        }
    }

    let order = |v: &mut Vec<&ProcSample>| {
        v.sort_by(|a, b| {
            let ord = cmp(a, b, sort);
            if desc { ord.reverse() } else { ord }
        });
    };
    order(&mut roots);
    for v in children.values_mut() {
        order(v);
    }

    // "Keep if it or a descendant matches" — precompute the retained set so the
    // DFS can prune whole subtrees that contain no match.
    let retained = retained_set(&by_pid, filter);

    let mut out = Vec::new();
    let mut visited = std::collections::HashSet::new();
    for r in &roots {
        walk(
            r,
            0,
            r.ppid.is_some(),
            &children,
            &retained,
            &mut visited,
            &mut out,
        );
    }
    out
}

/// DFS one subtree, emitting retained rows with their depth.
fn walk<'a>(
    p: &'a ProcSample,
    depth: usize,
    elided_parent: bool,
    children: &std::collections::HashMap<u32, Vec<&'a ProcSample>>,
    retained: &std::collections::HashSet<u32>,
    visited: &mut std::collections::HashSet<u32>,
    out: &mut Vec<ProcRow>,
) {
    if depth >= MAX_DEPTH || !visited.insert(p.pid) {
        return;
    }
    if retained.contains(&p.pid) {
        out.push(row_of(p, depth, elided_parent));
    }
    if let Some(kids) = children.get(&p.pid) {
        for k in kids {
            walk(k, depth + 1, false, children, retained, visited, out);
        }
    }
}

/// Pids to keep under a filter: a process matches, or any descendant does —
/// enforced by walking every match up to its ancestors within the kept set. An
/// empty filter keeps everything.
fn retained_set(
    by_pid: &std::collections::HashMap<u32, &ProcSample>,
    filter: &str,
) -> std::collections::HashSet<u32> {
    let mut keep = std::collections::HashSet::new();
    if filter.is_empty() {
        keep.extend(by_pid.keys().copied());
        return keep;
    }
    // Any matching process pulls in every ancestor within the kept set.
    for p in by_pid.values() {
        if matches(p, filter) {
            let mut cur = Some(*p);
            let mut guard = 0;
            while let Some(node) = cur {
                if !keep.insert(node.pid) || guard >= MAX_DEPTH {
                    break;
                }
                guard += 1;
                cur = node
                    .ppid
                    .filter(|pp| *pp != node.pid)
                    .and_then(|pp| by_pid.get(&pp).copied());
            }
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(pid: u32, ppid: Option<u32>, name: &str, cpu: f32, rss: u64) -> ProcSample {
        ProcSample {
            pid,
            ppid,
            name: name.into(),
            cpu_pct: cpu,
            rss_bytes: rss,
            run_secs: 0,
            owner: ProcOwner::Other,
        }
    }

    fn snap(procs: Vec<ProcSample>) -> ProcSnapshot {
        ProcSnapshot {
            total: procs.len(),
            procs,
            primed: true,
            enabled: true,
        }
    }

    fn view(filter: &str, tree: bool) -> ProcSnapshotView {
        ProcSnapshotView {
            sort: ProcSort::Cpu,
            desc: true,
            filter: filter.to_string(),
            tree,
        }
    }

    #[test]
    fn flat_filter_matches_name_pid_and_owner() {
        let mut cargo = sample(100, None, "cargo", 90.0, 1);
        cargo.owner = ProcOwner::Pane(3);
        let procs = vec![
            cargo,
            sample(200, None, "zsh", 1.0, 1),
            sample(4242, None, "helix", 2.0, 1),
        ];
        let s = snap(procs);
        // By name.
        let r = rows(&s, view("carg", false));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].pid, 100);
        // By pid fragment.
        assert_eq!(rows(&s, view("4242", false)).len(), 1);
        // By owner ("pane").
        let owned = rows(&s, view("pane", false));
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].pid, 100);
        // Empty filter keeps all, sorted by cpu desc.
        let all = rows(&s, view("", false));
        assert_eq!(
            all.iter().map(|r| r.pid).collect::<Vec<_>>(),
            [100, 4242, 200]
        );
    }

    #[test]
    fn tree_nests_children_and_flags_elided_roots() {
        // 10 (kept) → 20 → 30; plus 40 whose parent 999 is NOT in the set.
        let procs = vec![
            sample(10, None, "root", 5.0, 1),
            sample(20, Some(10), "mid", 4.0, 1),
            sample(30, Some(20), "leaf", 3.0, 1),
            sample(40, Some(999), "orphan", 9.0, 1),
        ];
        let s = snap(procs);
        let r = rows(&s, view("", true));
        // Orphan sorts first (highest cpu) as an elided root; then the 10-subtree.
        let by_pid: Vec<u32> = r.iter().map(|x| x.pid).collect();
        assert_eq!(by_pid, [40, 10, 20, 30]);
        let orphan = r.iter().find(|x| x.pid == 40).unwrap();
        assert_eq!(orphan.depth, 0);
        assert!(
            orphan.elided_parent,
            "parent outside kept set → elided root"
        );
        assert_eq!(r.iter().find(|x| x.pid == 20).unwrap().depth, 1);
        assert_eq!(r.iter().find(|x| x.pid == 30).unwrap().depth, 2);
        assert!(!r.iter().find(|x| x.pid == 10).unwrap().elided_parent);
    }

    #[test]
    fn tree_filter_keeps_ancestry_of_a_match() {
        // Filtering for the leaf must still show its ancestors for context.
        let procs = vec![
            sample(10, None, "root", 5.0, 1),
            sample(20, Some(10), "mid", 4.0, 1),
            sample(30, Some(20), "needle", 3.0, 1),
            sample(50, None, "unrelated", 1.0, 1),
        ];
        let s = snap(procs);
        let r = rows(&s, view("needle", true));
        let pids: Vec<u32> = r.iter().map(|x| x.pid).collect();
        assert_eq!(pids, [10, 20, 30], "ancestry retained, unrelated dropped");
    }

    #[test]
    fn a_cyclic_parent_chain_terminates() {
        // Two processes each claiming the other as parent must not loop forever.
        let procs = vec![
            sample(1, Some(2), "a", 1.0, 1),
            sample(2, Some(1), "b", 1.0, 1),
        ];
        let s = snap(procs);
        let r = rows(&s, view("", true));
        // Both appear exactly once; the walk terminates.
        assert_eq!(r.len(), 2);
    }
}
