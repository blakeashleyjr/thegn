//! The workspace tree's structured row model and builder.
//!
//! The sidebar shows **workspaces** (repos) at depth 0 and their **worktrees**
//! at depth 1 — a worktree's tabs live in the tabbar only, never here. Rows
//! come straight from the session's `WorktreeGroup` model (no name parsing).
//! It produces a `Vec<SidebarRow>` carrying enough structure for interaction
//! (collapse, filter, sort, pin, multi-select) and per-row status (git glyphs,
//! agent, activity dot). Glyph/connector composition lives at render time in
//! `chrome::draw_sidebar`.

use std::collections::HashSet;

use crate::session::Session;

/// Which level of the tree a row sits at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Workspace,
    Worktree,
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
    /// What this row activates on `Enter` (`None` for placeholder /
    /// collapsed-parent header rows that have no own target).
    pub tab_target: Option<RowTarget>,
    /// Whether this row is (in) the session's active worktree/tab.
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
    /// For Workspace rows: a non-git "dir" workspace (drives a distinct glyph).
    pub dir: bool,
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

/// What activating a sidebar row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowTarget {
    /// Focus a live `(worktree group, tab)` in the current session.
    Tab(usize, usize),
    /// Switch to another workspace (optionally landing on a named worktree
    /// group there — the `{slug}/{branch}` name in its persisted layout).
    Workspace {
        repo_path: String,
        group: Option<String>,
    },
}

/// A worktree registered in the DB for some workspace — how the sidebar lists
/// worktrees of workspaces that aren't currently loaded in the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbWorktree {
    /// The owning workspace's slug (the `{slug}/…` tab prefix).
    pub slug: String,
    /// Branch label shown in the tree.
    pub branch: String,
    /// The workspace's repo path (the switch target).
    pub repo_path: String,
    /// Full `{slug}/{branch}` group name.
    pub tab_name: String,
    /// Worktree dir on disk (status lookups).
    pub path: String,
}

/// Split a `{repo}/{branch}` group name into its parts.
pub fn split_tab(name: &str) -> Option<(String, String)> {
    let (repo, branch) = name.split_once('/')?;
    (!repo.is_empty()).then(|| (repo.to_string(), branch.to_string()))
}

/// A workspace's worktree, ready to sort: the branch label plus its session
/// group index and status.
#[derive(Debug, Clone)]
struct Group {
    label: String,
    gi: usize,
    activity: ActivityState,
}

