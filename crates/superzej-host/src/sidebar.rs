//! The workspace tree's structured row model and builder.
//!
//! The sidebar shows **workspaces** (repos) at depth 0, their **worktrees** at
//! depth 1, and — when a worktree owns more than one tab — its **pages** at
//! depth 2. Earlier this was a flat `Vec<String>` of pre-rendered lines built
//! in `run.rs`; it now produces a `Vec<SidebarRow>` carrying enough structure
//! for interaction (collapse, filter, sort, pin, multi-select) and per-row
//! status (git glyphs, agent, activity dot). Glyph/connector composition lives
//! at render time in `chrome::draw_sidebar`.

use std::collections::HashSet;

use crate::session::Session;

/// Which level of the tree a row sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Workspace,
    Worktree,
    Page,
}

/// Contextual activity, mirrored from the host-side `activity` state machine
/// (`superzej activity`). `Active` pulses; `Quiet` is the steady "done, look at
/// me" dot; `None`/acked render no dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityState {
    #[default]
    None,
    Active,
    Quiet,
}

impl ActivityState {
    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => ActivityState::Active,
            "quiet" => ActivityState::Quiet,
            _ => ActivityState::None, // "none" | "acked" | unknown
        }
    }
}

/// Git status summary for a worktree row (item 18). `dirty` = uncommitted
/// changes; `ahead`/`behind` are vs the upstream (absent when no upstream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitGlyphs {
    pub dirty: bool,
    pub ahead: usize,
    pub behind: usize,
}

/// Tree ordering for worktree groups within a workspace (item 23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// Case-insensitive label order, "home" first. Stable — a worktree keeps
    /// its slot when selected/opened (no jumping). The old plugin's default.
    #[default]
    Name,
    /// Most-recently-touched first (by tab position as a recency proxy).
    Recent,
    /// Active worktrees first, then quiet, then idle.
    Activity,
}

impl SortMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SortMode::Name => "name",
            SortMode::Recent => "recent",
            SortMode::Activity => "activity",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "recent" => SortMode::Recent,
            "activity" => SortMode::Activity,
            _ => SortMode::Name,
        }
    }
    /// Cycle to the next mode (for a single keybind).
    pub fn next(self) -> Self {
        match self {
            SortMode::Name => SortMode::Recent,
            SortMode::Recent => SortMode::Activity,
            SortMode::Activity => SortMode::Name,
        }
    }
}

/// One row in the workspace tree.
#[derive(Debug, Clone)]
pub struct SidebarRow {
    pub kind: RowKind,
    pub depth: u8,
    /// Bare label (no glyphs/connectors); e.g. repo display name, worktree base
    /// branch, or `·N` page tag.
    pub label: String,
    /// The grouping/collapse/pin key: the workspace slug for every row in a
    /// workspace's subtree.
    pub workspace_slug: String,
    /// The tab index this row activates on `Enter` (`None` for placeholder /
    /// collapsed-parent header rows that have no own tab).
    pub tab_target: Option<usize>,
    /// Whether this row's tab is the session's active tab.
    pub active: bool,
    /// Worktree path (Worktree rows only) — the key for git/agent/activity
    /// lookups, and for row actions like "copy path".
    #[allow(dead_code)]
    pub worktree_path: Option<String>,
    /// A stable key for pinning a row (workspace slug, or `slug/branch`).
    pub pin_key: String,
    /// The worktree's branch (Worktree rows). Retained for future status lines.
    #[allow(dead_code)]
    pub branch: Option<String>,
    pub git: Option<GitGlyphs>,
    pub agent: Option<String>,
    pub activity: ActivityState,
    /// Render/navigation visibility: false when hidden by a collapsed parent or
    /// filtered out.
    pub visible: bool,
    /// For Workspace rows: whether its subtree is collapsed (drives the caret).
    pub collapsed: bool,
}

/// Per-worktree status sourced from the (possibly slow) git/activity scan on
/// the hydration thread, merged onto rows at build time. `git`/`agent` are
/// keyed by worktree path; `activity` by tab name (matching the `activity`
/// state machine's TSV keys).
#[derive(Debug, Clone, Default)]
pub struct SidebarStatus {
    pub git: std::collections::BTreeMap<String, GitGlyphs>,
    pub agent: std::collections::BTreeMap<String, String>,
    pub activity: std::collections::BTreeMap<String, ActivityState>,
}

/// Persisted + transient view state that shapes the tree (collapse/sort/pins/
/// filter). Sourced from the `ui_state` DB table + in-memory interaction.
#[derive(Debug, Clone, Default)]
pub struct ViewState {
    /// Collapsed workspace slugs (their subtrees are hidden).
    pub collapsed: HashSet<String>,
    pub sort: SortMode,
    /// Pinned row keys (`pin_key`), in display order; pinned rows float to top.
    pub pins: Vec<String>,
    /// Active fuzzy filter; empty = no filter.
    pub filter: String,
}

