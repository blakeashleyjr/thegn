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
    Folder,
    Worktree,
}

/// Contextual activity, mirrored from the host-side `activity` state machine.
/// Drives the sidebar dot's color: `Active` (worktree busy / agent working)
/// renders a white dot; `Quiet` (was active, now idle — the agent is waiting
/// for the user) renders a red dot; `None`/acked (dormant) render no dot.
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

/// Badge kinds displayed on sidebar rows (item 28).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum BadgeKind {
    /// Open PRs for this worktree's branch.
    Pr,
    /// Unread notifications relevant to this worktree.
    Unread,
    /// Alerts: test failures, agent failures, log errors.
    Alert,
}

/// Tree ordering for worktree groups within a workspace (item 23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// User-controlled order: trusts the underlying sequence (the session's
    /// group order when loaded, the persisted `position` order when not),
    /// "home" first. Defaults to creation order and is what Shift+Alt+↑/↓
    /// rearranges. The default — worktrees never reshuffle on their own.
    #[default]
    Manual,
    /// Case-insensitive label order, "home" first. Stable — a worktree keeps
    /// its slot when selected/opened (no jumping). The old plugin's default.
    Name,
    /// Most-recently-touched first (by tab position as a recency proxy).
    Recent,
    /// Active worktrees first, then quiet, then idle.
    Activity,
}