/// Build the full ordered row list for the tree. `workspaces` is the
/// `(slug, display, kind, repo_path)` list in workspace order (caller pulls it
/// from the DB + live groups). `status` carries per-worktree status merged
/// onto rows. `db_worktrees` backs the rows of workspaces that are NOT loaded
/// in the session — every workspace shows its home + registered worktrees,
/// and activating one switches workspace.
pub fn build_rows(
    session: &Session,
    workspaces: &[(String, String, String, String)],
    view: &ViewState,
    status: &SidebarStatus,
    db_worktrees: &[DbWorktree],
) -> Vec<SidebarRow> {
    let activity = &status.activity;
    let mut rows = Vec::new();

    for (repo_slug, display, kind, repo_path) in workspaces {
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
            dir: kind == "dir",
        });

        // This repo's worktree groups, straight from the session model.
        let mut groups: Vec<Group> = Vec::new();
        for (gi, g) in session.worktrees.iter().enumerate() {
            let Some((repo, branch)) = split_tab(&g.name) else {
                continue;
            };
            if &repo != repo_slug {
                continue;
            }
            groups.push(Group {
                label: branch,
                gi,
                activity: activity.get(&g.name).copied().unwrap_or_default(),
            });
        }

        sort_groups(&mut groups, view.sort);
        let live = !groups.is_empty();

        for gr in groups {
            let g = &session.worktrees[gr.gi];
            let is_active_group = gr.gi == session.active;
            let wt_path = (!g.path.is_empty()).then(|| g.path.clone());
            let pin_key = format!("{repo_slug}/{}", gr.label);
            let git = wt_path.as_deref().and_then(|p| status.git.get(p)).copied();
            let agent = wt_path
                .as_deref()
                .and_then(|p| status.agent.get(p))
                .cloned();
            rows.push(SidebarRow {
                kind: RowKind::Worktree,
                depth: 1,
                label: gr.label.clone(),
                workspace_slug: repo_slug.clone(),
                tab_target: Some(RowTarget::Tab(gr.gi, g.active_tab)),
                active: is_active_group,
                worktree_path: wt_path,
                pin_key,
                branch: Some(gr.label.clone()),
                git,
                agent,
                activity: gr.activity,
                visible: !collapsed,
                collapsed: false,
                dir: false,
            });
        }

        // A workspace with no live session groups still shows its home and
        // registered worktrees; activating one switches workspace.
        if !live && !repo_path.is_empty() {
            let mk = |label: &str, group: Option<String>, path: Option<String>| SidebarRow {
                kind: RowKind::Worktree,
                depth: 1,
                label: label.to_string(),
                workspace_slug: repo_slug.clone(),
                tab_target: Some(RowTarget::Workspace {
                    repo_path: repo_path.clone(),
                    group,
                }),
                active: false,
                worktree_path: path.clone(),
                pin_key: format!("{repo_slug}/{label}"),
                branch: Some(label.to_string()),
                git: path.as_deref().and_then(|p| status.git.get(p)).copied(),
                agent: path.as_deref().and_then(|p| status.agent.get(p)).cloned(),
                activity: ActivityState::None,
                visible: !collapsed,
                collapsed: false,
                dir: false,
            };
            rows.push(mk(
                "home",
                Some(format!("{repo_slug}/home")),
                Some(repo_path.clone()),
            ));
            // A registry row for the home checkout would duplicate the
            // synthesized row above — skip it.
            for w in db_worktrees
                .iter()
                .filter(|w| &w.slug == repo_slug && w.branch != "home")
            {
                rows.push(mk(
                    &w.branch,
                    Some(w.tab_name.clone()),
                    Some(w.path.clone()),
                ));
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
            dir: false,
        });
    }

    apply_pins(&mut rows, &view.pins);
    apply_filter(&mut rows, &view.filter);
    rows
}

