//! CPU / memory attribution tree for the `cpu` and `mem` bar popups (THE-2025).
//!
//! Pure: the host gathers who owns each pane (workspace → worktree → tab →
//! program) and the sampler's per-pane totals, and this module folds them into
//! one ordered, bounded list of rows:
//!
//! ```text
//! workspace            ← sum of its worktrees
//!   worktree           ← sum of its panes
//!     tab · program    ← the pane's process-tree total
//!       proc (pid)     ← the heaviest sampled processes inside that pane
//!     gate / agent     ← the worktree's named background scopes (cgroup v2)
//! unattributed panes   ← panes whose owner row is gone (closed since the sample)
//! thegn                ← UI + daemon
//!   gate · <slug>      ← a named scope whose worktree has no pane here
//!   thegn jobs (unnamed) ← the slice's anonymous `run-*.scope`s
//! outside thegn        ← system total minus everything above, never negative
//! ```
//!
//! Scope figures come from `thegn.slice`'s cgroups, while pane and thegn
//! figures come from the process tree, and the two overlap (a capped pane runs
//! inside a `run-*.scope`; a gate is usually a child of the UI). So the scope
//! rows are a breakdown, not an addend: the thegn row stays UI + daemon, and
//! "outside thegn" subtracts whichever of the process-tree sum and the scope sum
//! is larger, counting a scope's processes once.
//!
//! CPU is percent of ONE core (a per-core sum, like `top`), so a busy pane can
//! read above 100. Missing data stays `None` all the way to the screen (`—`),
//! never a confident `0`. Core does not depend on `thegn-metrics`; the inputs
//! are plain structs the host fills.

use std::cmp::Ordering;
use std::collections::BTreeMap;

pub use crate::sandbox_cpucap::ScopeKind;
use crate::sandbox_cpucap::scope_slug;

/// Most rows a tree renders, the "+N more" row included. Workspaces, worktrees
/// and the thegn/outside tail are always kept; processes are trimmed first,
/// then panes.
pub const MAX_ROWS: usize = 64;

/// Default number of processes listed under each pane.
pub const TOP_PER_PANE: usize = 3;

/// Most named scopes listed under the thegn group (their worktree has no pane
/// here); the rest fold into the unnamed row.
pub const MAX_LOOSE_JOBS: usize = 8;

/// Most worktrees [`build_compact`] lists; idle and trimmed worktrees fold
/// into one summary row.
pub const TOP_WORKTREES: usize = 8;

/// Label of the row summing the slice's anonymous scopes.
pub const UNNAMED_JOBS: &str = "thegn jobs (unnamed)";

/// One CPU + resident-memory reading. CPU is percent of one core.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub cpu_pct: Option<f32>,
    pub rss: Option<u64>,
}

impl Usage {
    pub fn new(cpu_pct: Option<f32>, rss: Option<u64>) -> Self {
        Self { cpu_pct, rss }
    }

    /// Sum two readings. A side that is unknown contributes nothing; the sum is
    /// unknown only when both sides are.
    fn plus(self, other: Usage) -> Usage {
        Usage {
            cpu_pct: match (self.cpu_pct, other.cpu_pct) {
                (Some(a), Some(b)) => Some(a + b),
                (a, None) => a,
                (None, b) => b,
            },
            rss: match (self.rss, other.rss) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (a, None) => a,
                (None, b) => b,
            },
        }
    }
}

/// Where a pane lives, as the host's session index sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneOwner {
    pub pane_id: u32,
    pub workspace: String,
    pub worktree: String,
    /// The worktree's directory, when it has one (terminal groups do not).
    /// Named scopes match a worktree by this path's basename slug.
    pub worktree_path: Option<String>,
    pub tab: String,
    /// The launched program's short name (an agent, an editor, a shell).
    pub program: Option<String>,
}

/// One pane's process-tree total from the sampler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneUsage {
    pub pane_id: u32,
    pub usage: Usage,
}

/// Who a sampled process belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Pane(u32),
    Ui,
    Daemon,
    Other,
}

/// One of the sampler's top processes.
#[derive(Debug, Clone, PartialEq)]
pub struct TopProc {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f32,
    pub rss: u64,
    pub owner: Owner,
}

/// thegn's own processes and the measured "everything else" bucket.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SelfUsage {
    pub ui: Usage,
    pub daemon: Usage,
    /// The sampler's `Other` owner class; the fallback for "outside thegn" when
    /// a system total or the attributed sum is unknown.
    pub other: Usage,
}

/// One transient scope inside `thegn.slice`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeUsage {
    /// `None` for the anonymous `run-*.scope` sum.
    pub kind: Option<ScopeKind>,
    /// The worktree slug parsed from the unit name (`None` when unnamed).
    pub worktree: Option<String>,
    /// Percent of one core.
    pub cpu_pct: Option<f32>,
    pub rss: Option<u64>,
}

/// Everything [`build`] reads.
#[derive(Debug, Clone, Copy)]
pub struct AttribInputs<'a> {
    pub owners: &'a [PaneOwner],
    /// Per-pane totals. `None` when the platform's sampler cannot split the
    /// pane bucket (the tree is then marked partial and pane rows read `—`).
    pub panes: Option<&'a [PaneUsage]>,
    pub procs: &'a [TopProc],
    pub thegn: SelfUsage,
    /// The masthead totals: CPU as percent of ALL cores (0–100), used memory
    /// in bytes.
    pub system: Usage,
    pub cores: usize,
    /// Per-scope usage inside `thegn.slice` (empty without cgroup v2).
    pub scopes: &'a [ScopeUsage],
}