/// Split a `{repo}/{branch}` tab name into its parts.
pub fn split_tab(name: &str) -> Option<(String, String)> {
    let (repo, branch) = name.split_once('/')?;
    (!repo.is_empty()).then(|| (repo.to_string(), branch.to_string()))
}

/// Split a worktree branch's `base ·N` page suffix; returns `(base, page)` with
/// page 1 when there's no suffix.
pub fn split_page(branch: &str) -> (String, u32) {
    if let Some((base, suffix)) = branch.rsplit_once(" \u{b7}")
        && !suffix.is_empty()
        && suffix.chars().all(|c| c.is_ascii_digit())
        && let Ok(n) = suffix.parse()
    {
        return (base.to_string(), n);
    }
    (branch.to_string(), 1)
}

#[derive(Debug, Clone)]
struct Group {
    label: String,
    /// (page number, tab index, is-active)
    pages: Vec<(u32, usize, bool)>,
    active: bool,
    min_position: usize,
    /// Worst-case activity over the group's tabs (for Activity sort).
    activity: ActivityState,
}

/// Build the full ordered row list for the tree. `workspaces` is the (slug,
/// display) list in workspace order (caller pulls it from the DB + live tabs).
/// `status` carries per-worktree git/agent/activity merged onto rows.
pub fn build_rows(
    session: &Session,
    workspaces: &[(String, String)],
    view: &ViewState,
    status: &SidebarStatus,
) -> Vec<SidebarRow> {
    let activity = &status.activity;
    let mut rows = Vec::new();

    for (repo_slug, display) in workspaces {
        let collapsed = view.collapsed.contains(repo_slug);
        rows.push(SidebarRow {
            kind: RowKind::Workspace,
            depth: 0,
            label: display.clone(),
            workspace_slug: repo_slug.clone(),
            tab_target: None,
            active: false,
            worktree_path: None,
            pin_key: repo_slug.clone(),
            branch: None,
            git: None,
            agent: None,
            activity: ActivityState::None,
            visible: true,
            collapsed,
        });

        // Group this repo's tabs by worktree base; collect pages per group.
        let mut groups: Vec<Group> = Vec::new();
        for (idx, tab) in session.tabs.iter().enumerate() {
            let Some((tab_repo, branch)) = split_tab(&tab.name) else {
                continue;
            };
            if &tab_repo != repo_slug {
                continue;
            }
            let (base, page) = split_page(&branch);
            let is_active = idx == session.active;
            let act = activity
                .get(&tab.name)
                .copied()
                .unwrap_or(ActivityState::None);
            if let Some(g) = groups.iter_mut().find(|g| g.label == base) {
                g.pages.push((page, idx, is_active));
                g.active |= is_active;
                g.min_position = g.min_position.min(idx);
                g.activity = max_activity(g.activity, act);
            } else {
                groups.push(Group {
                    label: base,
                    pages: vec![(page, idx, is_active)],
                    active: is_active,
                    min_position: idx,
                    activity: act,
                });
            }
        }

        sort_groups(&mut groups, view.sort);
        for g in &mut groups {
            g.pages.sort_by_key(|(page, pos, _)| (*page, *pos));
        }

        for g in groups {
            let wt_tab = g.pages.first().map(|(_, pos, _)| *pos);
            let wt_path = wt_tab
                .and_then(|i| session.tabs.get(i))
                .map(|t| t.worktree.clone())
                .filter(|p| !p.is_empty());
            let pin_key = format!("{repo_slug}/{}", g.label);
            let git = wt_path.as_deref().and_then(|p| status.git.get(p)).copied();
            let agent = wt_path
                .as_deref()
                .and_then(|p| status.agent.get(p))
                .cloned();
            rows.push(SidebarRow {
                kind: RowKind::Worktree,
                depth: 1,
                label: g.label.clone(),
                workspace_slug: repo_slug.clone(),
                tab_target: wt_tab,
                active: g.active && g.pages.len() == 1,
                worktree_path: wt_path,
                pin_key,
                branch: Some(g.label.clone()),
                git,
                agent,
                activity: g.activity,
                visible: !collapsed,
                collapsed: false,
            });
            if g.pages.len() > 1 {
                for (page, pos, is_active) in &g.pages {
                    rows.push(SidebarRow {
                        kind: RowKind::Page,
                        depth: 2,
                        label: format!("\u{b7}{page}"),
                        workspace_slug: repo_slug.clone(),
                        tab_target: Some(*pos),
                        active: *is_active,
                        worktree_path: None,
                        pin_key: format!("{repo_slug}/{}/{page}", g.label),
                        branch: None,
                        git: None,
                        agent: None,
                        activity: ActivityState::None,
                        visible: !collapsed,
                        collapsed: false,
                    });
                }
            }
        }
    }

    if rows.is_empty() {
        rows.push(SidebarRow {
            kind: RowKind::Workspace,
            depth: 0,
            label: "no workspaces".into(),
            workspace_slug: String::new(),
            tab_target: None,
            active: false,
            worktree_path: None,
            pin_key: String::new(),
            branch: None,
            git: None,
            agent: None,
            activity: ActivityState::None,
            visible: true,
            collapsed: false,
        });
    }

    apply_pins(&mut rows, &view.pins);
    apply_filter(&mut rows, &view.filter);
    rows
}