impl SortMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SortMode::Manual => "manual",
            SortMode::Name => "name",
            SortMode::Recent => "recent",
            SortMode::Activity => "activity",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "name" => SortMode::Name,
            "recent" => SortMode::Recent,
            "activity" => SortMode::Activity,
            // Unknown / "manual" → the manual (creation-order) default.
            _ => SortMode::Manual,
        }
    }
    /// Cycle to the next mode (for a single keybind).
    pub fn next(self) -> Self {
        match self {
            SortMode::Manual => SortMode::Name,
            SortMode::Name => SortMode::Recent,
            SortMode::Recent => SortMode::Activity,
            SortMode::Activity => SortMode::Manual,
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
    /// For Worktree rows: the worktree path — the key for git/agent/activity
    /// lookups, and for row actions like "copy path". For Workspace rows: the
    /// repo path (the remove-workspace target), or `None` for a live fallback
    /// with no DB row yet.
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
    /// Badge: open PR count for this worktree's branch (item 28).
    #[allow(dead_code)]
    pub pr_count: Option<usize>,
    /// Lowest open PR number for this worktree's branch, used to compose the
    /// dynamic row title (`[PR: <n> | …]`). `None` when no open PR is cached.
    pub pr_number: Option<u64>,
    /// Badge: unread notification count for this worktree (item 28).
    #[allow(dead_code)]
    pub unread_count: usize,
    /// Badge: alert count (test failures, agent failures, log errors) for this worktree (item 28).
    #[allow(dead_code)]
    pub alert_count: usize,
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
    /// Badge: open PR count per worktree (item 28).
    pub pr_counts: std::collections::BTreeMap<String, usize>,
    /// Lowest open PR number per worktree, for the dynamic row title.
    pub pr_numbers: std::collections::BTreeMap<String, u64>,
    /// Badge: unread notification count per worktree (item 28).
    pub unread_counts: std::collections::BTreeMap<String, usize>,
    /// Badge: alert count per worktree (item 28).
    pub alert_counts: std::collections::BTreeMap<String, usize>,
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
    /// Nullable folder assignment
    pub folder_id: Option<i64>,
}

/// Split a `{repo}/{branch}` group name into its parts.
pub fn split_tab(name: &str) -> Option<(String, String)> {
    let (repo, branch) = name.split_once('/')?;
    (!repo.is_empty()).then(|| (repo.to_string(), branch.to_string()))
}

/// Strip a single trailing shell-prompt sigil (`$` `%` `#` `>`) and surrounding
/// whitespace from an OSC window title. zsh + starship and friends often append
/// "… $" to the terminal title, which we don't want bleeding into the sidebar.
fn strip_prompt_sigil(title: &str) -> String {
    let t = title.trim();
    let t = t.strip_suffix(['$', '%', '#', '>']).unwrap_or(t);
    t.trim_end().to_string()
}

/// Compose a worktree row's displayed title:
/// - PR present:     `[PR: <n> | <window-title-or-branch>]`
/// - title present:  `<window-title>` (sigil-stripped)
/// - otherwise:      `<branch>`
pub fn compose_row_label(
    pr_number: Option<u64>,
    window_title: Option<&str>,
    branch: &str,
) -> String {
    let title = window_title
        .map(strip_prompt_sigil)
        .filter(|t| !t.is_empty());
    match (pr_number, title) {
        (Some(n), Some(t)) => format!("[PR: {n} | {t}]"),
        (Some(n), None) => format!("[PR: {n} | {branch}]"),
        (None, Some(t)) => t,
        (None, None) => branch.to_string(),
    }
}

/// A workspace's worktree, ready to sort: the branch label plus its session
/// group index and status.
#[derive(Debug, Clone)]
struct Group {
    label: String,
    gi: usize,
    activity: ActivityState,
    folder_id: Option<i64>,
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
    db_folders: &[superzej_core::models::FolderRow],
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
            // Workspace rows carry the repo path (not a worktree path) so the
            // remove-workspace action can resolve its DB target without a
            // slug→path lookup. Empty for live fallbacks with no DB row yet.
            worktree_path: (!repo_path.is_empty()).then(|| repo_path.clone()),
            pin_key: repo_slug.clone(),
            branch: None,
            git: None,
            agent: None,
            activity: ActivityState::None,
            visible: true,
            collapsed,
            dir: kind == "dir",
            pr_count: None,
            pr_number: None,
            unread_count: 0,
            alert_count: 0,
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
                folder_id: db_worktrees
                    .iter()
                    .find(|w| w.tab_name == g.name)
                    .and_then(|w| w.folder_id),
            });
        }

        sort_groups(&mut groups, view.sort);
        let live = !groups.is_empty();

        // Split into unfiled (rendered at root) and filed (rendered under folders).
        // Unfiled keeps the existing home-first / sort behaviour; filed worktrees
        // are emitted later under their folder header at depth 2.
        let loose_groups: Vec<&Group> = groups.iter().filter(|g| g.folder_id.is_none()).collect();
        for gr in loose_groups {
            let g = &session.worktrees[gr.gi];
            let is_active_group = gr.gi == session.active;
            let wt_path = (!g.path.is_empty()).then(|| g.path.clone());
            let pin_key = format!("{repo_slug}/{}", gr.label);
            let git = wt_path.as_deref().and_then(|p| status.git.get(p)).copied();
            let agent = wt_path
                .as_deref()
                .and_then(|p| status.agent.get(p))
                .cloned();
            let pr_count = wt_path
                .as_deref()
                .and_then(|p| status.pr_counts.get(p))
                .copied();
            let pr_number = wt_path
                .as_deref()
                .and_then(|p| status.pr_numbers.get(p))
                .copied();
            let unread_count = wt_path
                .as_deref()
                .and_then(|p| status.unread_counts.get(p))
                .copied()
                .unwrap_or(0);
            let alert_count = wt_path
                .as_deref()
                .and_then(|p| status.alert_counts.get(p))
                .copied()
                .unwrap_or(0);
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
                pr_count,
                pr_number,
                unread_count,
                alert_count,
            });
        }

        // Folders section: home → loose (above) → folders by `position`.
        // Filed worktrees render at depth 2 under their folder header, in the
        // order the user arranged them (Move Up/Down on the worktree row will
        // eventually resequence via `swap_worktree_positions`; for now we
        // preserve the existing sort for visibility).
        let mut workspace_folders: Vec<&superzej_core::models::FolderRow> = db_folders
            .iter()
            .filter(|f| f.repo_path == *repo_path)
            .collect();
        workspace_folders.sort_by_key(|f| f.position);

        // Build a quick lookup from folder_id → worktree rows for this workspace.
        let filed_in_folders: std::collections::BTreeMap<i64, Vec<&Group>> = {
            let mut map: std::collections::BTreeMap<i64, Vec<&Group>> =
                std::collections::BTreeMap::new();
            for g in groups.iter().filter(|g| g.folder_id.is_some()) {
                if let Some(fid) = g.folder_id {
                    map.entry(fid).or_default().push(g);
                }
            }
            map
        };

        for folder in workspace_folders {
            let folder_key = format!("{repo_slug}/folder:{}", folder.folder_id);
            let folder_collapsed = view.collapsed.contains(&folder_key);
            let mut child_count = 0usize;
            if let Some(filed) = filed_in_folders.get(&folder.folder_id) {
                child_count = filed.len();
            }
            // Also count DB-registered (unloaded) worktrees filed to this folder
            // by scanning db_worktrees, so the count stays accurate when the
            // workspace is dormant.
            for w in db_worktrees
                .iter()
                .filter(|w| w.folder_id == Some(folder.folder_id))
            {
                let already_counted = filed_in_folders
                    .get(&folder.folder_id)
                    .map(|v| v.iter().any(|g| g.label == w.branch))
                    .unwrap_or(false);
                if !already_counted {
                    child_count += 1;
                }
            }
            rows.push(SidebarRow {
                kind: RowKind::Folder,
                depth: 1,
                label: if child_count > 0 {
                    format!("{} ({})", folder.name, child_count)
                } else {
                    folder.name.clone()
                },
                workspace_slug: repo_slug.clone(),
                tab_target: None,
                active: false,
                worktree_path: None,
                pin_key: folder_key.clone(),
                branch: None,
                git: None,
                agent: None,
                activity: ActivityState::None,
                visible: !collapsed,
                collapsed: folder_collapsed,
                dir: false,
                pr_count: None,
                pr_number: None,
                unread_count: 0,
                alert_count: 0,
            });

            if !folder_collapsed {
                // Live groups in this folder, in their sort order. We re-derive
                // the order from the same sort the loose path used by reusing
                // the position-based comparator on the slice.
                if let Some(filed) = filed_in_folders.get(&folder.folder_id) {
                    let mut filed_sorted: Vec<&Group> = filed.clone();
                    // Note: Instead of a new sort_groups_by_gi fn, we can just use
                    // the actual `sort` parameter the user requested, since the loose
                    // branch used the same sort. We can refactor `sort_groups` to take `&mut [&Group]`.
                    // For now, let's just sort them manually to match `SortMode::Manual`.
                    filed_sorted.sort_by_key(|a| (a.label != "home", a.gi));
                    for gr in filed_sorted {
                        let g = &session.worktrees[gr.gi];
                        let is_active_group = gr.gi == session.active;
                        let wt_path = (!g.path.is_empty()).then(|| g.path.clone());
                        let pin_key =
                            format!("{repo_slug}/{}/folder:{}", gr.label, folder.folder_id);
                        let git = wt_path.as_deref().and_then(|p| status.git.get(p)).copied();
                        let agent = wt_path
                            .as_deref()
                            .and_then(|p| status.agent.get(p))
                            .cloned();
                        let pr_count = wt_path
                            .as_deref()
                            .and_then(|p| status.pr_counts.get(p))
                            .copied();
                        let pr_number = wt_path
                            .as_deref()
                            .and_then(|p| status.pr_numbers.get(p))
                            .copied();
                        let unread_count = wt_path
                            .as_deref()
                            .and_then(|p| status.unread_counts.get(p))
                            .copied()
                            .unwrap_or(0);
                        let alert_count = wt_path
                            .as_deref()
                            .and_then(|p| status.alert_counts.get(p))
                            .copied()
                            .unwrap_or(0);
                        rows.push(SidebarRow {
                            kind: RowKind::Worktree,
                            depth: 2,
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
                            pr_count,
                            pr_number,
                            unread_count,
                            alert_count,
                        });
                    }
                }
            }
        }

        // A workspace with no live session groups still shows its home and
        // registered worktrees; activating one switches workspace.
        if !live && !repo_path.is_empty() {
            let mk = |label: &str, group: Option<String>, path: Option<String>| {
                let pr_count = path
                    .as_deref()
                    .and_then(|p| status.pr_counts.get(p))
                    .copied();
                let pr_number = path
                    .as_deref()
                    .and_then(|p| status.pr_numbers.get(p))
                    .copied();
                let unread_count = path
                    .as_deref()
                    .and_then(|p| status.unread_counts.get(p))
                    .copied()
                    .unwrap_or(0);
                let alert_count = path
                    .as_deref()
                    .and_then(|p| status.alert_counts.get(p))
                    .copied()
                    .unwrap_or(0);
                // Activity dot keyed by tab name (the `group`), same source the
                // live rows use — so a workspace you switch away from keeps its
                // last-known activity dot instead of going dark.
                let act = group
                    .as_deref()
                    .and_then(|t| status.activity.get(t))
                    .copied()
                    .unwrap_or_default();
                SidebarRow {
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
                    activity: act,
                    visible: !collapsed,
                    collapsed: false,
                    dir: false,
                    pr_count,
                    pr_number,
                    unread_count,
                    alert_count,
                }
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
            pr_count: None,
            pr_number: None,
            unread_count: 0,
            alert_count: 0,
        });
    }

    apply_pins(&mut rows, &view.pins);
    apply_filter(&mut rows, &view.filter);
    rows
}

