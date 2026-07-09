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
    /// A static, first-class category banner (e.g. "TERMINALS"), rendered like
    /// the "WORKSPACES" title. Never collapses, never a navigation target.
    SectionHeading,
    /// A collapsible host group under the TERMINALS banner. Behaves like a
    /// `Workspace` for collapse purposes (keyed by `workspace_slug`).
    TerminalHost,
    Terminal,
    /// A passive, non-interactive placeholder shown under a section banner when
    /// it has no real rows (e.g. "No terminals — Alt T to add"). Rendered dim;
    /// carries no `tab_target`, so landing on it and pressing Enter is a no-op.
    EmptyHint,
}

/// Contextual activity, mirrored from the host-side `activity` state machine.
/// Drives the sidebar dot's glyph + color: `Active` (worktree busy / agent
/// working) is a filled white ●; `Waiting` (was active, now idle — the agent is
/// stuck waiting for the user, *unread*) is a filled red ●; `Read` (the user has
/// focused the tab but it is still stuck) is a hollow red ○; `None` (dormant)
/// renders no dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActivityState {
    #[default]
    None,
    Active,
    Waiting,
    Read,
    /// A worktree being created: the loop overlays this on rows whose tab is in
    /// `loading_state`, rendered as an accent "↻" while the splash builds.
    Loading,
}

impl ActivityState {
    pub fn from_str(s: &str) -> Self {
        match s {
            "active" => ActivityState::Active,
            "waiting" => ActivityState::Waiting,
            "read" => ActivityState::Read,
            "quiet" => ActivityState::Waiting, // legacy snapshots
            _ => ActivityState::None,          // "none" | "acked" | unknown
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
    /// User-controlled order: trusts the underlying sequence (the session's
    /// group order when loaded, the persisted `position` order when not),
    /// "home" first. Defaults to creation order and is what Shift+Alt+↑/↓
    /// rearranges. Worktrees never reshuffle on their own.
    Manual,
    /// Case-insensitive label order, "home" first. Stable — a worktree keeps
    /// its slot when selected/opened (no jumping). The old plugin's default.
    Name,
    /// Most-recently-touched first (by tab position as a recency proxy).
    Recent,
    /// The default: whatever needs the user floats first, by attention tier
    /// (blocked on input > failures > finished > ready-to-land > working >
    /// idle; see `superzej_core::attention`). Ordering follows the
    /// hysteresis-stable ranks from hydration, so rows only move on a real
    /// state change — never from timestamp or cache churn. Successor of the
    /// old CPU-dot-only `Activity` mode (whose persisted name still parses).
    #[default]
    Attention,
}

impl SortMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SortMode::Manual => "manual",
            SortMode::Name => "name",
            SortMode::Recent => "recent",
            SortMode::Attention => "attention",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "name" => SortMode::Name,
            "recent" => SortMode::Recent,
            // "activity" is the pre-attention name of this mode; saved
            // ui_state migrates by parsing it as Attention.
            "attention" | "activity" => SortMode::Attention,
            // Unknown / "manual" → the manual (creation-order) mode.
            _ => SortMode::Manual,
        }
    }
    /// Cycle to the next mode (for a single keybind).
    pub fn next(self) -> Self {
        match self {
            SortMode::Manual => SortMode::Name,
            SortMode::Name => SortMode::Recent,
            SortMode::Recent => SortMode::Attention,
            SortMode::Attention => SortMode::Manual,
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
    /// The worktree's remembered agent. The sidebar no longer renders an
    /// agent/app indicator, but the field is retained (populated from the DB) for
    /// other surfaces and future status lines.
    #[allow(dead_code)]
    pub agent: Option<String>,
    pub sandbox_backend: Option<String>,
    /// Selected execution environment (`[env.<name>]`); `None`/`"default"` ⇒
    /// the implicit default (no badge shown).
    pub env_name: Option<String>,
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
    /// Disk usage of this worktree's checkout (bytes), from the off-loop scan.
    pub disk_bytes: Option<u64>,
    /// Disk usage of this worktree's `target/` subtree (bytes) — the reclaimable
    /// portion. Drives the amber tint on the size badge when it dominates.
    pub target_bytes: Option<u64>,
    /// Connection string for terminal rows
    pub terminal_connection: Option<String>,
    /// Attention score: the worktree's own (Worktree rows) or the workspace's
    /// most-urgent-child rollup (Workspace rows — drives the collapsed-row
    /// glyph). Denormalized from `SidebarStatus` in one pass at build time.
    pub attention: Option<superzej_core::attention::AttentionScore>,
    /// The worktree's merge-queue status (its `merge_queue` row, if any) —
    /// drives the detail line's MQ chip. Denormalized in the same pass.
    pub mq_status: Option<superzej_core::attention::MqStatus>,
}

impl SidebarRow {
    /// Whether this row can join the multi-select set: a workspace or worktree
    /// with a stable identity. Excludes section headings / empty hints (no
    /// `pin_key`) and folders / terminals (no bulk or reorder action).
    pub fn is_markable(&self) -> bool {
        !self.pin_key.is_empty() && matches!(self.kind, RowKind::Workspace | RowKind::Worktree)
    }
}

/// Per-worktree status sourced from the (possibly slow) git/activity scan on
/// the hydration thread, merged onto rows at build time. `git`/`agent` are
/// keyed by worktree path; `activity` by tab name (matching the `activity`
/// state machine's TSV keys).
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// Per-worktree disk usage `(total_bytes, target_bytes)` from the
    /// `worktree_disk` cache (populated off-loop by the disk scan). Drives the
    /// sidebar size badge and the statusbar total.
    pub disk_sizes: std::collections::HashMap<String, (i64, i64)>,
    /// Worktrees mid-hibernation (snapshot taken, compute destroyed or being
    /// destroyed). In the status so its diff repaints the sidebar; the render
    /// path reads the mirroring `hibernator::is_hibernated` cache.
    pub hibernated: std::collections::BTreeSet<String>,
    /// Per-worktree attention score (keyed by path) — the tiered "what needs
    /// the user" model (see `superzej_core::attention`). Drives the Attention
    /// sort, the row reason hint, the jump action, and the statusbar chip.
    pub attention: std::collections::BTreeMap<String, superzej_core::attention::AttentionScore>,
    /// Hysteresis-stable display rank per worktree path (0 = most urgent).
    /// Computed on the hydration thread; only a tier or membership change
    /// reorders, so timestamp/cache churn never reshuffles rows.
    pub attention_ranks: std::collections::BTreeMap<String, u32>,
    /// Per-workspace rollup (keyed by slug): the most urgent worktree's score.
    /// Drives the collapsed-workspace glyph and workspace bubbling.
    pub workspace_attention:
        std::collections::BTreeMap<String, superzej_core::attention::AttentionScore>,
    /// Per-worktree merge-queue status (keyed by path) — the queue rows the
    /// attention scan already reads, re-exposed for the sidebar's MQ chip.
    pub mq: std::collections::BTreeMap<String, superzej_core::attention::MqStatus>,
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
    /// Workspace-level ordering, from `[ui] sidebar_workspace_sort` (config,
    /// not ui_state — mirrored here on startup/reload). When `Attention`,
    /// workspaces stable-sort by their most-urgent worktree's tier.
    pub workspace_sort: superzej_core::config::WorkspaceSort,
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
    pub sandbox_backend: Option<String>,
    /// Selected execution environment (`[env.<name>]`); `None`/`"default"` ⇒
    /// the implicit default (no badge shown).
    pub env_name: Option<String>,
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
    /// Worktree path — the key into the attention-rank map for the
    /// [`SortMode::Attention`] ordering.
    path: String,
    activity: ActivityState,
    sandbox_backend: Option<String>,
    env_name: Option<String>,
    folder_id: Option<i64>,
}