/// What the tree is ordered by (heaviest first at every level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Cpu,
    Mem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Workspace,
    Worktree,
    Pane,
    Proc,
    Thegn,
    /// A named background scope (gate/agent) or the unnamed scope sum.
    Job,
    Outside,
    /// The "+N more" row standing in for trimmed processes/panes.
    More,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttribRow {
    pub depth: u8,
    pub kind: RowKind,
    pub label: String,
    /// Percent of one core.
    pub cpu_pct: Option<f32>,
    /// This row's fraction (0–1) of the masthead total for the sort key.
    pub share_of_total: Option<f32>,
    pub rss: Option<u64>,
    /// The pid for a `Proc` row and the pane id for a `Pane` row; `None`
    /// elsewhere. Lets the host jump to the row without parsing its label.
    pub target: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttribTree {
    pub rows: Vec<AttribRow>,
    /// Per-pane totals were unavailable: pane-level figures are unknown.
    pub partial: bool,
    /// "outside thegn" came out negative and was clamped to zero (shared pages
    /// counted per process, or the system and process samples disagree).
    pub clamped: bool,
    /// The masthead totals in row units (CPU as percent of one core).
    pub total: Usage,
}

struct PaneNode<'a> {
    owner: &'a PaneOwner,
    usage: Usage,
}

struct Group<'a, T> {
    name: &'a str,
    usage: Usage,
    children: Vec<T>,
    /// Worktree groups: the slug named scopes are matched against.
    slug: Option<String>,
    /// Worktree groups: their named scopes, one per kind.
    jobs: Vec<(ScopeKind, Usage)>,
}

/// Per-field larger of two readings; an unknown side contributes nothing.
fn at_least(a: Usage, b: Usage) -> Usage {
    let max_f = |x: Option<f32>, y: Option<f32>| match (x, y) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    };
    let max_u = |x: Option<u64>, y: Option<u64>| match (x, y) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (x, None) => x,
        (None, y) => y,
    };
    Usage {
        cpu_pct: max_f(a.cpu_pct, b.cpu_pct),
        rss: max_u(a.rss, b.rss),
    }
}

/// Merge `usage` into the entry for `key`, appending one if absent.
fn merge_job<K: PartialEq>(jobs: &mut Vec<(K, Usage)>, key: K, usage: Usage) {
    match jobs.iter_mut().find(|(k, _)| *k == key) {
        Some((_, total)) => *total = total.plus(usage),
        None => jobs.push((key, usage)),
    }
}

fn sort_value(u: Usage, sort: SortKey) -> f64 {
    match sort {
        SortKey::Cpu => u.cpu_pct.map_or(-1.0, f64::from),
        SortKey::Mem => u.rss.map_or(-1.0, |r| r as f64),
    }
}

/// Heaviest first; unknown sorts last; ties break on the label so the order is
/// stable across samples.
fn by_weight(a: Usage, a_label: &str, b: Usage, b_label: &str, sort: SortKey) -> Ordering {
    sort_value(b, sort)
        .total_cmp(&sort_value(a, sort))
        .then_with(|| a_label.cmp(b_label))
}

fn pane_label(owner: &PaneOwner) -> String {
    match owner.program.as_deref().filter(|p| !p.is_empty()) {
        Some(program) => format!("{} · {program}", owner.tab),
        None => owner.tab.clone(),
    }
}

/// `value / total`, or `None` when either is unknown or the total is not
/// positive.
fn share(value: Usage, total: Usage, sort: SortKey) -> Option<f32> {
    match sort {
        SortKey::Cpu => {
            let (v, t) = (value.cpu_pct?, total.cpu_pct?);
            (t > 0.0).then_some(v / t)
        }
        SortKey::Mem => {
            let (v, t) = (value.rss?, total.rss?);
            (t > 0).then_some((v as f64 / t as f64) as f32)
        }
    }
}

/// Build the attribution tree. `top_per_pane` caps the process rows under each
/// pane (see [`TOP_PER_PANE`]); the whole list is capped at [`MAX_ROWS`].
pub fn build(inputs: &AttribInputs<'_>, sort: SortKey, top_per_pane: usize) -> AttribTree {
    build_inner(inputs, sort, top_per_pane, None)
}

/// [`build`] for the masthead popups: worktrees with no readings (no pane
/// totals, background jobs or sampled processes) are dropped, only the
/// `max_worktrees` heaviest of the rest (across all workspaces) are listed,
/// workspaces left empty go, and one "N idle worktrees" row stands in for the
/// dropped ones. Folding happens before the [`MAX_ROWS`] cap, so hundreds of
/// idle worktrees never squeeze out the live ones' panes and processes.
pub fn build_compact(
    inputs: &AttribInputs<'_>,
    sort: SortKey,
    top_per_pane: usize,
    max_worktrees: usize,
) -> AttribTree {
    build_inner(inputs, sort, top_per_pane, Some(max_worktrees))
}

/// Whether a worktree has anything live to show.
fn has_readings(wt: &Group<'_, PaneNode<'_>>, procs: &[TopProc]) -> bool {
    wt.usage.cpu_pct.is_some()
        || wt.usage.rss.is_some()
        || !wt.jobs.is_empty()
        || wt.children.iter().any(|pane| {
            procs
                .iter()
                .any(|p| p.owner == Owner::Pane(pane.owner.pane_id))
        })
}

/// Drop worktrees without readings, keep the `max` heaviest of the rest, and
/// drop workspaces left empty. Returns `(trimmed, idle)` counts.
fn fold_worktrees<'a>(
    workspaces: &mut Vec<Group<'a, Group<'a, PaneNode<'a>>>>,
    procs: &[TopProc],
    sort: SortKey,
    max: usize,
) -> (usize, usize) {
    let mut idle = 0;
    for ws in workspaces.iter_mut() {
        let before = ws.children.len();
        ws.children.retain(|wt| has_readings(wt, procs));
        idle += before - ws.children.len();
    }
    let mut ranked: Vec<(Usage, &'a str, &'a str)> = Vec::new();
    for ws in workspaces.iter() {
        for wt in &ws.children {
            ranked.push((wt.usage, ws.name, wt.name));
        }
    }
    ranked.sort_by(|a, b| by_weight(a.0, a.2, b.0, b.2, sort).then_with(|| a.1.cmp(b.1)));
    let trimmed = ranked.len().saturating_sub(max);
    ranked.truncate(max);
    for ws in workspaces.iter_mut() {
        let ws_name = ws.name;
        ws.children.retain(|wt| {
            ranked
                .iter()
                .any(|(_, w, t)| *w == ws_name && *t == wt.name)
        });
    }
    workspaces.retain(|ws| !ws.children.is_empty());
    (trimmed, idle)
}