fn sort_groups(groups: &mut [Group], sort: SortMode) {
    match sort {
        SortMode::Name => {
            // "home" first, then case-insensitive label, ties by position.
            groups.sort_by(|a, b| {
                (a.label != "home", a.label.to_lowercase(), a.gi).cmp(&(
                    b.label != "home",
                    b.label.to_lowercase(),
                    b.gi,
                ))
            });
        }
        SortMode::Recent => {
            // Most-recent (highest group position) first, home still pinned first.
            groups.sort_by(|a, b| {
                (a.label != "home")
                    .cmp(&(b.label != "home"))
                    .then(b.gi.cmp(&a.gi))
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
                    .then(a.gi.cmp(&b.gi))
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
    // A worktree match surfaces its parent repo header; a workspace that
    // itself matched reveals its whole subtree.
    let mut last_workspace: Option<usize> = None;
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => last_workspace = Some(i),
            RowKind::Worktree => {
                if keep[i]
                    && let Some(w) = last_workspace
                {
                    keep[w] = true; // surface the parent repo header
                }
            }
        }
    }
    // Reveal worktrees only for workspaces that matched on their own label.
    let mut reveal_ws = false; // inside a self-matched workspace
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => reveal_ws = self_match[i],
            RowKind::Worktree => {
                if reveal_ws {
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
    use crate::session::{GroupKind, Session, WorktreeGroup};

    fn tab(name: &str, wt: &str) -> WorktreeGroup {
        WorktreeGroup::new(name, GroupKind::Branch, wt)
    }

    fn session(worktrees: Vec<WorktreeGroup>, active: usize) -> Session {
        Session {
            id: "s1".into(),
            worktrees,
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
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[]);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["app", "home", "feat"]);
        assert_eq!(rows[0].kind, RowKind::Workspace);
        assert_eq!(rows[1].kind, RowKind::Worktree);
    }

    #[test]
    fn live_workspace_renders_exactly_one_home_row() {
        // A canonical entry (slug + path) whose live group matches the slug:
        // the real (active-capable) home row renders, never a synthetic twin.
        let s = session(
            vec![WorktreeGroup::new(
                "washu/home",
                GroupKind::Home,
                "/repos/WASHU",
            )],
            0,
        );
        let ws = vec![(
            "washu".to_string(),
            "WASHU".to_string(),
            "repo".to_string(),
            "/repos/WASHU".to_string(),
        )];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[]);
        let homes: Vec<_> = rows.iter().filter(|r| r.label == "home").collect();
        assert_eq!(homes.len(), 1, "rows: {rows:?}");
        assert!(homes[0].active, "the live home row carries the active flag");
        assert!(
            matches!(homes[0].tab_target, Some(RowTarget::Tab(0, _))),
            "live row targets the session tab, not a workspace switch"
        );
    }

    #[test]
    fn workspace_kind_sets_dir_flag_on_the_row() {
        let s = session(vec![], 0);
        let ws = vec![
            (
                "repo".to_string(),
                "repo".to_string(),
                "repo".to_string(),
                String::new(),
            ),
            (
                "notes".to_string(),
                "notes".to_string(),
                "dir".to_string(),
                String::new(),
            ),
        ];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[]);
        let repo_row = rows.iter().find(|r| r.label == "repo").unwrap();
        let dir_row = rows.iter().find(|r| r.label == "notes").unwrap();
        assert!(!repo_row.dir, "repo workspace is not a dir");
        assert!(dir_row.dir, "non-git workspace is flagged dir");
    }

    #[test]
    fn other_workspaces_show_home_and_registered_worktrees() {
        // The session only holds "app"; "other" must still list its home and
        // DB-registered worktrees, targeting a workspace switch.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let ws = vec![
            (
                "app".to_string(),
                "app".to_string(),
                "repo".to_string(),
                "/repos/app".to_string(),
            ),
            (
                "other".to_string(),
                "other".to_string(),
                "repo".to_string(),
                "/repos/other".to_string(),
            ),
        ];
        let dbw = vec![DbWorktree {
            slug: "other".into(),
            branch: "feat-x".into(),
            repo_path: "/repos/other".into(),
            tab_name: "other/feat-x".into(),
            path: "/wt/other-feat-x".into(),
        }];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &dbw);
        let labels: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.workspace_slug.as_str(), r.label.as_str()))
            .collect();
        assert!(labels.contains(&("other", "home")), "{labels:?}");
        assert!(labels.contains(&("other", "feat-x")), "{labels:?}");
        // Their targets switch workspace (optionally onto the named group).
        let home = rows
            .iter()
            .find(|r| r.workspace_slug == "other" && r.label == "home")
            .unwrap();
        assert_eq!(
            home.tab_target,
            Some(RowTarget::Workspace {
                repo_path: "/repos/other".into(),
                group: Some("other/home".into()),
            })
        );
        // The live workspace keeps its session-backed rows.
        let app_home = rows
            .iter()
            .find(|r| r.workspace_slug == "app" && r.label == "home")
            .unwrap();
        assert_eq!(app_home.tab_target, Some(RowTarget::Tab(0, 0)));
    }

    #[test]
    fn collapse_hides_children() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let mut view = ViewState::default();
        view.collapsed.insert("app".to_string());
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[]);
        assert!(rows[0].visible); // workspace stays
        assert!(!rows[1].visible); // worktree hidden
    }

    #[test]
    fn tabs_never_appear_in_the_sidebar() {
        // Tabs live in the tabbar; the sidebar lists worktrees only — even
        // when a worktree owns several tabs.
        let mut home = tab("app/home", "/wt/home");
        home.add_tab();
        home.active_tab = 1;
        let s = session(vec![home], 0);
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[]);
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![RowKind::Workspace, RowKind::Worktree]);
        // The worktree row jumps to the group's remembered active tab.
        assert_eq!(rows[1].tab_target, Some(RowTarget::Tab(0, 1)));
        assert!(rows[1].active);
    }

    #[test]
    fn filter_keeps_matching_worktree_and_its_workspace() {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/feature-x", "/wt/fx")],
            0,
        );
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let view = ViewState {
            filter: "feature".into(),
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[]);
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
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let view = ViewState {
            pins: vec!["app/feat".into()],
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[]);
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
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let mut act = no_activity();
        act.activity
            .insert("app/busy".into(), ActivityState::Active);
        let view = ViewState {
            sort: SortMode::Activity,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &act, &[]);
        let busy = rows.iter().position(|r| r.label == "busy").unwrap();
        let home = rows.iter().position(|r| r.label == "home").unwrap();
        assert!(busy < home, "active worktree should sort first");
    }
}