/// Map a terminal's connection to its host group: `(collapse-key, display
/// label, is_local)`. Local terminals (empty connection, or the literal
/// `local`/`shell`) all collapse into one `local` group. Remote terminals group
/// by host: the `ssh `/`mosh ` prefix is stripped, then the portion after the
/// last `@` is the host — so `dave@prod` and `root@prod` fold into one `prod`
/// section. The collapse-key is lowercased so casing never splits a group.
pub(crate) fn terminal_host(conn: &str, kind: &str) -> (String, String, bool) {
    let c = conn.trim();
    if c.is_empty() || c == "local" || c == "shell" || kind == "local" {
        return ("local".into(), "local".into(), true);
    }
    let target = c
        .strip_prefix("ssh ")
        .or_else(|| c.strip_prefix("mosh "))
        .unwrap_or(c)
        .trim();
    let host = target.rsplit('@').next().unwrap_or(target).trim();
    let host = if host.is_empty() { target } else { host };
    (host.to_lowercase(), host.to_string(), false)
}

/// The expanded cursor row's second line: the secondary metadata that would
/// crowd the always-on row — execution env, sandbox backend, hibernation,
/// open PRs, unread notifications, and disk size. `None` when the row has
/// nothing extra to show. (Extracted from the ratchet-pinned `chrome.rs`.)
pub(crate) fn compose_detail_line(row: &SidebarRow) -> Option<crate::seg::Line> {
    use crate::chrome::S;
    use crate::seg::{Line, Seg, Tok, seg, sp};
    use superzej_core::theme;
    // Gutter + indent so the detail reads as hanging under the name.
    let mut segs: Vec<Seg> = vec![sp(5)];
    let start = segs.len();
    let dirty = row.git.is_some_and(|g| g.dirty);
    crate::sidebar_legend::push_row_markers(dirty, &mut segs);
    crate::sidebar_legend::push_attention_reason(row.attention.as_ref(), &mut segs);

    if let Some(env) = &row.env_name
        && !env.is_empty()
        && env != "default"
    {
        segs.push(seg(Tok::Slot(S::Faint), format!("\u{ab}{env}\u{bb} ")));
    }
    if let Some(backend) = &row.sandbox_backend
        && !backend.is_empty()
        && backend != "none"
        && backend != "host"
    {
        segs.push(seg(Tok::Slot(S::Faint), format!("({backend}) ")));
    }
    if row
        .worktree_path
        .as_deref()
        .is_some_and(crate::hibernator::is_hibernated)
    {
        let moon = crate::caps::active_glyphs().moon;
        segs.push(seg(Tok::Slot(S::Faint), format!("{moon} hibernated ")));
    }
    if let Some(pr) = row.pr_count.filter(|&c| c > 0) {
        let hex = crate::caps::active_glyphs().hex;
        segs.push(seg(Tok::Hue(theme::Hue::Green), format!("{hex} {pr} PR "))); // ⬡N PR
    }
    if let Some((glyph, hue)) = row.mq_status.and_then(mq_chip) {
        segs.push(seg(Tok::Hue(hue), format!("{glyph} MQ ")));
    }
    if row.unread_count > 0 {
        let mail = crate::caps::active_glyphs().mail;
        let blue = Tok::Hue(theme::Hue::Blue);
        segs.push(seg(blue, format!("{mail} {} unread ", row.unread_count)));
    }
    if let Some(total) = row.disk_bytes {
        let target = row.target_bytes.unwrap_or(0);
        let heavy = target > 1024 * 1024 * 1024 && target * 2 > total;
        let fg = if heavy {
            Tok::Hue(theme::Hue::Amber)
        } else {
            Tok::Slot(S::Dim)
        };
        segs.push(seg(fg, superzej_core::disk::human(total)));
    }

    (segs.len() > start).then_some(Line::Segs(segs))
}