/// The summary row's label for `trimmed` live and `idle` worktrees.
fn fold_label(trimmed: usize, idle: usize) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    match (trimmed, idle) {
        (t, 0) => format!("+{t} more worktree{}", plural(t)),
        (0, i) => format!("{i} idle worktree{}", plural(i)),
        (t, i) => format!("+{t} more · {i} idle worktree{}", plural(i)),
    }
}

fn build_inner(
    inputs: &AttribInputs<'_>,
    sort: SortKey,
    top_per_pane: usize,
    max_worktrees: Option<usize>,
) -> AttribTree {
    let partial = inputs.panes.is_none();
    let pane_usage: BTreeMap<u32, Usage> = inputs
        .panes
        .unwrap_or(&[])
        .iter()
        .map(|p| (p.pane_id, p.usage))
        .collect();

    // Group owners: workspace → worktree → pane, in first-seen order, then
    // sorted by weight below. A duplicate pane id keeps its first owner.
    let mut seen = std::collections::BTreeSet::new();
    let mut workspaces: Vec<Group<'_, Group<'_, PaneNode<'_>>>> = Vec::new();
    for owner in inputs.owners {
        if !seen.insert(owner.pane_id) {
            continue;
        }
        let usage = pane_usage.get(&owner.pane_id).copied().unwrap_or_default();
        let ws_index = match workspaces.iter().position(|w| w.name == owner.workspace) {
            Some(i) => i,
            None => {
                workspaces.push(Group {
                    name: &owner.workspace,
                    usage: Usage::default(),
                    children: Vec::new(),
                    slug: None,
                    jobs: Vec::new(),
                });
                workspaces.len() - 1
            }
        };
        let ws = &mut workspaces[ws_index];
        ws.usage = ws.usage.plus(usage);
        let wt_index = match ws.children.iter().position(|w| w.name == owner.worktree) {
            Some(i) => i,
            None => {
                ws.children.push(Group {
                    name: &owner.worktree,
                    usage: Usage::default(),
                    children: Vec::new(),
                    slug: Some(scope_slug(
                        owner.worktree_path.as_deref().unwrap_or(&owner.worktree),
                    )),
                    jobs: Vec::new(),
                });
                ws.children.len() - 1
            }
        };
        let wt = &mut ws.children[wt_index];
        wt.usage = wt.usage.plus(usage);
        wt.children.push(PaneNode { owner, usage });
    }

    // Named scopes join the worktree whose slug matches (and roll up into its
    // sums); the rest list under thegn. Anonymous scopes sum into one row.
    let mut scopes_sum: Option<Usage> = None;
    let mut loose: Vec<((ScopeKind, &str), Usage)> = Vec::new();
    let mut unnamed: Option<Usage> = None;
    for scope in inputs.scopes {
        let usage = Usage::new(scope.cpu_pct, scope.rss);
        scopes_sum = Some(scopes_sum.unwrap_or_default().plus(usage));
        let (Some(kind), Some(slug)) = (scope.kind, scope.worktree.as_deref()) else {
            unnamed = Some(unnamed.unwrap_or_default().plus(usage));
            continue;
        };
        let home = workspaces.iter_mut().find_map(|ws| {
            let wt = ws
                .children
                .iter_mut()
                .find(|wt| wt.slug.as_deref() == Some(slug))?;
            Some((&mut ws.usage, wt))
        });
        match home {
            Some((ws_usage, wt)) => {
                *ws_usage = ws_usage.plus(usage);
                wt.usage = wt.usage.plus(usage);
                merge_job(&mut wt.jobs, kind, usage);
            }
            None => merge_job(&mut loose, (kind, slug), usage),
        }
    }
    loose.sort_by(|a, b| by_weight(a.1, a.0.1, b.1, b.0.1, sort).then(a.0.0.cmp(&b.0.0)));
    for (_, usage) in loose.drain(MAX_LOOSE_JOBS.min(loose.len())..) {
        unnamed = Some(unnamed.unwrap_or_default().plus(usage));
    }

    workspaces.sort_by(|a, b| by_weight(a.usage, a.name, b.usage, b.name, sort));
    let folded = max_worktrees
        .map(|max| fold_worktrees(&mut workspaces, inputs.procs, sort, max))
        .filter(|(trimmed, idle)| trimmed + idle > 0);

    let mut tree_rows: Vec<AttribRow> = Vec::new();
    let row_t =
        |depth: u8, kind: RowKind, label: String, usage: Usage, target: Option<u32>| AttribRow {
            depth,
            kind,
            label,
            cpu_pct: usage.cpu_pct,
            share_of_total: None,
            rss: usage.rss,
            target,
        };
    let row = |depth: u8, kind: RowKind, label: String, usage: Usage| {
        row_t(depth, kind, label, usage, None)
    };
    for ws in &mut workspaces {
        tree_rows.push(row(0, RowKind::Workspace, ws.name.to_string(), ws.usage));
        ws.children
            .sort_by(|a, b| by_weight(a.usage, a.name, b.usage, b.name, sort));
        for wt in &mut ws.children {
            tree_rows.push(row(1, RowKind::Worktree, wt.name.to_string(), wt.usage));
            wt.children.sort_by(|a, b| {
                by_weight(a.usage, &a.owner.tab, b.usage, &b.owner.tab, sort)
                    .then_with(|| a.owner.pane_id.cmp(&b.owner.pane_id))
            });
            wt.jobs
                .sort_by(|a, b| by_weight(a.1, a.0.as_str(), b.1, b.0.as_str(), sort));
            for (kind, usage) in &wt.jobs {
                tree_rows.push(row(2, RowKind::Job, kind.as_str().to_string(), *usage));
            }
            for pane in &wt.children {
                tree_rows.push(row_t(
                    2,
                    RowKind::Pane,
                    pane_label(pane.owner),
                    pane.usage,
                    Some(pane.owner.pane_id),
                ));
                let mut procs: Vec<&TopProc> = inputs
                    .procs
                    .iter()
                    .filter(|p| p.owner == Owner::Pane(pane.owner.pane_id))
                    .collect();
                procs.sort_by(|a, b| {
                    let (ua, ub) = (proc_usage(a), proc_usage(b));
                    by_weight(ua, &a.name, ub, &b.name, sort).then_with(|| a.pid.cmp(&b.pid))
                });
                for p in procs.into_iter().take(top_per_pane) {
                    tree_rows.push(row_t(
                        3,
                        RowKind::Proc,
                        format!("{} ({})", p.name, p.pid),
                        proc_usage(p),
                        Some(p.pid),
                    ));
                }
            }
        }
    }
    if let Some((trimmed, idle)) = folded {
        tree_rows.push(row(
            0,
            RowKind::More,
            fold_label(trimmed, idle),
            Usage::default(),
        ));
    }

    // Panes the sampler saw but no owner row claims (closed since the sample).
    let unattributed = pane_usage
        .iter()
        .filter(|(id, _)| !seen.contains(*id))
        .fold(None::<Usage>, |acc, (_, u)| {
            Some(acc.unwrap_or_default().plus(*u))
        });
    if let Some(u) = unattributed {
        tree_rows.push(row(0, RowKind::Workspace, "unattributed panes".into(), u));
    }

    // Masthead totals in row units: the system CPU is a percent of all cores.
    let total = Usage {
        cpu_pct: inputs.system.cpu_pct.and_then(|pct| {
            let cores = u16::try_from(inputs.cores).unwrap_or(u16::MAX);
            (cores > 0).then_some(pct * f32::from(cores))
        }),
        rss: inputs.system.rss,
    };

    let panes_sum = pane_usage
        .values()
        .fold(Usage::default(), |acc, u| acc.plus(*u));
    let thegn = inputs.thegn.ui.plus(inputs.thegn.daemon);
    // Scopes overlap the process tree, so count whichever sum is larger.
    let attributed = at_least(panes_sum.plus(thegn), scopes_sum.unwrap_or_default());
    let mut clamped = false;
    let outside = if partial {
        inputs.thegn.other
    } else {
        Usage {
            cpu_pct: match (total.cpu_pct, attributed.cpu_pct) {
                (Some(t), Some(a)) => {
                    let v = t - a;
                    if v < 0.0 {
                        clamped = true;
                    }
                    Some(v.max(0.0))
                }
                _ => inputs.thegn.other.cpu_pct,
            },
            rss: match (total.rss, attributed.rss) {
                (Some(t), Some(a)) => {
                    if a > t {
                        clamped = true;
                    }
                    Some(t.saturating_sub(a))
                }
                _ => inputs.thegn.other.rss,
            },
        }
    };

    let mut tail = vec![
        row(0, RowKind::Thegn, "thegn".into(), thegn),
        row(1, RowKind::Thegn, "ui".into(), inputs.thegn.ui),
        row(1, RowKind::Thegn, "daemon".into(), inputs.thegn.daemon),
    ];
    for ((kind, slug), usage) in &loose {
        tail.push(row(
            1,
            RowKind::Job,
            format!("{} · {slug}", kind.as_str()),
            *usage,
        ));
    }
    if let Some(usage) = unnamed {
        tail.push(row(1, RowKind::Job, UNNAMED_JOBS.into(), usage));
    }
    tail.push(row(0, RowKind::Outside, "outside thegn".into(), outside));

    let budget = MAX_ROWS.saturating_sub(tail.len());
    let mut rows = cap_rows(tree_rows, budget);
    rows.extend(tail);
    for r in &mut rows {
        r.share_of_total = share(Usage::new(r.cpu_pct, r.rss), total, sort);
    }
    AttribTree {
        rows,
        partial,
        clamped,
        total,
    }
}