fn max_activity(a: ActivityState, b: ActivityState) -> ActivityState {
    use ActivityState::*;
    match (a, b) {
        (Active, _) | (_, Active) => Active,
        (Quiet, _) | (_, Quiet) => Quiet,
        _ => None,
    }
}

fn sort_groups(groups: &mut [Group], sort: SortMode) {
    match sort {
        SortMode::Name => {
            // "home" first, then case-insensitive label, ties by position.
            groups.sort_by(|a, b| {
                (a.label != "home", a.label.to_lowercase(), a.min_position).cmp(&(
                    b.label != "home",
                    b.label.to_lowercase(),
                    b.min_position,
                ))
            });
        }
        SortMode::Recent => {
            // Most-recent (highest tab position) first, home still pinned first.
            groups.sort_by(|a, b| {
                (a.label != "home")
                    .cmp(&(b.label != "home"))
                    .then(b.min_position.cmp(&a.min_position))
            });
        }
        SortMode::Activity => {
            // Active, then quiet, then idle; home first within each tier.
            let rank = |s: ActivityState| match s {
                ActivityState::Active => 0,
                ActivityState::Quiet => 1,
                ActivityState::None => 2,
            };
            groups.sort_by(|a, b| {
                rank(a.activity)
                    .cmp(&rank(b.activity))
                    .then((a.label != "home").cmp(&(b.label != "home")))
                    .then(a.min_position.cmp(&b.min_position))
            });
        }
    }
}

/// Float pinned blocks to the top of their sibling level, in `pins` order.
/// Operates hierarchically: workspace blocks reorder among workspaces, and
/// within each workspace its worktree blocks reorder among worktrees — so a
/// pinned worktree rises within its repo, and a pinned workspace rises overall.
fn apply_pins(rows: &mut Vec<SidebarRow>, pins: &[String]) {
    if pins.is_empty() {
        return;
    }
    let original = std::mem::take(rows);
    *rows = reorder_level(original, pins);
}

/// Reorder a contiguous run of rows whose first element is at the run's minimum
/// depth. Each block = a head row plus the deeper-depth rows that follow it;
/// children are reordered recursively, then blocks with pinned keys are moved
/// to the front in `pins` order (stable for the rest).
fn reorder_level(run: Vec<SidebarRow>, pins: &[String]) -> Vec<SidebarRow> {
    if run.is_empty() {
        return run;
    }
    let base_depth = run[0].depth;
    let mut blocks: Vec<(String, Vec<SidebarRow>)> = Vec::new();
    let mut i = 0;
    while i < run.len() {
        let key = run[i].pin_key.clone();
        let mut block = vec![run[i].clone()];
        i += 1;
        while i < run.len() && run[i].depth > base_depth {
            block.push(run[i].clone());
            i += 1;
        }
        // Recurse into the block's children (everything past the head row).
        let head = block.remove(0);
        let children = reorder_level(block, pins);
        let mut whole = Vec::with_capacity(children.len() + 1);
        whole.push(head);
        whole.extend(children);
        blocks.push((key, whole));
    }

    let mut pinned: Vec<Vec<SidebarRow>> = Vec::new();
    for key in pins {
        if let Some(pos) = blocks.iter().position(|(k, _)| k == key) {
            pinned.push(blocks.remove(pos).1);
        }
    }
    let mut out = Vec::new();
    for block in pinned {
        out.extend(block);
    }
    for (_, block) in blocks {
        out.extend(block);
    }
    out
}