/// The detail line's merge-queue chip for a status: the section's glyph
/// vocabulary (see `panel/sections/merge_queue.rs::status_glyph`), minus
/// `Landed` — a finished row is panel detail, not sidebar-worthy signal.
fn mq_chip(
    mq: superzej_core::attention::MqStatus,
) -> Option<(&'static str, superzej_core::theme::Hue)> {
    use superzej_core::attention::MqStatus as M;
    use superzej_core::theme::Hue;
    Some(match mq {
        M::Landed => return None,
        M::Ready => ("◆", Hue::Green),
        M::Deferred | M::GateFailed => ("⚑", Hue::Red),
        M::NeedsHuman => ("✋", Hue::Red),
        M::Folding | M::Verifying => ("●", Hue::Amber),
        M::AgentRunning => ("◐", Hue::Amber),
        M::Queued => ("○", Hue::Blue),
    })
}

/// Group `db_terminals` into host sections in sidebar **display order**:
/// `local` first, then remote hosts by case-insensitive label. Each entry is
/// `(collapse-key, display-label, is_local, terminals-in-order)`. The
/// collapse-key is the bare host key; the row slug is `terminals/host:{key}`.
/// Shared by the tree builder ([`build_rows`]) and the host/leaf region
/// navigation in `run.rs`, so both see exactly one ordering.
pub fn terminal_hosts_ordered(
    db_terminals: &[superzej_core::models::TerminalRow],
) -> Vec<(
    String,
    String,
    bool,
    Vec<&superzej_core::models::TerminalRow>,
)> {
    let mut host_order: Vec<(String, String, bool)> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&superzej_core::models::TerminalRow>> =
        std::collections::HashMap::new();
    for t in db_terminals {
        let (key, label, local) = terminal_host(&t.connection_string, &t.kind);
        if !groups.contains_key(&key) {
            host_order.push((key.clone(), label, local));
        }
        groups.entry(key).or_default().push(t);
    }
    // `local` sorts first; the rest by label, case-insensitively.
    host_order.sort_by(|a, b| {
        b.2.cmp(&a.2) // local (true) before remote (false)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    host_order
        .into_iter()
        .map(|(key, label, local)| {
            let terms = groups.remove(&key).unwrap_or_default();
            (key, label, local, terms)
        })
        .collect()
}

/// Build the full ordered row list for the tree. `workspaces` is the
/// `(slug, display, kind, repo_path)` list in workspace order (caller pulls it
/// from the DB + live groups). `status` carries per-worktree status merged
/// onto rows. `db_worktrees` backs the rows of workspaces that are NOT loaded
/// in the session — every workspace shows its home + registered worktrees,
/// and activating one switches workspace. (Configured `[host.*]` machines live
/// in the System ▸ Hosts panel section, not the sidebar.)
#[allow(clippy::too_many_arguments)]
pub fn build_rows(
    session: &Session,
    workspaces: &[(String, String, String, String)],
    view: &ViewState,
    status: &SidebarStatus,
    db_worktrees: &[DbWorktree],
    db_folders: &[superzej_core::models::FolderRow],
    db_terminals: &[superzej_core::models::TerminalRow],
) -> Vec<SidebarRow> {
    let activity = &status.activity;
    let mut rows = Vec::new();

    // Workspace bubbling (`[ui] sidebar_workspace_sort = "attention"`): order
    // workspaces by their most-urgent worktree's tier. The sort is stable and
    // tier-granular, so equal-urgency workspaces keep their manual order and a
    // workspace only moves on a real tier change — hysteresis for free.
    let mut workspaces: Vec<&(String, String, String, String)> = workspaces.iter().collect();
    if view.workspace_sort == superzej_core::config::WorkspaceSort::Attention {
        workspaces.sort_by_key(|(slug, ..)| {
            status
                .workspace_attention
                .get(slug)
                .map(|s| s.tier as u8)
                .unwrap_or(u8::MAX)
        });
    }

    // Index the DB rows once. `build_rows` runs on every tab/worktree switch
    // AND on every filter keystroke, and the per-group / per-folder lookups
    // below were linear rescans of `db_worktrees` — O(groups × worktrees) per
    // rebuild. `or_insert` keeps `.find()`'s first-match semantics.
    let mut db_by_tab: std::collections::HashMap<&str, &DbWorktree> =
        std::collections::HashMap::with_capacity(db_worktrees.len());
    let mut db_by_folder: std::collections::HashMap<i64, Vec<&DbWorktree>> =
        std::collections::HashMap::new();
    // Per-slug rows keep `db_worktrees`' order (the DB's position ordering).
    let mut db_by_slug: std::collections::HashMap<&str, Vec<&DbWorktree>> =
        std::collections::HashMap::new();
    for w in db_worktrees {
        db_by_tab.entry(w.tab_name.as_str()).or_insert(w);
        if let Some(fid) = w.folder_id {
            db_by_folder.entry(fid).or_default().push(w);
        }
        db_by_slug.entry(w.slug.as_str()).or_default().push(w);
    }

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
            sandbox_backend: None,
            env_name: None,
            activity: ActivityState::None,
            visible: true,
            collapsed,
            dir: kind == "dir",
            pr_count: None,
            pr_number: None,
            unread_count: 0,
            alert_count: 0,
            terminal_connection: None,
            disk_bytes: None,
            target_bytes: None,
            attention: None,
            mq_status: None,
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
            let dbw = db_by_tab.get(g.name.as_str());
            groups.push(Group {
                label: branch,
                gi,
                path: g.path.clone(),
                sandbox_backend: dbw.and_then(|w| w.sandbox_backend.clone()),
                env_name: dbw.and_then(|w| w.env_name.clone()),
                activity: activity.get(&g.name).copied().unwrap_or_default(),
                folder_id: dbw.and_then(|w| w.folder_id),
            });
        }

        sort_groups(&mut groups, view.sort, &status.attention_ranks);
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
                sandbox_backend: gr.sandbox_backend.clone(),
                env_name: gr.env_name.clone(),
                activity: gr.activity,
                visible: !collapsed,
                collapsed: false,
                dir: false,
                pr_count,
                pr_number,
                unread_count,
                alert_count,
                terminal_connection: None,
                disk_bytes: None,
                target_bytes: None,
                attention: None,
                mq_status: None,
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
            // (indexed lookup), so the count stays accurate when the workspace
            // is dormant.
            for w in db_by_folder
                .get(&folder.folder_id)
                .map(Vec::as_slice)
                .unwrap_or_default()
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
                sandbox_backend: None,
                env_name: None,
                activity: ActivityState::None,
                visible: !collapsed,
                collapsed: folder_collapsed,
                dir: false,
                pr_count: None,
                pr_number: None,
                unread_count: 0,
                alert_count: 0,
                terminal_connection: None,
                disk_bytes: None,
                target_bytes: None,
                attention: None,
                mq_status: None,
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
                            sandbox_backend: gr.sandbox_backend.clone(),
                            env_name: gr.env_name.clone(),
                            activity: gr.activity,
                            visible: !collapsed,
                            collapsed: false,
                            dir: false,
                            pr_count,
                            pr_number,
                            unread_count,
                            alert_count,
                            terminal_connection: None,
                            disk_bytes: None,
                            target_bytes: None,
                            attention: None,
                            mq_status: None,
                        });
                    }
                }
            }
        }

        // A workspace with no live session groups still shows its home and
        // registered worktrees; activating one switches workspace.
        if !live && !repo_path.is_empty() {
            let mk = |label: &str,
                      group: Option<String>,
                      path: Option<String>,
                      backend: Option<String>,
                      env: Option<String>| {
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
                    sandbox_backend: backend,
                    env_name: env,
                    activity: act,
                    visible: !collapsed,
                    collapsed: false,
                    dir: false,
                    pr_count,
                    pr_number,
                    unread_count,
                    alert_count,
                    terminal_connection: None,
                    disk_bytes: None,
                    target_bytes: None,
                    attention: None,
                    mq_status: None,
                }
            };
            rows.push(mk(
                "home",
                Some(format!("{repo_slug}/home")),
                Some(repo_path.clone()),
                None,
                None,
            ));
            // A registry row for the home checkout would duplicate the
            // synthesized row above — skip it.
            for w in db_by_slug
                .get(repo_slug.as_str())
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter(|w| w.branch != "home")
            {
                rows.push(mk(
                    &w.branch,
                    Some(w.tab_name.clone()),
                    Some(w.path.clone()),
                    w.sandbox_backend.clone(),
                    w.env_name.clone(),
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
            sandbox_backend: None,
            env_name: None,
            activity: ActivityState::None,
            visible: true,
            collapsed: false,
            dir: false,
            pr_count: None,
            pr_number: None,
            unread_count: 0,
            alert_count: 0,
            terminal_connection: None,
            disk_bytes: None,
            target_bytes: None,
            attention: None,
            mq_status: None,
        });
    }

    {
        // TERMINALS is a first-class, static category banner (a peer of the
        // "WORKSPACES" title), never collapsible and never a nav target. It is
        // always shown — even with no terminals — so the section (and its
        // "New terminal…" entry point) never silently vanishes.
        rows.push(SidebarRow {
            kind: RowKind::SectionHeading,
            depth: 0,
            label: "TERMINALS".into(),
            workspace_slug: "terminals".into(),
            tab_target: None,
            active: false,
            worktree_path: None,
            pin_key: String::new(),
            branch: None,
            git: None,
            agent: None,
            sandbox_backend: None,
            env_name: None,
            activity: ActivityState::None,
            visible: true,
            collapsed: false,
            dir: false,
            pr_count: None,
            pr_number: None,
            unread_count: 0,
            alert_count: 0,
            terminal_connection: None,
            disk_bytes: None,
            target_bytes: None,
            attention: None,
            mq_status: None,
        });

        // Genuinely-empty fallback (the startup reseed normally keeps a `local`
        // terminal, so this shows only when that couldn't run): a passive,
        // non-interactive hint pointing at the add flow.
        if db_terminals.is_empty() {
            rows.push(SidebarRow {
                kind: RowKind::EmptyHint,
                depth: 1,
                label: "No terminals — Alt T to add".into(),
                workspace_slug: "terminals".into(),
                tab_target: None,
                active: false,
                worktree_path: None,
                pin_key: String::new(),
                branch: None,
                git: None,
                agent: None,
                sandbox_backend: None,
                env_name: None,
                activity: ActivityState::None,
                visible: true,
                collapsed: false,
                dir: false,
                pr_count: None,
                pr_number: None,
                unread_count: 0,
                alert_count: 0,
                terminal_connection: None,
                disk_bytes: None,
                target_bytes: None,
                attention: None,
                mq_status: None,
            });
        }

        // Under the banner, terminals divide into collapsible sections by host,
        // `local` first, then remote hosts in stable label order. Grouping +
        // ordering is shared with the region-navigation logic in `run.rs`.
        let host_order = terminal_hosts_ordered(db_terminals);

        for (key, label, local, terms) in &host_order {
            let slug = format!("terminals/host:{key}");
            let collapsed = view.collapsed.contains(&slug);
            // A representative connection drives the host glyph (💻 vs 🌐).
            let rep_conn = if *local {
                String::new()
            } else {
                terms
                    .first()
                    .map(|t| t.connection_string.clone())
                    .unwrap_or_default()
            };

            rows.push(SidebarRow {
                kind: RowKind::TerminalHost,
                depth: 1,
                label: label.clone(),
                workspace_slug: slug.clone(),
                tab_target: None,
                active: false,
                worktree_path: None,
                pin_key: String::new(),
                branch: None,
                git: None,
                agent: None,
                sandbox_backend: None,
                env_name: None,
                activity: ActivityState::None,
                visible: true,
                collapsed,
                dir: false,
                pr_count: None,
                pr_number: None,
                unread_count: 0,
                alert_count: 0,
                terminal_connection: Some(rep_conn),
                disk_bytes: None,
                target_bytes: None,
                attention: None,
                mq_status: None,
            });

            if collapsed {
                continue;
            }
            for t in terms {
                let active = session
                    .worktrees
                    .get(session.active)
                    .is_some_and(|wt| wt.name == t.name);
                let target = session
                    .worktrees
                    .iter()
                    .position(|w| w.name == t.name)
                    .map(|i| RowTarget::Tab(i, 0));

                rows.push(SidebarRow {
                    kind: RowKind::Terminal,
                    depth: 2,
                    label: t.name.clone(),
                    workspace_slug: slug.clone(),
                    tab_target: target.or_else(|| {
                        Some(RowTarget::Workspace {
                            repo_path: "terminal".into(),
                            group: Some(t.name.clone()),
                        })
                    }),
                    active,
                    worktree_path: Some(t.name.clone()),
                    pin_key: format!("terminals/{}", t.name),
                    branch: None,
                    git: None,
                    agent: None,
                    // Show the sandbox backend on the row like a worktree does;
                    // blank/`host` means an un-sandboxed shell (rendered as none).
                    sandbox_backend: {
                        let b = t.sandbox_backend.trim();
                        (!b.is_empty() && b != "host" && b != "none").then(|| b.to_string())
                    },
                    env_name: (!t.env_name.trim().is_empty()).then(|| t.env_name.clone()),
                    activity: ActivityState::None,
                    visible: true,
                    collapsed: false,
                    dir: false,
                    pr_count: None,
                    pr_number: None,
                    unread_count: 0,
                    alert_count: 0,
                    terminal_connection: Some(t.connection_string.clone()),
                    disk_bytes: None,
                    target_bytes: None,
                    attention: None,
                    mq_status: None,
                });
            }
        }
    }

    // Denormalize cached disk sizes onto every worktree row (one pass, keyed by
    // path), so the badge renderer reads them straight off the row like the
    // PR/unread/alert counts.
    if !status.disk_sizes.is_empty() {
        for row in &mut rows {
            if let Some(p) = &row.worktree_path
                && let Some(&(total, target)) = status.disk_sizes.get(p)
            {
                row.disk_bytes = Some(total.max(0) as u64);
                row.target_bytes = Some(target.max(0) as u64);
            }
        }
    }

    // Denormalize attention scores the same way: a worktree row carries its own
    // score (keyed by path); a workspace row carries its rollup (keyed by slug)
    // so a collapsed workspace still shows its most urgent child's glyph.
    for row in &mut rows {
        match row.kind {
            RowKind::Worktree => {
                row.attention = row
                    .worktree_path
                    .as_deref()
                    .and_then(|p| status.attention.get(p))
                    .copied();
                row.mq_status = row
                    .worktree_path
                    .as_deref()
                    .and_then(|p| status.mq.get(p))
                    .copied();
            }
            RowKind::Workspace => {
                row.attention = status.workspace_attention.get(&row.workspace_slug).copied();
            }
            _ => {}
        }
    }

    apply_pins(&mut rows, &view.pins);
    apply_filter(&mut rows, &view.filter);
    rows
}