fn proc_usage(p: &TopProc) -> Usage {
    Usage::new(Some(p.cpu_pct), Some(p.rss))
}

/// Trim `rows` to `budget` (the "+N more" row included): process rows go first,
/// from the bottom up, then pane rows; workspaces and worktrees always stay.
fn cap_rows(rows: Vec<AttribRow>, budget: usize) -> Vec<AttribRow> {
    if rows.len() <= budget {
        return rows;
    }
    let mut excess = rows.len() - budget.saturating_sub(1);
    let mut keep = vec![true; rows.len()];
    for kind in [RowKind::Proc, RowKind::Job, RowKind::Pane] {
        for (i, r) in rows.iter().enumerate().rev() {
            if excess == 0 {
                break;
            }
            if r.kind == kind && keep[i] {
                keep[i] = false;
                excess -= 1;
            }
        }
    }
    let dropped = keep.iter().filter(|k| !**k).count();
    let mut out: Vec<AttribRow> = rows
        .into_iter()
        .zip(keep)
        .filter_map(|(r, k)| k.then_some(r))
        .collect();
    out.push(AttribRow {
        depth: 0,
        kind: RowKind::More,
        label: format!("+{dropped} more"),
        cpu_pct: None,
        share_of_total: None,
        rss: None,
        target: None,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::{
        AttribInputs, AttribRow, MAX_LOOSE_JOBS, MAX_ROWS, Owner, PaneOwner, PaneUsage, RowKind,
        ScopeKind, ScopeUsage, SelfUsage, SortKey, TOP_PER_PANE, TOP_WORKTREES, TopProc,
        UNNAMED_JOBS, Usage, at_least, build, build_compact, pane_label,
    };

    const EPS: f32 = 1e-4;

    fn owner(id: u32, ws: &str, wt: &str, tab: &str, program: Option<&str>) -> PaneOwner {
        PaneOwner {
            pane_id: id,
            workspace: ws.into(),
            worktree: wt.into(),
            worktree_path: Some(format!("/repos/{}", wt.replace('/', "-"))),
            tab: tab.into(),
            program: program.map(str::to_string),
        }
    }

    fn pane(id: u32, cpu: f32, rss: u64) -> PaneUsage {
        PaneUsage {
            pane_id: id,
            usage: Usage::new(Some(cpu), Some(rss)),
        }
    }

    fn proc_row(pid: u32, name: &str, cpu: f32, rss: u64, owner: Owner) -> TopProc {
        TopProc {
            pid,
            name: name.into(),
            cpu_pct: cpu,
            rss,
            owner,
        }
    }

    fn thegn() -> SelfUsage {
        SelfUsage {
            ui: Usage::new(Some(5.0), Some(100)),
            daemon: Usage::new(Some(1.0), Some(50)),
            other: Usage::new(Some(7.0), Some(70)),
        }
    }

    fn find<'a>(rows: &'a [AttribRow], label: &str) -> &'a AttribRow {
        rows.iter()
            .find(|r| r.label == label)
            .unwrap_or_else(|| panic!("no row {label}"))
    }

    fn close(a: Option<f32>, b: f32) -> bool {
        a.is_some_and(|a| (a - b).abs() < EPS)
    }

    fn fixture() -> (Vec<PaneOwner>, Vec<PaneUsage>, Vec<TopProc>) {
        let owners = vec![
            owner(1, "alpha", "alpha/main", "1", Some("claude")),
            owner(2, "alpha", "alpha/main", "2", None),
            owner(3, "alpha", "alpha/feat", "1", Some("nvim")),
            // A parked workspace's pane.
            owner(4, "beta", "beta/main", "1", Some("cargo")),
        ];
        let panes = vec![
            pane(1, 50.0, 1_000),
            pane(2, 10.0, 9_000),
            pane(3, 5.0, 500),
            pane(4, 120.0, 2_000),
        ];
        let procs = vec![
            proc_row(11, "node", 40.0, 800, Owner::Pane(1)),
            proc_row(12, "rg", 9.0, 100, Owner::Pane(1)),
            proc_row(13, "git", 0.5, 50, Owner::Pane(1)),
            proc_row(14, "sh", 0.1, 10, Owner::Pane(1)),
            proc_row(41, "rustc", 110.0, 1_800, Owner::Pane(4)),
            proc_row(90, "thegn", 5.0, 100, Owner::Ui),
            proc_row(91, "firefox", 30.0, 5_000, Owner::Other),
        ];
        (owners, panes, procs)
    }

    fn inputs<'a>(
        owners: &'a [PaneOwner],
        panes: Option<&'a [PaneUsage]>,
        procs: &'a [TopProc],
    ) -> AttribInputs<'a> {
        AttribInputs {
            owners,
            panes,
            procs,
            thegn: thegn(),
            // 25% of 8 cores = 200% of one core; 20_000 bytes used.
            system: Usage::new(Some(25.0), Some(20_000)),
            cores: 8,
            scopes: &[],
        }
    }

    #[test]
    fn row_targets_are_pid_for_procs_and_pane_id_for_panes() {
        let (owners, panes, procs) = fixture();
        let tree = build(
            &inputs(&owners, Some(panes.as_slice()), &procs),
            SortKey::Cpu,
            TOP_PER_PANE,
        );
        assert_eq!(find(&tree.rows, "node (11)").target, Some(11));
        assert_eq!(find(&tree.rows, "rustc (41)").target, Some(41));
        for r in &tree.rows {
            match r.kind {
                RowKind::Pane => assert!(r.target.is_some_and(|id| (1..=4).contains(&id))),
                RowKind::Proc => assert!(r.target.is_some()),
                _ => assert_eq!(r.target, None, "{}", r.label),
            }
        }
        let pane3 = tree
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Pane && r.target == Some(3));
        assert!(pane3.is_some());
    }

    #[test]
    fn sums_roll_up_per_level_and_parked_workspaces_appear() {
        let (owners, panes, procs) = fixture();
        let tree = build(
            &inputs(&owners, Some(panes.as_slice()), &procs),
            SortKey::Cpu,
            TOP_PER_PANE,
        );
        assert!(!tree.partial);
        assert!(!tree.clamped);
        let alpha = find(&tree.rows, "alpha");
        assert_eq!(alpha.kind, RowKind::Workspace);
        assert!(close(alpha.cpu_pct, 65.0));
        assert_eq!(alpha.rss, Some(10_500));
        let main = find(&tree.rows, "alpha/main");
        assert_eq!((main.depth, main.kind), (1, RowKind::Worktree));
        assert!(close(main.cpu_pct, 60.0));
        assert_eq!(main.rss, Some(10_000));
        let beta = find(&tree.rows, "beta");
        assert!(close(beta.cpu_pct, 120.0));
        let claude = find(&tree.rows, "1 · claude");
        assert_eq!((claude.depth, claude.kind), (2, RowKind::Pane));
        let thegn = find(&tree.rows, "thegn");
        assert!(close(thegn.cpu_pct, 6.0));
        assert_eq!(thegn.rss, Some(150));
        // Outside = 200 - (185 panes + 6 thegn) = 9; mem 20_000 - (12_500 + 150).
        let outside = find(&tree.rows, "outside thegn");
        assert!(close(outside.cpu_pct, 9.0));
        assert_eq!(outside.rss, Some(7_350));
        assert!(close(tree.total.cpu_pct, 200.0));
        // Share of the masthead total, in the sort key's unit.
        assert!(close(beta.share_of_total, 0.6));
        // Only pane-owned processes appear under panes, capped per pane.
        assert!(!tree.rows.iter().any(|r| r.label.starts_with("firefox")));
        assert!(!tree.rows.iter().any(|r| r.label == "thegn (90)"));
        assert!(!tree.rows.iter().any(|r| r.label == "sh (14)"));
        assert!(tree.rows.iter().any(|r| r.label == "git (13)"));
    }

    #[test]
    fn orders_heaviest_first_by_cpu_and_by_mem() {
        let (owners, panes, procs) = fixture();
        let by_cpu = build(
            &inputs(&owners, Some(panes.as_slice()), &procs),
            SortKey::Cpu,
            3,
        );
        let ws: Vec<&str> = by_cpu
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Workspace)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(ws, ["beta", "alpha"]);
        let panes_cpu: Vec<&str> = by_cpu
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Pane)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(panes_cpu, ["1 · cargo", "1 · claude", "2", "1 · nvim"]);
        let procs_cpu: Vec<&str> = by_cpu
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Proc && r.label != "rustc (41)")
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(procs_cpu, ["node (11)", "rg (12)", "git (13)"]);

        let by_mem = build(
            &inputs(&owners, Some(panes.as_slice()), &procs),
            SortKey::Mem,
            3,
        );
        let ws: Vec<&str> = by_mem
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Workspace)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(ws, ["alpha", "beta"]);
        let panes_mem: Vec<&str> = by_mem
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Pane && r.depth == 2)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(panes_mem, ["2", "1 · claude", "1 · nvim", "1 · cargo"]);
        let alpha = find(&by_mem.rows, "alpha");
        assert!(close(alpha.share_of_total, 10_500.0 / 20_000.0));
    }

    #[test]
    fn missing_data_stays_none_and_partial_falls_back_to_other() {
        let (owners, _, procs) = fixture();
        let tree = build(&inputs(&owners, None, &procs), SortKey::Cpu, 3);
        assert!(tree.partial);
        for label in ["alpha", "alpha/main", "1 · claude", "beta"] {
            let r = find(&tree.rows, label);
            assert_eq!((r.cpu_pct, r.rss, r.share_of_total), (None, None, None));
        }
        // Processes still list under their panes.
        assert!(tree.rows.iter().any(|r| r.label == "node (11)"));
        let outside = find(&tree.rows, "outside thegn");
        assert!(close(outside.cpu_pct, 7.0));
        assert_eq!(outside.rss, Some(70));

        // A pane with an unknown CPU (first, unprimed pass) keeps CPU unknown
        // up the tree but still sums memory.
        let owners = vec![owner(1, "w", "w/a", "1", None)];
        let panes = vec![PaneUsage {
            pane_id: 1,
            usage: Usage::new(None, Some(10)),
        }];
        let mut input = inputs(&owners, Some(panes.as_slice()), &[]);
        input.system = Usage::default();
        input.thegn = SelfUsage::default();
        let tree = build(&input, SortKey::Mem, 3);
        let w = find(&tree.rows, "w");
        assert_eq!((w.cpu_pct, w.rss), (None, Some(10)));
        let outside = find(&tree.rows, "outside thegn");
        assert_eq!((outside.cpu_pct, outside.rss), (None, None));
        assert_eq!(tree.total, Usage::default());
        assert_eq!(w.share_of_total, None);
    }

    #[test]
    fn outside_thegn_clamps_at_zero_and_flags_it() {
        let owners = vec![owner(1, "w", "w/a", "1", Some(""))];
        let panes = vec![pane(1, 500.0, 50_000)];
        let tree = build(
            &inputs(&owners, Some(panes.as_slice()), &[]),
            SortKey::Cpu,
            3,
        );
        assert!(tree.clamped);
        let outside = find(&tree.rows, "outside thegn");
        assert!(close(outside.cpu_pct, 0.0));
        assert_eq!(outside.rss, Some(0));
        // An empty program name keeps the bare tab label.
        assert!(tree.rows.iter().any(|r| r.label == "1"));

        // Zero cores: the total (and so every share) is unknown, and outside
        // falls back to the measured `other` bucket.
        let mut input = inputs(&owners, Some(panes.as_slice()), &[]);
        input.cores = 0;
        input.system.rss = Some(100_000);
        let tree = build(&input, SortKey::Cpu, 3);
        assert_eq!(tree.total.cpu_pct, None);
        assert!(!tree.clamped);
        let outside = find(&tree.rows, "outside thegn");
        assert!(close(outside.cpu_pct, 7.0));
        assert_eq!(outside.rss, Some(100_000 - 50_150));
        assert!(tree.rows.iter().all(|r| r.share_of_total.is_none()));

        // Memory clamps independently of CPU.
        let mut input = inputs(&owners, Some(panes.as_slice()), &[]);
        input.system = Usage::new(Some(100.0), Some(10));
        let tree = build(&input, SortKey::Mem, 3);
        assert!(tree.clamped);
        let outside = find(&tree.rows, "outside thegn");
        assert!(close(outside.cpu_pct, 800.0 - 506.0));
        assert_eq!(outside.rss, Some(0));
    }

    #[test]
    fn panes_without_an_owner_fold_into_unattributed() {
        let owners = vec![
            owner(1, "w", "w/a", "1", None),
            // A duplicate id keeps the first owner.
            owner(1, "dup", "dup/a", "9", None),
        ];
        let panes = vec![pane(1, 1.0, 1), pane(7, 2.0, 20), pane(8, 3.0, 30)];
        let tree = build(
            &inputs(&owners, Some(panes.as_slice()), &[]),
            SortKey::Cpu,
            3,
        );
        let un = find(&tree.rows, "unattributed panes");
        assert!(close(un.cpu_pct, 5.0));
        assert_eq!(un.rss, Some(50));
        assert!(!tree.rows.iter().any(|r| r.label == "dup"));
        // No unattributed row when every pane is owned.
        let tree = build(&inputs(&owners, Some(&panes[..1]), &[]), SortKey::Cpu, 3);
        assert!(!tree.rows.iter().any(|r| r.label == "unattributed panes"));
    }

    #[test]
    fn caps_at_max_rows_trimming_procs_then_panes() {
        // 10 worktrees × 3 panes × 3 procs = 1 + 10 + 30 + 90 rows + 4 tail.
        let mut owners = Vec::new();
        let mut panes = Vec::new();
        let mut procs = Vec::new();
        for wt in 0..10_u32 {
            for p in 0..3_u32 {
                let id = wt * 10 + p;
                owners.push(owner(id, "w", &format!("w/{wt}"), &format!("t{id}"), None));
                panes.push(pane(id, 1.0, 1));
                for k in 0..3_u32 {
                    procs.push(proc_row(id * 100 + k, "p", 1.0, 1, Owner::Pane(id)));
                }
            }
        }
        let tree = build(
            &inputs(&owners, Some(panes.as_slice()), &procs),
            SortKey::Cpu,
            3,
        );
        assert_eq!(tree.rows.len(), MAX_ROWS);
        let more = tree
            .rows
            .iter()
            .find(|r| r.kind == RowKind::More)
            .expect("+N more row");
        // 135 rows into 64: 72 dropped (all procs are trimmed before any pane).
        assert_eq!(more.label, "+72 more");
        assert_eq!(more.share_of_total, None);
        let count = |k: RowKind| tree.rows.iter().filter(|r| r.kind == k).count();
        assert_eq!(count(RowKind::Workspace), 1);
        assert_eq!(count(RowKind::Worktree), 10);
        assert_eq!(count(RowKind::Pane), 30);
        assert_eq!(count(RowKind::Proc), 18);
        assert_eq!(tree.rows.last().map(|r| r.kind), Some(RowKind::Outside));

        // Without processes, panes are trimmed next; worktrees always stay.
        let mut many = Vec::new();
        let mut usage = Vec::new();
        for id in 0..80_u32 {
            many.push(owner(id, "w", &format!("w/{}", id % 5), "t", None));
            usage.push(pane(id, 1.0, 1));
        }
        let tree = build(&inputs(&many, Some(&usage), &[]), SortKey::Mem, 3);
        assert_eq!(tree.rows.len(), MAX_ROWS);
        let count = |k: RowKind| tree.rows.iter().filter(|r| r.kind == k).count();
        assert_eq!(count(RowKind::Worktree), 5);
        assert_eq!(count(RowKind::Pane), MAX_ROWS - 1 - 5 - 1 - 4);
    }

    fn scope(kind: Option<ScopeKind>, slug: Option<&str>, cpu: f32, rss: u64) -> ScopeUsage {
        ScopeUsage {
            kind,
            worktree: slug.map(str::to_string),
            cpu_pct: Some(cpu),
            rss: Some(rss),
        }
    }

    #[test]
    fn named_scopes_join_their_worktree_and_the_rest_list_under_thegn() {
        let (owners, panes, procs) = fixture();
        // fixture paths: /repos/alpha-main, /repos/alpha-feat, /repos/beta-main.
        let scopes = [
            scope(Some(ScopeKind::Gate), Some("alpha-feat"), 100.0, 1_000),
            // Two agent scopes for one worktree merge into one row.
            scope(Some(ScopeKind::Agent), Some("alpha-feat"), 10.0, 10),
            scope(Some(ScopeKind::Agent), Some("alpha-feat"), 5.0, 5),
            scope(Some(ScopeKind::Gate), Some("gone-tree"), 7.0, 70),
            scope(None, None, 3.0, 30),
        ];
        let mut input = inputs(&owners, Some(panes.as_slice()), &procs);
        input.scopes = &scopes;
        let tree = build(&input, SortKey::Cpu, TOP_PER_PANE);
        let feat = tree
            .rows
            .iter()
            .position(|r| r.label == "alpha/feat")
            .expect("feat row");
        // Pane 3 (5%) + gate 100 + agents 15.
        assert!(close(tree.rows[feat].cpu_pct, 120.0));
        assert_eq!(tree.rows[feat].rss, Some(500 + 1_015));
        let gate = &tree.rows[feat + 1];
        assert_eq!(
            (gate.depth, gate.kind, gate.label.as_str()),
            (2, RowKind::Job, "gate")
        );
        assert!(close(gate.cpu_pct, 100.0));
        let agent = &tree.rows[feat + 2];
        assert_eq!(agent.label, "agent");
        assert!(close(agent.cpu_pct, 15.0));
        // The workspace rolls the jobs up too: 65 + 115.
        assert!(close(find(&tree.rows, "alpha").cpu_pct, 180.0));
        // Unmatched slug and the unnamed sum sit under thegn, which stays
        // UI + daemon.
        let loose = find(&tree.rows, "gate · gone-tree");
        assert_eq!((loose.depth, loose.kind), (1, RowKind::Job));
        let unnamed = find(&tree.rows, UNNAMED_JOBS);
        assert!(close(unnamed.cpu_pct, 3.0));
        assert!(close(find(&tree.rows, "thegn").cpu_pct, 6.0));
        let thegn_at = tree.rows.iter().position(|r| r.label == "thegn").unwrap();
        let outside_at = tree
            .rows
            .iter()
            .position(|r| r.label == "outside thegn")
            .unwrap();
        assert!(
            thegn_at
                < tree
                    .rows
                    .iter()
                    .position(|r| r.label == UNNAMED_JOBS)
                    .unwrap()
        );
        assert_eq!(outside_at, tree.rows.len() - 1);
        // Scopes (125%) are below the process-tree sum (191%), so outside is
        // unchanged: they overlap, not add.
        assert!(close(find(&tree.rows, "outside thegn").cpu_pct, 9.0));
        assert!(!tree.clamped);
    }

    #[test]
    fn scope_sum_above_the_process_tree_lowers_outside_and_clamps() {
        let owners = [owner(1, "w", "w/a", "1", None)];
        let panes = [pane(1, 10.0, 100)];
        // A gate running outside thegn's process tree: 150% of one core.
        let scopes = [scope(Some(ScopeKind::Gate), Some("w-a"), 150.0, 1_000)];
        let mut input = inputs(&owners, Some(panes.as_slice()), &[]);
        input.scopes = &scopes;
        let tree = build(&input, SortKey::Cpu, 3);
        // Total 200; max(10 + 6, 150) = 150 attributed.
        assert!(close(find(&tree.rows, "outside thegn").cpu_pct, 50.0));
        assert!(!tree.clamped);
        let big = [scope(None, None, 900.0, 90_000)];
        input.scopes = &big;
        let tree = build(&input, SortKey::Cpu, 3);
        assert!(tree.clamped);
        let outside = find(&tree.rows, "outside thegn");
        assert!(close(outside.cpu_pct, 0.0));
        assert_eq!(outside.rss, Some(0));
    }

    #[test]
    fn loose_jobs_are_bounded_and_overflow_into_unnamed() {
        let scopes: Vec<ScopeUsage> = (0..MAX_LOOSE_JOBS + 3)
            .map(|i| {
                let slug = format!("s{i:02}");
                scope(Some(ScopeKind::Agent), Some(slug.as_str()), 1.0, 1)
            })
            .collect();
        let mut input = inputs(&[], Some(&[]), &[]);
        input.scopes = &scopes;
        let tree = build(&input, SortKey::Mem, 3);
        let jobs = tree
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Job && r.label.starts_with("agent · "))
            .count();
        assert_eq!(jobs, MAX_LOOSE_JOBS);
        assert_eq!(find(&tree.rows, UNNAMED_JOBS).rss, Some(3));
        // A worktree without a path matches on its display name's slug.
        let mut bare = owner(1, "w", "w/plain", "1", None);
        bare.worktree_path = None;
        let owners = [bare];
        let scopes = [scope(Some(ScopeKind::Gate), Some("plain"), 2.0, 2)];
        let mut input = inputs(&owners, Some(&[]), &[]);
        input.scopes = &scopes;
        let tree = build(&input, SortKey::Cpu, 3);
        let wt = tree.rows.iter().position(|r| r.label == "w/plain").unwrap();
        assert_eq!(tree.rows[wt + 1].label, "gate");
    }

    #[test]
    fn at_least_takes_the_larger_known_side() {
        let a = Usage::new(Some(1.0), None);
        let b = Usage::new(Some(3.0), Some(5));
        assert_eq!(at_least(a, b), Usage::new(Some(3.0), Some(5)));
        assert_eq!(at_least(b, Usage::default()), b);
        assert_eq!(at_least(Usage::default(), a), a);
        assert_eq!(at_least(Usage::new(Some(9.0), Some(9)), b).rss, Some(9));
    }

    #[test]
    fn pane_label_joins_tab_and_program() {
        assert_eq!(pane_label(&owner(1, "w", "t", "2", Some("pi"))), "2 · pi");
        assert_eq!(pane_label(&owner(1, "w", "t", "2", None)), "2");
    }

    #[test]
    fn compact_lists_top_live_worktrees_and_folds_idle_ones_before_the_row_cap() {
        let (mut owners, mut panes, mut procs) = (Vec::new(), Vec::new(), Vec::new());
        for i in 0..12_u32 {
            owners.push(owner(i + 1, "w", &format!("w/{i}"), "t", None));
            panes.push(pane(i + 1, 20.0 - i as f32, 1));
            procs.push(proc_row(1000 + i, "p", 1.0, 1, Owner::Pane(i + 1)));
        }
        // The heaviest worktree lives in a lighter-sorted second workspace.
        owners.push(owner(50, "x", "x/hot", "t", None));
        panes.push(pane(50, 100.0, 1));
        procs.push(proc_row(5000, "hot", 90.0, 1, Owner::Pane(50)));
        // 300 idle worktrees (far more than MAX_ROWS) and an all-idle workspace.
        for k in 0..300_u32 {
            owners.push(owner(100 + k, "w", &format!("idle/{k}"), "t", None));
        }
        owners.push(owner(999, "z", "z/idle", "t", None));
        let tree = build_compact(
            &inputs(&owners, Some(panes.as_slice()), &procs),
            SortKey::Cpu,
            TOP_PER_PANE,
            TOP_WORKTREES,
        );
        let count = |k: RowKind| tree.rows.iter().filter(|r| r.kind == k).count();
        assert_eq!(count(RowKind::Worktree), TOP_WORKTREES);
        // Live worktrees keep their panes and processes (the cap never bit).
        assert_eq!(count(RowKind::Pane), TOP_WORKTREES);
        assert_eq!(count(RowKind::Proc), TOP_WORKTREES);
        assert_eq!(count(RowKind::More), 1);
        find(&tree.rows, "x/hot");
        find(&tree.rows, "w/6");
        assert!(tree.rows.iter().all(|r| r.label != "w/7" && r.label != "z"));
        let more = tree
            .rows
            .iter()
            .position(|r| r.kind == RowKind::More)
            .expect("summary row");
        assert_eq!(tree.rows[more].label, "+5 more · 301 idle worktrees");
        assert_eq!(tree.rows[more].share_of_total, None);
        assert_eq!(tree.rows[more + 1].kind, RowKind::Thegn);
    }

    #[test]
    fn compact_counts_sampled_processes_as_live_and_is_quiet_without_folds() {
        let owners = [
            owner(1, "w", "w/a", "t", None),
            owner(2, "w", "w/b", "t", None),
        ];
        let procs = [proc_row(10, "p", 1.0, 1, Owner::Pane(1))];
        // No per-pane totals: `w/a` is live through its sampled process.
        let tree = build_compact(&inputs(&owners, None, &procs), SortKey::Cpu, 3, 8);
        find(&tree.rows, "w/a");
        assert!(tree.rows.iter().all(|r| r.label != "w/b"));
        assert_eq!(find(&tree.rows, "1 idle worktree").kind, RowKind::More);

        let panes = [pane(1, 1.0, 1), pane(2, 2.0, 2)];
        let input = inputs(&owners, Some(&panes), &[]);
        let compact = build_compact(&input, SortKey::Mem, 3, 8);
        assert_eq!(compact, build(&input, SortKey::Mem, 3));
        let trimmed = build_compact(&input, SortKey::Mem, 3, 1);
        find(&trimmed.rows, "w/b");
        assert_eq!(find(&trimmed.rows, "+1 more worktree").kind, RowKind::More);
    }
}