/// Substring (case-insensitive) filter: a row matches on its own label, and a
/// workspace stays visible if any descendant matches. Non-matches set
/// `visible = false` (preserving collapse state for matches).
fn apply_filter(rows: &mut [SidebarRow], filter: &str) {
    let q = filter.trim().to_lowercase();
    if q.is_empty() {
        return;
    }
    let n = rows.len();
    // Which rows match on their own label.
    let self_match: Vec<bool> = rows
        .iter()
        .map(|r| r.label.to_lowercase().contains(&q))
        .collect();

    let mut keep = self_match.clone();
    // A worktree match reveals its own pages; a workspace that itself matched
    // reveals its whole subtree. Propagate matches up to the owning rows.
    let mut last_workspace: Option<usize> = None;
    let mut last_worktree: Option<usize> = None;
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => {
                last_workspace = Some(i);
                last_worktree = None;
            }
            RowKind::Worktree => {
                last_worktree = Some(i);
                if keep[i]
                    && let Some(w) = last_workspace
                {
                    keep[w] = true; // surface the parent repo header
                }
            }
            RowKind::Page => {
                if keep[i] {
                    if let Some(wt) = last_worktree {
                        keep[wt] = true;
                    }
                    if let Some(w) = last_workspace {
                        keep[w] = true;
                    }
                }
            }
        }
    }
    // Reveal descendants only for rows that matched on their own label.
    let mut reveal_ws = false; // inside a self-matched workspace
    let mut reveal_wt = false; // inside a self-matched worktree
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => {
                reveal_ws = self_match[i];
                reveal_wt = false;
            }
            RowKind::Worktree => {
                reveal_wt = self_match[i];
                if reveal_ws {
                    keep[i] = true;
                }
            }
            RowKind::Page => {
                if reveal_ws || reveal_wt {
                    keep[i] = true;
                }
            }
        }
    }
    for (i, r) in rows.iter_mut().enumerate() {
        r.visible = keep[i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::center::CenterTree;
    use crate::session::{Session, Tab, TabKind};

    fn tab(name: &str, wt: &str) -> Tab {
        Tab {
            name: name.into(),
            kind: TabKind::Worktree,
            worktree: wt.into(),
            center: CenterTree::Leaf(0),
            focused_pane: 0,
        }
    }

    fn session(tabs: Vec<Tab>, active: usize) -> Session {
        Session {
            id: "s1".into(),
            tabs,
            active,
        }
    }

    fn no_activity() -> SidebarStatus {
        SidebarStatus::default()
    }

    #[test]
    fn groups_worktrees_under_workspace_with_home_first() {
        let s = session(
            vec![tab("app/feat", "/wt/feat"), tab("app/home", "/wt/home")],
            0,
        );
        let ws = vec![("app".to_string(), "app".to_string())];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity());
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["app", "home", "feat"]);
        assert_eq!(rows[0].kind, RowKind::Workspace);
        assert_eq!(rows[1].kind, RowKind::Worktree);
    }

    #[test]
    fn collapse_hides_children() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let ws = vec![("app".to_string(), "app".to_string())];
        let mut view = ViewState::default();
        view.collapsed.insert("app".to_string());
        let rows = build_rows(&s, &ws, &view, &no_activity());
        assert!(rows[0].visible); // workspace stays
        assert!(!rows[1].visible); // worktree hidden
    }

    #[test]
    fn pages_appear_under_multi_tab_worktree() {
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/home \u{b7}2", "/wt/home"),
            ],
            0,
        );
        let ws = vec![("app".to_string(), "app".to_string())];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity());
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::Workspace,
                RowKind::Worktree,
                RowKind::Page,
                RowKind::Page
            ]
        );
    }

    #[test]
    fn filter_keeps_matching_worktree_and_its_workspace() {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/feature-x", "/wt/fx")],
            0,
        );
        let ws = vec![("app".to_string(), "app".to_string())];
        let view = ViewState {
            filter: "feature".into(),
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity());
        let visible: Vec<&str> = rows
            .iter()
            .filter(|r| r.visible)
            .map(|r| r.label.as_str())
            .collect();
        assert!(visible.contains(&"app"));
        assert!(visible.contains(&"feature-x"));
        assert!(!visible.contains(&"home"));
    }

    #[test]
    fn pin_floats_worktree_block_to_top() {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/feat", "/wt/feat")],
            0,
        );
        let ws = vec![("app".to_string(), "app".to_string())];
        let view = ViewState {
            pins: vec!["app/feat".into()],
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity());
        // Workspace block contains all rows (depth>0), so pinning the worktree
        // inside reorders within — feat should precede home.
        let feat = rows.iter().position(|r| r.label == "feat").unwrap();
        let home = rows.iter().position(|r| r.label == "home").unwrap();
        assert!(feat < home, "pinned feat should sort before home");
    }

    #[test]
    fn activity_sort_puts_active_first() {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/busy", "/wt/busy")],
            0,
        );
        let ws = vec![("app".to_string(), "app".to_string())];
        let mut act = no_activity();
        act.activity
            .insert("app/busy".into(), ActivityState::Active);
        let view = ViewState {
            sort: SortMode::Activity,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &act);
        let busy = rows.iter().position(|r| r.label == "busy").unwrap();
        let home = rows.iter().position(|r| r.label == "home").unwrap();
        assert!(busy < home, "active worktree should sort first");
    }
}