fn sort_groups(
    groups: &mut [Group],
    sort: SortMode,
    ranks: &std::collections::BTreeMap<String, u32>,
) {
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
        SortMode::Attention => {
            // Most urgent first, by the hysteresis-stable ranks computed on the
            // hydration thread (see `attention_status`). A path with no rank yet
            // (brand-new worktree, first pass) keeps its manual slot at the end.
            groups.sort_by(|a, b| {
                let r = |g: &Group| ranks.get(&g.path).copied().unwrap_or(u32::MAX);
                r(a).cmp(&r(b))
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
    // A worktree match surfaces its parent repo header; a terminal match
    // surfaces both its host group and the TERMINALS banner; a header that
    // itself matched reveals its whole subtree.
    let mut last_workspace: Option<usize> = None;
    let mut last_section: Option<usize> = None;
    let mut last_host: Option<usize> = None;
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => last_workspace = Some(i),
            RowKind::Folder => {}
            RowKind::SectionHeading => last_section = Some(i),
            RowKind::TerminalHost => {
                last_host = Some(i);
                if keep[i]
                    && let Some(s) = last_section
                {
                    keep[s] = true; // surface the TERMINALS banner
                }
            }
            RowKind::Terminal => {
                if keep[i] {
                    if let Some(h) = last_host {
                        keep[h] = true; // surface the host group
                    }
                    if let Some(s) = last_section {
                        keep[s] = true; // surface the TERMINALS banner
                    }
                }
            }
            RowKind::Worktree => {
                if keep[i]
                    && let Some(w) = last_workspace
                {
                    keep[w] = true; // surface the parent repo header
                }
            }
            RowKind::EmptyHint => {}
        }
    }
    // Reveal children only for headers/groups that matched on their own label.
    let mut reveal_ws = false; // inside a self-matched workspace
    let mut reveal_section = false; // inside a self-matched TERMINALS banner
    let mut reveal_host = false; // inside a self-matched host group
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => reveal_ws = self_match[i],
            RowKind::Folder => {}
            RowKind::SectionHeading => reveal_section = self_match[i],
            RowKind::TerminalHost => {
                reveal_host = self_match[i] || reveal_section;
                if reveal_section {
                    keep[i] = true;
                }
            }
            RowKind::Terminal => {
                if reveal_host {
                    keep[i] = true;
                }
            }
            RowKind::Worktree => {
                if reveal_ws {
                    keep[i] = true;
                }
            }
            RowKind::EmptyHint => {}
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
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
        // The TERMINALS section is always present (empty here → its hint row).
        let labels: Vec<&str> = rows
            .iter()
            .take_while(|r| r.kind != RowKind::SectionHeading)
            .map(|r| r.label.as_str())
            .collect();
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
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
        // Ignore the always-present TERMINALS section (empty → hint row).
        let labels: Vec<&str> = rows
            .iter()
            .take_while(|r| r.kind != RowKind::SectionHeading)
            .map(|r| r.label.as_str())
            .collect();
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
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
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
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
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
            sandbox_backend: None,
            env_name: Some("company-k8s".into()),
        }];
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        // The DB worktree's selected env flows onto its (unloaded-workspace) row.
        let feat = rows
            .iter()
            .find(|r| r.workspace_slug == "other" && r.label == "feat-x")
            .unwrap();
        assert_eq!(feat.env_name.as_deref(), Some("company-k8s"));
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
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
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
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
        // Only the workspace-structure rows; the always-present TERMINALS
        // section (heading + empty-state hint) trails and is excluded here.
        let kinds: Vec<RowKind> = rows
            .iter()
            .take_while(|r| r.kind != RowKind::SectionHeading)
            .map(|r| r.kind)
            .collect();
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
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
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
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
        // Workspace block contains all rows (depth>0), so pinning the worktree
        // inside reorders within — feat should precede home.
        let feat = rows.iter().position(|r| r.label == "feat").unwrap();
        let home = rows.iter().position(|r| r.label == "home").unwrap();
        assert!(feat < home, "pinned feat should sort before home");
    }

    #[test]
    fn attention_sort_orders_by_hydration_ranks() {
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/calm", "/wt/calm"),
                tab("app/urgent", "/wt/urgent"),
            ],
            0,
        );
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let mut status = no_activity();
        // Hydration-computed ranks: urgent first, then home, then calm.
        for (p, r) in [("/wt/urgent", 0u32), ("/wt/home", 1), ("/wt/calm", 2)] {
            status.attention_ranks.insert(p.into(), r);
        }
        let urgent_score = superzej_core::attention::AttentionScore {
            tier: superzej_core::attention::AttentionTier::Blocked,
            sub: 0,
            reason: superzej_core::attention::AttentionReason::AgentNeedsInput,
            since: Some(100),
        };
        status.attention.insert("/wt/urgent".into(), urgent_score);
        let view = ViewState {
            sort: SortMode::Attention,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &status, &[], &[], &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, vec!["urgent", "home", "calm"]);
        // The urgent row carries its score for the legend/glyph.
        let urgent = rows.iter().find(|r| r.label == "urgent").unwrap();
        assert_eq!(urgent.attention, Some(urgent_score));
    }

    #[test]
    fn attention_sort_without_ranks_keeps_manual_order() {
        // No hydration pass yet (empty ranks): the Attention default degrades
        // to the manual order — home first, then session order — so a fresh
        // launch never flashes a reshuffle.
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

        // The default is Attention now.
        assert_eq!(ViewState::default().sort, SortMode::Attention);
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
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
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, vec!["home", "alpha", "zebra"]);
    }

    #[test]
    fn sort_mode_migrates_and_cycles() {
        // The old persisted "activity" value parses as Attention (the ui_state
        // migration), and the cycle visits all four modes.
        assert_eq!(SortMode::from_str("activity"), SortMode::Attention);
        assert_eq!(SortMode::from_str("attention"), SortMode::Attention);
        assert_eq!(SortMode::from_str("manual"), SortMode::Manual);
        assert_eq!(SortMode::from_str("bogus"), SortMode::Manual);
        assert_eq!(SortMode::Attention.as_str(), "attention");
        assert_eq!(SortMode::default(), SortMode::Attention);
        let mut m = SortMode::Manual;
        let mut seen = vec![m];
        for _ in 0..3 {
            m = m.next();
            seen.push(m);
        }
        assert_eq!(
            seen,
            vec![
                SortMode::Manual,
                SortMode::Name,
                SortMode::Recent,
                SortMode::Attention
            ]
        );
        assert_eq!(m.next(), SortMode::Manual);
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
                sandbox_backend: None,
                env_name: None,
            },
            DbWorktree {
                slug: "app".into(),
                branch: "alpha".into(),
                repo_path: "/repos/app".into(),
                tab_name: "app/alpha".into(),
                path: "/wt/alpha".into(),
                folder_id: None,
                sandbox_backend: None,
                env_name: None,
            },
        ];
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        // home synthesized first, then the DB order (not alphabetized).
        assert_eq!(labels, vec!["home", "zebra", "alpha"]);
    }

    fn term(name: &str, kind: &str, conn: &str) -> superzej_core::models::TerminalRow {
        superzej_core::models::TerminalRow {
            id: 0,
            name: name.into(),
            kind: kind.into(),
            connection_string: conn.into(),
            folder_id: None,
            created_at: 0,
            last_active: 0,
            position: 0,
            sandbox_backend: String::new(),
            env_name: String::new(),
        }
    }

    #[test]
    fn terminal_host_derives_group() {
        assert_eq!(
            terminal_host("", "local"),
            ("local".into(), "local".into(), true)
        );
        assert_eq!(
            terminal_host("local", ""),
            ("local".into(), "local".into(), true)
        );
        assert_eq!(
            terminal_host("shell", ""),
            ("local".into(), "local".into(), true)
        );
        // ssh/mosh strip the prefix and group by the host after the last '@'.
        assert_eq!(
            terminal_host("ssh dave@prod", "remote"),
            ("prod".into(), "prod".into(), false)
        );
        assert_eq!(
            terminal_host("mosh root@prod", "remote"),
            ("prod".into(), "prod".into(), false)
        );
        // A bare host with no user/prefix is used as-is (lowercased key).
        assert_eq!(
            terminal_host("Box1.internal", "remote"),
            ("box1.internal".into(), "Box1.internal".into(), false)
        );
    }

    #[test]
    fn terminals_render_under_banner_grouped_by_host_local_first() {
        let s = session(vec![], 0);
        let ws: Vec<(String, String, String, String)> = vec![];
        let terms = vec![
            term("term-ssh-dave-prod", "remote", "ssh dave@prod"),
            term("local", "local", ""),
            term("term-ssh-root-prod", "remote", "ssh root@prod"),
        ];
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &terms,
        );
        // One static banner.
        let banners: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::SectionHeading)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(banners, vec!["TERMINALS"]);
        // Host groups: local first, then `prod` (both ssh-to-prod terminals fold
        // into one group).
        let hosts: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::TerminalHost)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(hosts, vec!["local", "prod"]);
        // The two prod terminals live under the prod host group.
        let term_labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Terminal)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(
            term_labels,
            vec!["local", "term-ssh-dave-prod", "term-ssh-root-prod"]
        );
    }

    #[test]
    fn collapsed_host_hides_its_terminals() {
        let s = session(vec![], 0);
        let ws: Vec<(String, String, String, String)> = vec![];
        let terms = vec![term("local", "local", ""), term("t1", "remote", "ssh prod")];
        let view = ViewState {
            collapsed: ["terminals/host:prod".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &terms);
        // The prod host row is present but its terminal `t1` is not.
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::TerminalHost && r.label == "prod")
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.kind == RowKind::Terminal && r.label == "t1")
        );
        // The local group is still expanded.
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::Terminal && r.label == "local")
        );
    }

    #[test]
    fn filter_surfaces_host_group_and_banner() {
        let s = session(vec![], 0);
        let ws: Vec<(String, String, String, String)> = vec![];
        let terms = vec![
            term("local", "local", ""),
            term("web-prod", "remote", "ssh prod"),
        ];
        let view = ViewState {
            filter: "web-prod".into(),
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &terms);
        let visible: Vec<(&RowKind, &str)> = rows
            .iter()
            .filter(|r| r.visible)
            .map(|r| (&r.kind, r.label.as_str()))
            .collect();
        // The matched terminal, its host group, and the banner stay visible; the
        // unrelated `local` group is filtered out.
        assert!(visible.contains(&(&RowKind::SectionHeading, "TERMINALS")));
        assert!(visible.contains(&(&RowKind::TerminalHost, "prod")));
        assert!(visible.contains(&(&RowKind::Terminal, "web-prod")));
        assert!(!visible.iter().any(|(_, l)| *l == "local"));
    }
}