fn sort_groups(groups: &mut [Group], sort: SortMode) {
    match sort {
        SortMode::Manual => {
            // Trust the session order (gi); just float "home" to the top.
            // `gi` is the worktree's slot in `session.worktrees`, which the
            // host keeps in persisted `position` order — so this is the
            // creation-order-by-default, manually-reorderable sequence.
            groups.sort_by_key(|a| (a.label != "home", a.gi));
        }
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
            RowKind::Folder => {}
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
            RowKind::Folder => {}
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

    #[test]
    fn strip_prompt_sigil_drops_trailing_prompt_chars() {
        assert_eq!(strip_prompt_sigil("superzej dev $"), "superzej dev");
        assert_eq!(strip_prompt_sigil("  build  % "), "build");
        assert_eq!(strip_prompt_sigil("plain title"), "plain title");
        assert_eq!(strip_prompt_sigil("root #"), "root");
        assert_eq!(strip_prompt_sigil(">"), "");
        // Only one trailing sigil is stripped.
        assert_eq!(strip_prompt_sigil("a $$"), "a $");
    }

    #[test]
    fn compose_row_label_follows_pr_title_branch_rules() {
        // PR + window title.
        assert_eq!(
            compose_row_label(Some(142), Some("superzej dev $"), "feat/x"),
            "[PR: 142 | superzej dev]"
        );
        // PR, no window title → branch inside the brackets.
        assert_eq!(
            compose_row_label(Some(7), None, "feat/x"),
            "[PR: 7 | feat/x]"
        );
        // PR with a window title that strips to empty → branch fallback.
        assert_eq!(
            compose_row_label(Some(9), Some(" $"), "main"),
            "[PR: 9 | main]"
        );
        // No PR, window title only.
        assert_eq!(
            compose_row_label(None, Some("cargo build"), "feat/x"),
            "cargo build"
        );
        // No PR, no title → branch.
        assert_eq!(compose_row_label(None, None, "feat/x"), "feat/x");
        assert_eq!(compose_row_label(None, Some("   "), "feat/x"), "feat/x");
    }

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
    fn new_worktree_renders_below_current_under_manual_sort() {
        // home(gi0), feat-a(gi1, current), then add feat-b(gi2) at the end.
        let mut s = session(
            vec![tab("app/home", "/wt/home"), tab("app/feat-a", "/wt/a")],
            1,
        );
        s.add_group(tab("app/feat-b", "/wt/b"));
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[], &[]);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["app", "home", "feat-a", "feat-b"]);
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
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[], &[]);
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
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[], &[]);
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
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[], &[]);
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
            folder_id: None,
        }];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &dbw, &[]);
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
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[]);
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
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[], &[]);
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
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[]);
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
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[]);
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
        let rows = build_rows(&s, &ws, &view, &act, &[], &[]);
        let busy = rows.iter().position(|r| r.label == "busy").unwrap();
        let home = rows.iter().position(|r| r.label == "home").unwrap();
        assert!(busy < home, "active worktree should sort first");
    }

    #[test]
    fn manual_sort_is_the_default_and_preserves_session_order() {
        // Session order is zebra, then alpha — deliberately *not* alphabetical.
        // The default (Manual) keeps that order (home first), so worktrees never
        // reshuffle on their own; only an explicit Name sort alphabetizes.
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/zebra", "/wt/zebra"),
                tab("app/alpha", "/wt/alpha"),
            ],
            0,
        );
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];

        // Default == Manual: home, then session order (zebra, alpha).
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &[], &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, vec!["home", "zebra", "alpha"]);

        // Name sort, by contrast, alphabetizes the non-home worktrees.
        let view = ViewState {
            sort: SortMode::Name,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, vec!["home", "alpha", "zebra"]);
    }

    #[test]
    fn unloaded_workspace_lists_db_worktrees_in_given_order() {
        // A workspace with no live session groups renders home + its registered
        // worktrees straight from the DB list, whose order the DB query fixes
        // (persisted `position`). build_rows preserves that order verbatim.
        let s = session(vec![], 0);
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let dbw = vec![
            DbWorktree {
                slug: "app".into(),
                branch: "zebra".into(),
                repo_path: "/repos/app".into(),
                tab_name: "app/zebra".into(),
                path: "/wt/zebra".into(),
                folder_id: None,
            },
            DbWorktree {
                slug: "app".into(),
                branch: "alpha".into(),
                repo_path: "/repos/app".into(),
                tab_name: "app/alpha".into(),
                path: "/wt/alpha".into(),
                folder_id: None,
            },
        ];
        let rows = build_rows(&s, &ws, &ViewState::default(), &no_activity(), &dbw, &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        // home synthesized first, then the DB order (not alphabetized).
        assert_eq!(labels, vec!["home", "zebra", "alpha"]);
    }
}
