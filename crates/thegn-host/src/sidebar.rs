//! The workspace tree's structured row model and builder.
//!
//! The sidebar shows **workspaces** (repos) at depth 0 and their **worktrees**
//! at depth 1 — a worktree's tabs live in the tabbar only, never here. Rows
//! come straight from the session's `WorktreeGroup` model (no name parsing).
//! It produces a `Vec<SidebarRow>` carrying enough structure for interaction
//! (collapse, filter, sort, pin, multi-select) and per-row status (git glyphs,
//! agent, activity dot). Glyph/connector composition lives at render time in
//! `sidebar_view::draw_sidebar`.

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
    /// A passive placeholder shown under a section banner when it has no real
    /// rows (e.g. "No terminals — Enter to add"). Rendered dim; carries no
    /// `tab_target`, but Enter (and a click) on it runs the hinted action —
    /// the key/mouse handlers synthesize `Action::NewTerminal` for it.
    EmptyHint,
    /// A single compact rollup of the **agent-dispatch roster** — `Pipeline ▸ 3
    /// running`. Emitted only while the roster has live rows (see
    /// [`PipelineSummary`]), never collapses, and is not a navigation target:
    /// `↵`/click synthesize `Action::OpenPipelineBoard`, exactly as an
    /// [`RowKind::EmptyHint`] synthesizes its hinted action.
    PipelineSummary,
    /// A **derived** `Pipelines` folder under a workspace: one per workspace
    /// that has at least one pipeline lane, holding the lanes (see
    /// [`crate::sidebar_pipeline`]). Collapsible; carries no `folder_id` and
    /// no `tab_target`, and is deliberately NOT a [`RowKind::Folder`]: the
    /// file/unfile/rename/reorder paths all resolve a real `folders` row,
    /// which a derived folder has not got.
    PipelineGroup,
    /// A **derived** pipeline lane folder (`THE-74`), one per issue the
    /// dispatch roster knows — rows of ANY status, so it survives a restart
    /// (see [`crate::sidebar_pipeline`]). Collapsible; carries no `folder_id`
    /// and no `tab_target`, and is deliberately NOT a [`RowKind::Folder`] for
    /// the same reason: there is no `folders` row behind it.
    PipelineLane,
    /// A worktree the lane's roster rows reference, as a leaf inside its lane
    /// folder. A **mirror**: it carries the same `tab_target` the primary
    /// worktree row does, but its OWN `pin_key` (`pipeline/lane:{key}/wt:{path}`
    /// — never the primary row's), and every pin/mark/bulk path skips it by
    /// kind (`is_markable` / `is_pinnable` / the drag sources all gate on
    /// `RowKind`). The key is for identity anchoring only: the context menu's
    /// re-anchor, double-click detection and the rebuild's cursor re-seek all
    /// resolve rows by `pin_key`, and an empty key would resolve to the first
    /// keyless row instead. A mirrored [`RowKind::Worktree`] would also make
    /// the board's "jump to this worktree" depend on emission order.
    PipelineWorktree,
}

impl RowKind {
    /// Whether rows of this kind head a collapsible subtree (drive a caret and
    /// respond to the collapse/expand keys). Workspaces, 📂 folder sub-groups,
    /// terminal-host groups and the derived pipeline group/lane folders
    /// collapse; leaves and banners do not.
    pub fn is_collapsible(self) -> bool {
        matches!(
            self,
            RowKind::Workspace
                | RowKind::Folder
                | RowKind::TerminalHost
                | RowKind::PipelineGroup
                | RowKind::PipelineLane
        )
    }
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
    /// A worktree whose (non-local) env failed to come up with `failover` off:
    /// the loop overlays this on rows in `materialize_failed`/`prewarm_failed`,
    /// rendered as a red "✗" so the failure stays visible after the halt modal
    /// is dismissed.
    Failed,
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
/// changes; `ahead`/`behind` are vs the upstream (absent when no upstream);
/// `add`/`del` are the uncommitted working-tree line stat vs HEAD; `branch_diff`
/// is the `(added, deleted)` total vs the default branch (`None` when no base).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitGlyphs {
    pub dirty: bool,
    pub ahead: usize,
    pub behind: usize,
    pub add: u32,
    pub del: u32,
    pub branch_diff: Option<(u32, u32)>,
    /// The worktree's repo is colocated with jujutsu (a `.jj/` beside `.git/`).
    /// Drives the sidebar jj marker; a directory check, never a `jj` subprocess.
    pub jj: bool,
}

/// Tree ordering for worktree groups within a workspace (item 23).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// The default: user-controlled order that trusts the underlying sequence
    /// (the session's group order when loaded, the persisted `position` order
    /// when not), "home" first. Defaults to creation order and is what
    /// Shift+↑/↓ rearranges. Worktrees never reshuffle on their own — the
    /// explorer contract users expect; urgency still surfaces through the
    /// activity dots, the statusbar needs-you chip, and `Alt a`.
    #[default]
    Manual,
    /// Case-insensitive label order, "home" first. Stable — a worktree keeps
    /// its slot when selected/opened (no jumping). The old plugin's default.
    Name,
    /// Newest first — reverse tab position, i.e. reverse creation order (the
    /// menu labels it "recent — newest first"; activation does NOT reorder
    /// the session, so this is not last-used. `Live` is the real-recency
    /// sort).
    Recent,
    /// Whatever needs the user floats first, by attention tier (blocked on
    /// input > failures > finished > ready-to-land > working > idle; see
    /// `thegn_core::attention`). Ordering follows the hysteresis-stable
    /// ranks from hydration, so rows only move on a real state change — never
    /// from timestamp or cache churn. Successor of the old CPU-dot-only
    /// `Activity` mode (whose persisted name still parses).
    Attention,
    /// Most-recently-active first, by the real per-worktree activity timestamp
    /// (`SidebarStatus::activity_recency`, computed off-loop on the hydration
    /// thread from the activity FSM's `last_active_at`). Unlike `Recent` (tab
    /// position, a fixed proxy) this reflects genuine process/agent work: the
    /// worktree that most recently ran code floats to the top. Unlike
    /// `Attention` (tiered urgency with hysteresis) it is a pure recency
    /// ranking, so a just-touched worktree bubbles even if it needs nothing.
    /// A more-recently-active worktree outranks "home"; worktrees the FSM has
    /// never seen active keep their manual slot at the end.
    Live,
}

impl SortMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SortMode::Manual => "manual",
            SortMode::Name => "name",
            SortMode::Recent => "recent",
            SortMode::Attention => "attention",
            SortMode::Live => "live",
        }
    }
    pub fn from_str(s: &str) -> Self {
        match s {
            "manual" => SortMode::Manual,
            "name" => SortMode::Name,
            "recent" => SortMode::Recent,
            "live" => SortMode::Live,
            // "activity" is the pre-attention name of this mode; saved
            // ui_state migrates by parsing it as Attention (and `load`
            // rewrites the stored value to the canonical spelling).
            "attention" | "activity" => SortMode::Attention,
            // Unknown → the default.
            _ => SortMode::default(),
        }
    }
}

/// Roster rollup behind the sidebar's [`RowKind::PipelineSummary`] row.
///
/// Counted off-loop with the rest of [`SidebarStatus`] (see
/// `monitor_pipeline::summary`) from the roster read the attention scan already
/// does — no new query, no new wake source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineSummary {
    /// Rows whose worker is (or should be) live — `AgentDispatchStatus::is_active`.
    pub active: usize,
    /// The `waiting_human` subset of [`Self::active`]: the part parked on a
    /// person. Rendered in the attention tone, because it is the half of the
    /// count that is asking for something.
    pub waiting_human: usize,
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
    /// For Worktree rows: the worktree path — the key for git/activity/disk
    /// lookups, and for row actions like "copy path". For Workspace rows: the
    /// repo path (the remove-workspace target), or `None` for a live fallback
    /// with no DB row yet.
    pub worktree_path: Option<String>,
    /// A stable key for pinning a row (workspace slug, or `slug/branch`).
    pub pin_key: String,
    /// The worktree's branch (Worktree rows) — the seed for the rename prompt
    /// and the base of the "branch from this" action.
    pub branch: Option<String>,
    pub git: Option<GitGlyphs>,
    pub sandbox_backend: Option<String>,
    /// Selected execution environment (`[env.<name>]`); `None`/`"default"` ⇒
    /// the implicit default (no badge shown).
    pub env_name: Option<String>,
    /// The env is a managed provider but content resolved local (degraded to the
    /// host) — renders the `«env»` badge as `«env ✗»`.
    pub env_degraded: bool,
    pub activity: ActivityState,
    /// Render/navigation visibility: false when hidden by a collapsed parent or
    /// filtered out.
    pub visible: bool,
    /// For Workspace rows: whether its subtree is collapsed (drives the caret).
    pub collapsed: bool,
    /// For Workspace rows: a non-git "dir" workspace (drives a distinct glyph).
    pub dir: bool,
    /// Open PR count for this worktree's branch — the cursor row's detail-line
    /// PR badge (item 28).
    pub pr_count: Option<usize>,
    /// Lowest open PR number for this worktree's branch, used to compose the
    /// dynamic row title (`[PR: <n> | …]`). `None` when no open PR is cached.
    pub pr_number: Option<u64>,
    /// Unread notification count — the detail line's ✉ badge (item 28).
    pub unread_count: usize,
    /// Alert count (test/agent failures, log errors) — the main row's always-on
    /// red ⚠ badge (item 28).
    pub alert_count: usize,
    /// Disk usage of this worktree's checkout (bytes), from the off-loop scan.
    pub disk_bytes: Option<u64>,
    /// Disk usage of this worktree's `target/` subtree (bytes) — the reclaimable
    /// portion. Drives the amber tint on the size badge when it dominates.
    pub target_bytes: Option<u64>,
    /// Connection string for terminal rows
    pub terminal_connection: Option<String>,
    /// For Folder rows: the DB `folders.folder_id` — the key for folder
    /// actions (rename/delete) without re-parsing `pin_key`.
    pub folder_id: Option<i64>,
    /// For Folder rows: how many worktrees are filed inside (drives the
    /// rendered "(N)" count; the label itself stays the bare folder name so
    /// actions can seed rename prompts from it).
    pub child_count: usize,
    /// Attention score: the worktree's own (Worktree rows) or the workspace's
    /// most-urgent-child rollup (Workspace rows). Denormalized from
    /// `SidebarStatus` in one pass at build time. Feeds sorting/bubbling only
    /// — no glyph is painted from it (the ✋ lives on the statusbar and the
    /// "Needs you" popup).
    pub attention: Option<thegn_core::attention::AttentionScore>,
    /// The worktree's merge-queue status (its `merge_queue` row, if any) —
    /// drives the detail line's MQ chip. Denormalized in the same pass.
    pub mq_status: Option<thegn_core::attention::MqStatus>,
    /// The pipeline stage this worktree's live agent is working (`architect`,
    /// `code`, `review`, …), denormalized from `SidebarStatus::pipeline_stages`
    /// in the same pass as `mq_status`.
    ///
    /// **Evidence, never state.** It paints a short tag beside the activity dot;
    /// the `ActivityState` FSM is untouched, and a `waiting_human` stage row
    /// reaches the dot through the EXISTING blocked evidence (the attention
    /// tier), not through a stage-shaped activity variant.
    pub pipeline_stage: Option<String>,
    /// Flat-layout only: the owning workspace's display name, rendered as a dim
    /// prefix so a flat cross-repo worktree row still shows which repo it
    /// belongs to. `None` in grouped mode (the workspace header gives context).
    pub repo_prefix: Option<String>,
    /// [`RowKind::PipelineSummary`] rows only: the counts the row renders.
    /// `None` everywhere else.
    pub pipeline: Option<PipelineSummary>,
}

impl SidebarRow {
    /// A bare row of `kind` at `depth` with every target/status/badge field
    /// defaulted (visible, not collapsed, no pin key). Construction sites layer
    /// only what the row actually carries via struct-update syntax, so adding a
    /// field to `SidebarRow` touches one place.
    pub fn base(
        kind: RowKind,
        depth: u8,
        label: impl Into<String>,
        workspace_slug: impl Into<String>,
    ) -> Self {
        SidebarRow {
            kind,
            depth,
            label: label.into(),
            workspace_slug: workspace_slug.into(),
            tab_target: None,
            active: false,
            worktree_path: None,
            pin_key: String::new(),
            branch: None,
            git: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
            activity: ActivityState::None,
            visible: true,
            collapsed: false,
            dir: false,
            pr_count: None,
            pr_number: None,
            unread_count: 0,
            alert_count: 0,
            disk_bytes: None,
            target_bytes: None,
            terminal_connection: None,
            folder_id: None,
            child_count: 0,
            attention: None,
            mq_status: None,
            pipeline_stage: None,
            repo_prefix: None,
            pipeline: None,
        }
    }

    /// Whether this row can join the multi-select set: a workspace or worktree
    /// with a stable identity. Excludes section headings / empty hints (no
    /// `pin_key`) and folders / terminals (no bulk or reorder action).
    pub fn is_markable(&self) -> bool {
        !self.pin_key.is_empty() && matches!(self.kind, RowKind::Workspace | RowKind::Worktree)
    }

    /// Whether this row can be pinned (floated to the top of its sibling run).
    /// Everything with a `pin_key` except the derived pipeline rows: a group
    /// and a lane carry a key purely so their collapse state has somewhere to
    /// live, and floating a row the roster invented would reorder a tree the
    /// user never arranged (the worktree mirror's key is anchor-only; see
    /// `RowKind::PipelineWorktree`).
    pub fn is_pinnable(&self) -> bool {
        !self.pin_key.is_empty()
            && !matches!(
                self.kind,
                RowKind::PipelineGroup | RowKind::PipelineLane | RowKind::PipelineWorktree
            )
    }

    /// The `ViewState::collapsed` key for this row's group. 📂 Folder groups
    /// collapse independently of their workspace, so they key on `pin_key`
    /// (`{slug}/folder:{id}`); the derived pipeline group/lane rows do the
    /// same (`pipeline/group:{slug}` / `pipeline/lane:{key}`) — lanes share
    /// one `workspace_slug` with their group, so keying on it would collapse
    /// every lane at once. Every other collapsible row (Workspace,
    /// TerminalHost) keys on its `workspace_slug`. Meaningless for leaves.
    pub fn collapse_key(&self) -> &str {
        match self.kind {
            RowKind::Folder | RowKind::PipelineGroup | RowKind::PipelineLane => &self.pin_key,
            _ => &self.workspace_slug,
        }
    }
}

/// Per-worktree status sourced from the (possibly slow) git/activity scan on
/// the hydration thread, merged onto rows at build time. `git`/`agent` are
/// keyed by worktree path; `activity` by tab name (matching the `activity`
/// state machine's TSV keys).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SidebarStatus {
    pub git: std::collections::BTreeMap<String, GitGlyphs>,
    /// The worktree's LIVE `HEAD` branch, keyed by path — read by the same git
    /// scan that produces `git`. The row's `{slug}/{branch}` tab name is only a
    /// creation-time identity (it never moves when you `git checkout` inside the
    /// worktree), so the displayed branch must come from here or it goes stale
    /// forever. Absent for a detached HEAD or a worktree never scanned.
    pub branches: std::collections::BTreeMap<String, String>,
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
    /// Per-worktree size-cache measurement timestamp (unix seconds), keyed by
    /// path — the age the monitor's Disk tab shows so a stale row reads as stale
    /// rather than pretending to be current. Same source as `disk_sizes`.
    pub disk_stamps: std::collections::HashMap<String, i64>,
    /// Worktrees mid-hibernation (snapshot taken, compute destroyed or being
    /// destroyed). In the status so its diff repaints the sidebar; the render
    /// path reads the mirroring `hibernator::is_hibernated` cache.
    pub hibernated: std::collections::BTreeSet<String>,
    /// Per-worktree attention score (keyed by path) — the tiered "what needs
    /// the user" model (see `thegn_core::attention`). Drives the Attention
    /// sort ranks, the jump/ring actions, the statusbar ✋ badge, and the
    /// "Needs you" popup. (Rows themselves paint no attention glyph — the
    /// denormalized `SidebarRow::attention` feeds sorting only.)
    pub attention: std::collections::BTreeMap<String, thegn_core::attention::AttentionScore>,
    /// Hysteresis-stable display rank per worktree path (0 = most urgent).
    /// Computed on the hydration thread; only a tier or membership change
    /// reorders, so timestamp/cache churn never reshuffles rows.
    pub attention_ranks: std::collections::BTreeMap<String, u32>,
    /// Per-worktree last-activity time (unix seconds, keyed by path, bucketed
    /// to 2s), from the activity FSM snapshot — computed off-loop on the
    /// hydration thread. Drives `SortMode::Live` (most-recently-active first).
    /// Absent for worktrees the FSM has never seen active. In `PartialEq`, so a
    /// recency change participates in the status diff that gates repaints.
    pub activity_recency: std::collections::BTreeMap<String, f64>,
    /// Per-workspace rollup (keyed by slug): the most urgent worktree's score.
    /// Drives workspace bubbling (`[ui] sidebar_workspace_sort = "attention"`);
    /// no per-workspace glyph is painted from it.
    pub workspace_attention:
        std::collections::BTreeMap<String, thegn_core::attention::AttentionScore>,
    /// Per-worktree merge-queue status (keyed by path) — the queue rows the
    /// attention scan already reads, re-exposed for the sidebar's MQ chip.
    pub mq: std::collections::BTreeMap<String, thegn_core::attention::MqStatus>,
    /// Per-worktree pipeline stage (keyed by path): the stage name of that
    /// worktree's most recent **active** agent-dispatch row. Evidence, not
    /// state — it drives a short tag beside the activity dot and nothing else.
    /// Absent for a worktree with no live staged dispatch (see
    /// `monitor_pipeline::stage_badges`).
    pub pipeline_stages: std::collections::BTreeMap<String, String>,
    /// Whole-roster rollup (live rows, and the human-parked subset) behind the
    /// sidebar's compact Pipeline row. Same off-loop roster read as
    /// `pipeline_stages`; zeroed when nothing is dispatched, which is what
    /// hides the row.
    pub pipeline: PipelineSummary,
    /// The derived lane folders under the Pipeline row: one per issue/worktree
    /// with **active** dispatch rows, each carrying its agents (see
    /// [`crate::sidebar_pipeline::lanes`]). A fourth pure fold over the same
    /// off-loop roster read — no DB open, no wake source. Empty ⇒ no lane rows,
    /// which is how a finished lane disappears with nothing to reap.
    pub pipeline_lanes: Vec<crate::sidebar_pipeline::Lane>,
    /// Worktree paths whose current attention signal the user has acknowledged
    /// (see `attention::AttentionScore::is_acked_by`). Suppressed from the nag
    /// surfaces — the `✋` badge count and the "Needs you" popup / jump ring —
    /// while acked; the sidebar sort still reflects the true tier.
    pub acked: std::collections::BTreeSet<String>,
    /// The worktree paths belonging to the **active worktree's repo** — the
    /// default scope of the nag surfaces (`✋` badge, "Needs you" popup, `Alt a`
    /// ring), mirroring how the notification inbox already scopes itself. Signals
    /// outside it are rolled up rather than counted, and the System-tab "all"
    /// toggle (`panel::scope::system_all`) reveals them in full.
    ///
    /// `None` when no active repo resolved: **fail open** and scope nothing, so a
    /// scoping bug can never hide something that needs the user. `attention` and
    /// `workspace_attention` above stay global regardless — every workspace's
    /// sidebar row still shows its own tier.
    pub repo_scope: Option<std::collections::BTreeSet<String>>,
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
    pub workspace_sort: thegn_core::config::WorkspaceSort,
    /// TERMINALS section visibility, from `[ui] sidebar_terminals_section`
    /// (config, mirrored like `workspace_sort`). `NonEmpty` hides the banner
    /// and its hint until a terminal exists.
    pub terminals_section: thegn_core::config::TerminalsSection,
    /// Flat cross-workspace layout (`g`): drop per-repo grouping and show one
    /// recency-ordered list of every worktree, each tagged with its repo.
    /// Persisted as the `sidebar_flat` ui_state key; independent of `sort`.
    pub flat: bool,
    /// Worktree-row display options, from `[ui]` config (mirrored here on
    /// startup/reload like `workspace_sort`). Which right-cluster fields show,
    /// the focused-detail policy, and glyph overrides.
    pub display: crate::sidebar_view::SidebarDisplay,
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
    /// The env is a managed PROVIDER but the worktree's content resolved local
    /// (degraded to the host / never provisioned) — renders the `«env»` badge as
    /// `«env ✗»`. Computed in [`crate::hydrate::db_worktree_list`].
    pub env_degraded: bool,
}

/// Split a `{repo}/{branch}` group name into its parts.
pub fn split_tab(name: &str) -> Option<(String, String)> {
    let (repo, branch) = name.split_once('/')?;
    (!repo.is_empty()).then(|| (repo.to_string(), branch.to_string()))
}

/// The `ViewState::collapsed` keys that hide the active worktree named
/// `active_name` (a `{slug}/{branch}` group name). Always its workspace slug;
/// plus its folder key (`{slug}/folder:{id}`) when a matching `DbWorktree` (by
/// `tab_name`) is filed to a folder. Empty when `active_name` isn't a worktree
/// (e.g. a terminal group name that doesn't split), so the caller reveals
/// nothing. Pure over the DB carrier so it's unit-testable without a `Session`.
pub fn active_reveal_keys(active_name: &str, db_worktrees: &[DbWorktree]) -> Vec<String> {
    let Some((slug, _branch)) = split_tab(active_name) else {
        return Vec::new();
    };
    let mut keys = vec![slug.clone()];
    if let Some(fid) = db_worktrees
        .iter()
        .find(|w| w.tab_name == active_name)
        .and_then(|w| w.folder_id)
    {
        keys.push(format!("{slug}/folder:{fid}"));
    }
    keys
}

/// Index (into the same `visible_rows` slice) of the nearest collapsible
/// **ancestor** of the cursor row — the first earlier row with a smaller depth
/// whose kind is collapsible. Drives "collapse key on a sub-item collapses its
/// parent": a filed worktree → its Folder, a loose worktree → its Workspace, a
/// terminal → its TerminalHost, a folder → its Workspace. `None` at the top of
/// the tree (no collapsible ancestor). Pure over the visible-row slice.
pub fn parent_collapsible_index(visible_rows: &[&SidebarRow], cursor: usize) -> Option<usize> {
    let depth = visible_rows.get(cursor)?.depth;
    visible_rows[..cursor]
        .iter()
        .enumerate()
        .rev()
        .find(|(_, r)| r.depth < depth && r.kind.is_collapsible())
        .map(|(i, _)| i)
}

/// Strip a single trailing shell-prompt sigil (`$` `%` `#` `>`) and surrounding
/// whitespace from an OSC window title. zsh + starship and friends often append
/// "… $" to the terminal title, which we don't want bleeding into the sidebar.
fn strip_prompt_sigil(title: &str) -> String {
    let t = title.trim();
    let t = t.strip_suffix(['$', '%', '#', '>']).unwrap_or(t);
    t.trim_end().to_string()
}

/// Compose a worktree row's displayed title: the dynamic name (the OSC window
/// title a running agent/shell sets, sigil-stripped) when present, else the
/// branch. PR state is no longer wrapped into the title — it shows as a compact
/// `⬡N` chip in the row's right cluster and `PR #N` on the focused detail line.
pub fn compose_row_label(window_title: Option<&str>, branch: &str) -> String {
    window_title
        .map(strip_prompt_sigil)
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| branch.to_string())
}

/// The branch a worktree row shows on its MAIN line when no OSC window title is
/// set: the live `HEAD` ([`SidebarRow::branch`], refreshed by the git scan) so a
/// `git checkout` inside the worktree is reflected, falling back to the
/// creation-time tab name ([`SidebarRow::label`]) before the first scan lands.
///
/// The one exception is the **home** row, which is named `home` — not after a
/// branch — in every workspace. Substituting its live branch there would rename
/// it to `main` and break the identity users navigate by; its actual branch
/// still shows on the detail line. Pure, so the exception is unit-tested.
pub fn row_display_branch(row: &SidebarRow) -> &str {
    if row.label == "home" {
        return &row.label;
    }
    row.branch
        .as_deref()
        .filter(|b| !b.is_empty())
        .unwrap_or(&row.label)
}

/// A workspace's worktree, ready to sort and render: the branch label plus its
/// sort key, status, and what activating its row does. Built from either a live
/// session group or a dormant DB row, so the tree renders identically whether
/// or not the workspace is the one currently loaded in the session.
#[derive(Debug, Clone)]
struct Group {
    label: String,
    /// Sort tie-break within a tier: the live session slot for a loaded
    /// workspace, or the DB position (per-slug enumeration index) for a
    /// dormant one.
    gi: usize,
    /// Worktree path — the key into the attention-rank map for the
    /// [`SortMode::Attention`] ordering (and into the per-worktree status maps).
    path: String,
    activity: ActivityState,
    sandbox_backend: Option<String>,
    env_name: Option<String>,
    /// The env is a provider but content is local (degraded) — drives the `✗` on
    /// the env badge. See [`DbWorktree::env_degraded`].
    env_degraded: bool,
    folder_id: Option<i64>,
    /// What activating this group's row does: focus the live `(gi, tab)` for a
    /// loaded workspace, or switch to the workspace (landing on this group's
    /// `{slug}/{branch}` name) for a dormant one.
    target: RowTarget,
    /// Whether this is the session's active worktree (always `false` when
    /// dormant — an unloaded workspace has no active pointer of its own).
    active: bool,
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

/// The focused row's second line: the branch name plus the secondary metadata
/// that would crowd the always-on row — branch-vs-local-default-branch line stat,
/// execution env, sandbox backend, hibernation, open PRs, unread notifications,
/// and disk size. Field visibility follows the `[ui]` sidebar toggles carried on
/// `disp`. `None` when the row has nothing to show. (Extracted from the
/// ratchet-pinned `chrome.rs`.)
pub(crate) fn compose_detail_line(
    row: &SidebarRow,
    disp: &crate::sidebar_view::SidebarDisplay,
) -> Option<crate::seg::Line> {
    use crate::chrome::S;
    use crate::seg::{Line, Seg, Tok, seg, sp};
    use thegn_core::theme;
    // Gutter + indent so the detail reads as hanging under the name.
    let mut segs: Vec<Seg> = vec![sp(5)];
    let start = segs.len();
    // Lead with the branch name — the main line now shows the dynamic name.
    // `row.branch` is the LIVE `HEAD` (it tracks a `git checkout`); `row.label`
    // is the creation-time tab name and is only the fallback.
    let branch = row
        .branch
        .as_deref()
        .filter(|b| !b.is_empty())
        .unwrap_or(&row.label);
    if disp.detail_branch && !branch.is_empty() {
        segs.push(seg(Tok::Slot(S::Dim), format!("{branch} ")));
    }
    // This branch's own work over the repo's LOCAL default branch (`+adds` green
    // / `-dels` red) — the same base `thegn diff` and the diff viewer use, so a
    // row on the trunk (or freshly branched off it) correctly shows nothing.
    if disp.detail_branch_stat
        && let Some((add, del)) = row.git.and_then(|g| g.branch_diff)
        && (add > 0 || del > 0)
    {
        if add > 0 {
            segs.push(seg(Tok::Hue(theme::Hue::Green), format!("+{add}")));
        }
        if del > 0 {
            if add > 0 {
                segs.push(sp(1));
            }
            segs.push(seg(Tok::Hue(theme::Hue::Red), format!("-{del}")));
        }
        segs.push(sp(1));
    }
    let dirty = row.git.is_some_and(|g| g.dirty);
    crate::sidebar_legend::push_row_markers(dirty, &mut segs);

    if let Some(env) = &row.env_name
        && !env.is_empty()
        && env != "default"
    {
        let gl = crate::caps::active_glyphs();
        // Degraded provider pin: mark the badge (`«env ✗»`, amber) so it doesn't
        // imply the pane is on the provider when it's really on the host. ASCII
        // fallback swaps `✗`→`x` and `«»`→`<>` via the caps glyph set.
        if row.env_degraded {
            segs.push(seg(
                Tok::Hue(theme::Hue::Amber),
                format!("{}{env} {}{} ", gl.quote_open, gl.cross, gl.quote_close),
            ));
        } else {
            segs.push(seg(
                Tok::Slot(S::Faint),
                format!("{}{env}{} ", gl.quote_open, gl.quote_close),
            ));
        }
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
    if disp.detail_pr {
        let hex = crate::caps::active_glyphs().hex;
        // Prefer the concrete PR number (`⬡ PR #123`); fall back to the count
        // (`⬡ 2 PR`) when only a count is known for this branch.
        if let Some(n) = row.pr_number {
            segs.push(seg(Tok::Hue(theme::Hue::Green), format!("{hex} PR #{n} ")));
        } else if let Some(pr) = row.pr_count.filter(|&c| c > 0) {
            segs.push(seg(Tok::Hue(theme::Hue::Green), format!("{hex} {pr} PR ")));
        }
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
        segs.push(seg(fg, thegn_core::disk::human(total)));
    }

    (segs.len() > start).then_some(Line::Segs(segs))
}

/// The detail line's merge-queue chip for a status: the shared
/// [`thegn_core::attention::MqStatus::glyph`] vocabulary (also used by
/// `panel/sections/merge_queue.rs`), minus `Landed` — a finished row is panel
/// detail, not sidebar-worthy signal.
fn mq_chip(mq: thegn_core::attention::MqStatus) -> Option<(&'static str, thegn_core::theme::Hue)> {
    if mq == thegn_core::attention::MqStatus::Landed {
        return None;
    }
    Some(mq.glyph(crate::caps::active_glyphs()))
}

/// Group `db_terminals` into host sections in sidebar **display order**:
/// `local` first, then remote hosts by case-insensitive label. Each entry is
/// `(collapse-key, display-label, is_local, terminals-in-order)`. The
/// collapse-key is the bare host key; the row slug is `terminals/host:{key}`.
/// Shared by the tree builder ([`build_rows`]) and the host/leaf region
/// navigation in `run.rs`, so both see exactly one ordering.
pub fn terminal_hosts_ordered(
    db_terminals: &[thegn_core::models::TerminalRow],
) -> Vec<(String, String, bool, Vec<&thegn_core::models::TerminalRow>)> {
    let mut host_order: Vec<(String, String, bool)> = Vec::new();
    let mut groups: std::collections::HashMap<String, Vec<&thegn_core::models::TerminalRow>> =
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
    db_folders: &[thegn_core::models::FolderRow],
    db_terminals: &[thegn_core::models::TerminalRow],
) -> Vec<SidebarRow> {
    let activity = &status.activity;
    let mut rows = Vec::new();

    // Workspace bubbling (`[ui] sidebar_workspace_sort = "attention"`): order
    // workspaces by their most-urgent worktree's tier. The sort is stable and
    // tier-granular, so equal-urgency workspaces keep their manual order and a
    // workspace only moves on a real tier change — hysteresis for free.
    let mut workspaces: Vec<&(String, String, String, String)> = workspaces.iter().collect();
    if view.workspace_sort == thegn_core::config::WorkspaceSort::Attention {
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

    // Flat cross-workspace layout: emit one recency-ordered list of every
    // worktree (each tagged with its repo) under a single banner, and skip the
    // per-workspace grouped loop below. The shared tail (TERMINALS, disk /
    // attention denormalize, pins, filter) still runs for both layouts.
    if view.flat {
        build_rows_flat(
            &mut rows,
            session,
            &workspaces,
            view,
            status,
            &db_by_tab,
            &db_by_slug,
        );
    }

    // The derived pipeline folders (THE-74): one `Pipelines` group per
    // workspace, one lane folder per pipeline — named from the roster's
    // issue id — holding every worktree that pipeline's roster rows
    // reference, whatever their status. Resolution of a lane's worktree to
    // (workspace, jump target) uses the same sources the primary rows do —
    // live session slots first, then the DB-registered rows — so a leaf
    // lands on exactly the primary row's door. A lane files under the
    // workspace owning its FIRST resolvable worktree; a lane with no
    // resolvable worktree (dispatch rows recorded for paths thegn has no
    // registration for) falls back to the tail group under the board door,
    // so a referenced worktree is never silently dropped. In the flat
    // layout there are no workspace rows to nest under, so every lane rides
    // the tail group there.
    let mut lane_targets: std::collections::HashMap<&str, (String, RowTarget)> =
        std::collections::HashMap::new();
    for (gi, g) in session.worktrees.iter().enumerate() {
        if let Some((repo, _)) = split_tab(&g.name) {
            lane_targets
                .entry(g.path.as_str())
                .or_insert_with(|| (repo, RowTarget::Tab(gi, g.active_tab)));
        }
    }
    for w in db_worktrees {
        lane_targets.entry(w.path.as_str()).or_insert_with(|| {
            (
                w.slug.clone(),
                RowTarget::Workspace {
                    repo_path: w.repo_path.clone(),
                    group: Some(w.tab_name.clone()),
                },
            )
        });
    }
    let mut lanes_by_ws: std::collections::BTreeMap<String, Vec<&crate::sidebar_pipeline::Lane>> =
        std::collections::BTreeMap::new();
    let mut unfiled_lanes: Vec<&crate::sidebar_pipeline::Lane> = Vec::new();
    for lane in &status.pipeline_lanes {
        let home = if view.flat {
            None
        } else {
            lane.worktrees
                .iter()
                .find_map(|w| lane_targets.get(w.path.as_str()))
                .map(|(slug, _)| slug.clone())
        };
        match home {
            Some(slug) => lanes_by_ws.entry(slug).or_default().push(lane),
            None => unfiled_lanes.push(lane),
        }
    }

    for (repo_slug, display, kind, repo_path) in if view.flat { Vec::new() } else { workspaces } {
        let collapsed = view.collapsed.contains(repo_slug);
        rows.push(SidebarRow {
            // Workspace rows carry the repo path (not a worktree path) so the
            // remove-workspace action can resolve its DB target without a
            // slug→path lookup. Empty for live fallbacks with no DB row yet.
            worktree_path: (!repo_path.is_empty()).then(|| repo_path.clone()),
            pin_key: repo_slug.clone(),
            collapsed,
            dir: kind == "dir",
            ..SidebarRow::base(RowKind::Workspace, 0, display.clone(), repo_slug.clone())
        });

        // This repo's worktree groups — live from the session model, else
        // reconstructed from the DB for a dormant workspace (see
        // `gather_groups`). Both flow through one shared sort + render below.
        let mut groups = gather_groups(
            session,
            repo_slug,
            repo_path,
            activity,
            &db_by_tab,
            &db_by_slug,
        );

        sort_groups(
            &mut groups,
            view.sort,
            &status.attention_ranks,
            &status.activity_recency,
        );

        // One shared worktree-row builder for both the loose (depth 1) and
        // filed (depth 2) placements, and for both live and dormant sources —
        // everything placement-specific (`depth`, `pin_key`) is an argument;
        // everything source-specific (`target`, `active`, `path`) rides on the
        // `Group`. This is the single render routine that keeps a dormant
        // workspace's tree identical to its live one.
        let mk_row = |gr: &Group, depth: u8, pin_key: String| -> SidebarRow {
            worktree_row(gr, status, repo_slug, depth, pin_key, !collapsed, None)
        };

        // Folders section: home → loose → folders by `position`. Filed
        // worktrees render at depth 2 under their folder header, in the order
        // the user arranged them: this emission order defines the **sibling
        // runs** that `crate::sidebar_order` reorders within, so the loose list
        // and each folder are independent runs and a manual move never swaps a
        // filed worktree with a loose one. Computed BEFORE the loose pass so it
        // can see which folders will actually get a header.
        let mut workspace_folders: Vec<&thegn_core::models::FolderRow> = db_folders
            .iter()
            .filter(|f| f.repo_path == *repo_path)
            .collect();
        workspace_folders.sort_by_key(|f| f.position);
        // The folder ids that render a header for this workspace. A worktree
        // whose `folder_id` points *outside* this set has nowhere to nest —
        // e.g. a merge-queue `file_into` that recorded the folder under a
        // `repo_path` string that doesn't byte-match this workspace's, or a
        // stale id — so it must fall back to the loose list below, or it would
        // render neither loose (it has a `folder_id`) nor filed (no header
        // matches) and silently vanish from the tree.
        let rendered_folder_ids: std::collections::HashSet<i64> =
            workspace_folders.iter().map(|f| f.folder_id).collect();

        // Split into unfiled (rendered at root) and filed (rendered under
        // folders). Unfiled keeps the sorted home-first order; filed worktrees
        // are emitted later under their folder header at depth 2. A worktree
        // whose `folder_id` has no matching header falls back to loose so it is
        // never invisible.
        for gr in groups.iter().filter(|g| {
            g.folder_id
                .is_none_or(|fid| !rendered_folder_ids.contains(&fid))
        }) {
            let pin_key = format!("{repo_slug}/{}", gr.label);
            rows.push(mk_row(gr, 1, pin_key));
        }

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
                pin_key: folder_key.clone(),
                visible: !collapsed,
                collapsed: folder_collapsed,
                folder_id: Some(folder.folder_id),
                child_count,
                ..SidebarRow::base(RowKind::Folder, 1, folder.name.clone(), repo_slug.clone())
            });

            // Filed children in the workspace's sort order: iterate the
            // already-sorted `groups` filtered to this folder, so the same
            // `view.sort` that ordered the loose rows orders these too. Always
            // emit the rows (mirroring how a collapsed *workspace* still emits
            // children with `visible:false`), only toggling visibility on the
            // folder-collapsed state — so the sidebar filter can find and reveal
            // a worktree filed into a collapsed folder, just as it can one under
            // a collapsed workspace.
            for gr in groups
                .iter()
                .filter(|g| g.folder_id == Some(folder.folder_id))
            {
                // Same `{slug}/{label}` key as the loose and flat placements —
                // the key is the row's IDENTITY (pins, marks, menu targets,
                // drag anchors), and embedding the folder id re-keyed the row
                // on every file/unfile/flat-toggle, silently dropping its pin
                // and marks (twice, when the optimistic negative folder id was
                // swapped for the real one). Labels are unique per slug, so
                // the placement suffix bought no uniqueness.
                let pin_key = format!("{repo_slug}/{}", gr.label);
                let mut row = mk_row(gr, 2, pin_key);
                row.visible = !collapsed && !folder_collapsed;
                rows.push(row);
            }
        }

        // The workspace's derived `Pipelines` folder (THE-74), at the tail
        // of its own tree: a lane lives under the workspace that owns its
        // worktrees, so a pipeline's spawns read as part of that repo — and
        // a worktree no roster row references stays exactly where it was.
        // Children are always emitted with `visible` tracking collapsed
        // ancestors (the folder precedent), so the filter can reveal a row
        // inside a collapsed group.
        if let Some(ws_lanes) = lanes_by_ws.get(repo_slug) {
            push_pipeline_group(
                &mut rows,
                ws_lanes,
                &format!("pipeline/group:{repo_slug}"),
                repo_slug,
                collapsed,
                view,
                &lane_targets,
            );
        }
    }

    if rows.is_empty() {
        rows.push(SidebarRow::base(RowKind::Workspace, 0, "no workspaces", ""));
    }

    // One compact roster rollup, only while agents are actually running. Placed
    // at the TAIL (just above the TERMINALS banner) rather than the head: the
    // row appears and vanishes with the roster, and the sidebar cursor is a
    // visible-row INDEX, so a head placement would shunt the cursor off the
    // workspace row under it every time an agent started or finished.
    if status.pipeline.active > 0 {
        rows.push(SidebarRow {
            pipeline: Some(status.pipeline),
            ..SidebarRow::base(RowKind::PipelineSummary, 0, "Pipeline", "pipeline")
        });
    }

    // Lanes thegn could not attribute to a workspace — or, in the flat
    // layout, every lane — group under the same `Pipelines` name at the
    // tail, under the board door, so a referenced worktree is never dropped
    // from the tree. Same tail placement as the door row: these rows appear
    // and vanish with the roster, and the cursor is a visible-row index.
    push_pipeline_group(
        &mut rows,
        &unfiled_lanes,
        "pipeline/group:unfiled",
        "pipeline",
        false,
        view,
        &lane_targets,
    );

    // TERMINALS is a first-class, static category banner (a peer of the
    // "WORKSPACES" title), never collapsible and never a nav target. By
    // default it is always shown — even with no terminals — so the section
    // (and its "New terminal…" entry point) never silently vanishes;
    // `[ui] sidebar_terminals_section = "nonempty"` opts into hiding the
    // whole section until a terminal exists.
    let hide_terminals = db_terminals.is_empty()
        && view.terminals_section == thegn_core::config::TerminalsSection::NonEmpty;
    if !hide_terminals {
        rows.push(SidebarRow::base(
            RowKind::SectionHeading,
            0,
            "TERMINALS",
            "terminals",
        ));

        // Genuinely-empty fallback (the startup reseed normally keeps a `local`
        // terminal, so this shows only when that couldn't run): a passive,
        // non-interactive hint pointing at the add flow.
        if db_terminals.is_empty() {
            rows.push(SidebarRow::base(
                RowKind::EmptyHint,
                1,
                "No terminals — Enter to add",
                "terminals",
            ));
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
                collapsed,
                terminal_connection: Some(rep_conn),
                ..SidebarRow::base(RowKind::TerminalHost, 1, label.clone(), slug.clone())
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
                    tab_target: target.or_else(|| {
                        Some(RowTarget::Workspace {
                            repo_path: "terminal".into(),
                            group: Some(t.name.clone()),
                        })
                    }),
                    active,
                    worktree_path: Some(t.name.clone()),
                    pin_key: format!("terminals/{}", t.name),
                    // Show the sandbox backend on the row like a worktree does;
                    // blank/`host` means an un-sandboxed shell (rendered as none).
                    // OBSERVED, not the pick — see `hydrate_terminal::terminal_env`.
                    sandbox_backend: {
                        let b = t.observed_backend.trim();
                        (!b.is_empty() && b != "host" && b != "none").then(|| b.to_string())
                    },
                    env_name: (!t.env_name.trim().is_empty()).then(|| t.env_name.clone()),
                    terminal_connection: Some(t.connection_string.clone()),
                    ..SidebarRow::base(RowKind::Terminal, 2, t.name.clone(), slug.clone())
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
                row.pipeline_stage = row
                    .worktree_path
                    .as_deref()
                    .and_then(|p| status.pipeline_stages.get(p))
                    .cloned();
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

/// Emit one derived `Pipelines` group: the folder head, each lane folder
/// named from the roster's issue id, and each lane's referenced worktrees as
/// mirror leaves.
///
/// Nothing here is state: the rows are a fold of `status.pipeline_lanes`,
/// which is itself a fold of the roster's rows of ANY status — so the folders
/// survive a restart and a finished lane stays until its rows go, with no
/// reaper and no DB write. The worktree leaves mirror the `tab_target` of the
/// primary row for the same path (resolved from the same `Group`/`DbWorktree`
/// sources the primary rows build from), so activating one lands exactly
/// where activating the primary row does; a path with no primary row keeps
/// `tab_target: None` and renders faint rather than vanishing.
///
/// Visibility follows the folder precedent: children are ALWAYS emitted and
/// only their `visible` flag tracks a collapsed ancestor, so the sidebar
/// filter can still find and reveal a row inside a collapsed group or lane.
fn push_pipeline_group(
    rows: &mut Vec<SidebarRow>,
    lanes: &[&crate::sidebar_pipeline::Lane],
    group_key: &str,
    group_slug: &str,
    parent_collapsed: bool,
    view: &ViewState,
    lane_targets: &std::collections::HashMap<&str, (String, RowTarget)>,
) {
    if lanes.is_empty() {
        return;
    }
    let group_collapsed = view.collapsed.contains(group_key);
    rows.push(SidebarRow {
        pin_key: group_key.to_string(),
        collapsed: group_collapsed,
        visible: !parent_collapsed,
        child_count: lanes.len(),
        ..SidebarRow::base(RowKind::PipelineGroup, 1, "Pipelines", group_slug)
    });
    for lane in lanes {
        let lane_key = format!("pipeline/lane:{}", lane.key);
        let lane_collapsed = view.collapsed.contains(&lane_key);
        rows.push(SidebarRow {
            pin_key: lane_key,
            collapsed: lane_collapsed,
            visible: !parent_collapsed && !group_collapsed,
            child_count: lane.worktrees.len(),
            ..SidebarRow::base(RowKind::PipelineLane, 2, lane.label.clone(), group_slug)
        });
        for wt in &lane.worktrees {
            let hit = lane_targets.get(wt.path.as_str());
            rows.push(SidebarRow {
                // A MIRROR: its `pin_key` is its own (lane-scoped, path-qualified)
                // identity, never the primary row's — pins/marks cannot reach it
                // because those paths are kind-gated (`is_markable`,
                // `is_pinnable`, the drag sources). The key exists for the
                // ANCHOR paths that resolve a row by `pin_key`: the context-menu
                // re-anchor (keyboard Enter and the mouse click both
                // `.position(|r| r.pin_key == target)`), double-click detection
                // and the rebuild's cursor re-seek. An empty key there would
                // resolve to the FIRST keyless row — another mirror or the door
                // row — and fire the menu entry at the wrong worktree.
                pin_key: format!("pipeline/lane:{}/wt:{}", lane.key, wt.path),
                visible: !parent_collapsed && !group_collapsed && !lane_collapsed,
                worktree_path: Some(wt.path.clone()),
                tab_target: hit.map(|(_, t)| t.clone()),
                ..SidebarRow::base(
                    RowKind::PipelineWorktree,
                    3,
                    wt.name.clone(),
                    hit.map(|(slug, _)| slug.clone())
                        .unwrap_or_else(|| group_slug.to_string()),
                )
            });
        }
    }
}

/// Build the unsorted worktree `Group` list for one workspace. A *loaded*
/// workspace draws its groups straight from the session model (live `Tab`
/// targets + real active flag); a *dormant* one (parked into the
/// `WorkspacePool` when another workspace became active) has no session slots,
/// so we reconstruct the SAME group list from the DB-registered rows — `home`
/// first, then every registered non-home worktree in position order, each with
/// a `Workspace` switch target. Both the grouped and the flat `build_rows`
/// paths call this, so the tree never rearranges just because a different
/// workspace is active.
fn gather_groups(
    session: &Session,
    repo_slug: &str,
    repo_path: &str,
    activity: &std::collections::BTreeMap<String, ActivityState>,
    db_by_tab: &std::collections::HashMap<&str, &DbWorktree>,
    db_by_slug: &std::collections::HashMap<&str, Vec<&DbWorktree>>,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for (gi, g) in session.worktrees.iter().enumerate() {
        let Some((repo, branch)) = split_tab(&g.name) else {
            continue;
        };
        if repo != repo_slug {
            continue;
        }
        let dbw = db_by_tab.get(g.name.as_str());
        groups.push(Group {
            label: branch,
            gi,
            path: g.path.clone(),
            sandbox_backend: dbw.and_then(|w| w.sandbox_backend.clone()),
            env_name: dbw.and_then(|w| w.env_name.clone()),
            env_degraded: dbw.is_some_and(|w| w.env_degraded),
            activity: activity.get(&g.name).copied().unwrap_or_default(),
            folder_id: dbw.and_then(|w| w.folder_id),
            target: RowTarget::Tab(gi, g.active_tab),
            active: gi == session.active,
        });
    }
    let live = !groups.is_empty();

    // Dormant workspace: synthesize the same shape from the DB. `home` first
    // (pull its folder/backend/env from the `home` DB row when present, else
    // default), then every registered non-home worktree. `gi` is the per-slug
    // enumeration index (DB position order) so sort tie-breaks match the live
    // path.
    if !live && !repo_path.is_empty() {
        let slug_rows = db_by_slug
            .get(repo_slug)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let db_home = slug_rows.iter().find(|w| w.branch == "home");
        groups.push(Group {
            label: "home".into(),
            gi: 0,
            path: repo_path.to_string(),
            sandbox_backend: db_home.and_then(|w| w.sandbox_backend.clone()),
            env_name: db_home.and_then(|w| w.env_name.clone()),
            env_degraded: db_home.is_some_and(|w| w.env_degraded),
            // Keyed by tab name, same source the live rows use — so a
            // workspace you switched away from keeps its activity dot.
            activity: activity
                .get(format!("{repo_slug}/home").as_str())
                .copied()
                .unwrap_or_default(),
            folder_id: db_home.and_then(|w| w.folder_id),
            target: RowTarget::Workspace {
                repo_path: repo_path.to_string(),
                group: Some(format!("{repo_slug}/home")),
            },
            active: false,
        });
        for (i, w) in slug_rows.iter().filter(|w| w.branch != "home").enumerate() {
            groups.push(Group {
                label: w.branch.clone(),
                gi: i + 1,
                path: w.path.clone(),
                sandbox_backend: w.sandbox_backend.clone(),
                env_name: w.env_name.clone(),
                env_degraded: w.env_degraded,
                activity: activity
                    .get(w.tab_name.as_str())
                    .copied()
                    .unwrap_or_default(),
                folder_id: w.folder_id,
                target: RowTarget::Workspace {
                    repo_path: repo_path.to_string(),
                    group: Some(w.tab_name.clone()),
                },
                active: false,
            });
        }
    }
    groups
}

/// Fill a worktree `SidebarRow` from a `Group` + hydrated status. Shared by the
/// grouped (`mk_row`) and flat (`build_rows_flat`) emitters: everything
/// placement-specific (`depth`, `pin_key`, `visible`, `repo_prefix`) is an
/// argument; everything source-specific rides on the `Group`. `repo_prefix` is
/// `Some(display)` only in flat mode, tagging the row with its repo.
fn worktree_row(
    gr: &Group,
    status: &SidebarStatus,
    repo_slug: &str,
    depth: u8,
    pin_key: String,
    visible: bool,
    repo_prefix: Option<String>,
) -> SidebarRow {
    let wt_path = (!gr.path.is_empty()).then(|| gr.path.clone());
    let git = wt_path.as_deref().and_then(|p| status.git.get(p)).copied();
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
    // The DISPLAYED branch is the live `HEAD` from the git scan, not `gr.label`
    // (the `{slug}/{branch}` tab name, which is fixed at creation and never
    // follows a `git checkout` inside the worktree). Falls back to the label
    // when the scan hasn't landed yet / HEAD is detached. `label` itself stays
    // the tab identity — sort order, `pin_key`, filter and the `== "home"`
    // checks all key off it.
    let live_branch = wt_path
        .as_deref()
        .and_then(|p| status.branches.get(p))
        .cloned();
    SidebarRow {
        tab_target: Some(gr.target.clone()),
        active: gr.active,
        worktree_path: wt_path,
        pin_key,
        branch: Some(live_branch.unwrap_or_else(|| gr.label.clone())),
        git,
        sandbox_backend: gr.sandbox_backend.clone(),
        env_name: gr.env_name.clone(),
        env_degraded: gr.env_degraded,
        activity: gr.activity,
        visible,
        pr_count,
        pr_number,
        unread_count,
        alert_count,
        repo_prefix,
        ..SidebarRow::base(
            RowKind::Worktree,
            depth,
            gr.label.clone(),
            repo_slug.to_string(),
        )
    }
}

/// Flat cross-workspace layout: gather every worktree from every workspace into
/// one pool, order it globally by the active `SortMode` (recency for `Live`),
/// and emit one depth-1 row each under a single "WORKTREES" banner, tagged with
/// its repo. Folders and per-workspace collapse are intentionally ignored —
/// flat mode is a single recency-ordered list, so every worktree is visible at
/// depth 1 (its collapse keys stay persisted for the return to grouped view).
fn build_rows_flat(
    rows: &mut Vec<SidebarRow>,
    session: &Session,
    workspaces: &[&(String, String, String, String)],
    view: &ViewState,
    status: &SidebarStatus,
    db_by_tab: &std::collections::HashMap<&str, &DbWorktree>,
    db_by_slug: &std::collections::HashMap<&str, Vec<&DbWorktree>>,
) {
    // A first-class banner, peer of TERMINALS.
    rows.push(SidebarRow::base(
        RowKind::SectionHeading,
        0,
        "WORKTREES",
        "worktrees",
    ));

    let mut pool: Vec<(String, String, Group)> = Vec::new();
    for (slug, display, _kind, repo_path) in workspaces {
        for g in gather_groups(
            session,
            slug,
            repo_path,
            &status.activity,
            db_by_tab,
            db_by_slug,
        ) {
            pool.push((slug.clone(), display.clone(), g));
        }
    }

    sort_groups_flat(
        &mut pool,
        view.sort,
        &status.attention_ranks,
        &status.activity_recency,
    );

    for (slug, display, gr) in &pool {
        // Same loose-worktree pin key as the grouped path, so pins persist
        // across a flat↔grouped toggle.
        let pin_key = format!("{slug}/{}", gr.label);
        rows.push(worktree_row(
            gr,
            status,
            slug,
            1,
            pin_key,
            true,
            Some(display.clone()),
        ));
    }
}

/// Order the flat cross-workspace pool. Like [`sort_groups`] but global — there
/// is no per-repo `home`-first pinning (many homes would clump); ties fall
/// through to the stable pool order (workspace order, home-first within). Rust's
/// stable sort keeps equal-key rows put, so co-active worktrees don't churn.
fn sort_groups_flat(
    pool: &mut [(String, String, Group)],
    sort: SortMode,
    ranks: &std::collections::BTreeMap<String, u32>,
    recency: &std::collections::BTreeMap<String, f64>,
) {
    match sort {
        // Manual: keep the gathered interleave (workspace order, home-first).
        SortMode::Manual => {}
        SortMode::Name => {
            pool.sort_by_key(|x| x.2.label.to_lowercase());
        }
        SortMode::Recent => {
            pool.sort_by_key(|x| std::cmp::Reverse(x.2.gi));
        }
        SortMode::Attention => {
            pool.sort_by(|a, b| {
                let r = |g: &Group| ranks.get(&g.path).copied().unwrap_or(u32::MAX);
                r(&a.2).cmp(&r(&b.2))
            });
        }
        SortMode::Live => {
            pool.sort_by(|a, b| {
                let t = |g: &Group| recency.get(&g.path).copied().unwrap_or(f64::MIN);
                t(&b.2)
                    .partial_cmp(&t(&a.2))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
}

fn sort_groups(
    groups: &mut [Group],
    sort: SortMode,
    ranks: &std::collections::BTreeMap<String, u32>,
    recency: &std::collections::BTreeMap<String, f64>,
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
        SortMode::Live => {
            // Most-recently-active first, by the off-loop FSM timestamp per
            // path (`activity_recency`, 2s-bucketed). Recency is the primary
            // key — a genuinely more-recent feature worktree outranks "home"
            // (the point of the mode); "home" only breaks ties among equal
            // buckets, then the stable session slot breaks the rest so
            // co-active worktrees never churn frame-to-frame. A worktree the
            // FSM never saw active sorts to the end (recency floor).
            groups.sort_by(|a, b| {
                let t = |g: &Group| recency.get(&g.path).copied().unwrap_or(f64::MIN);
                t(b).partial_cmp(&t(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
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
    let mut last_folder: Option<usize> = None;
    let mut last_section: Option<usize> = None;
    let mut last_host: Option<usize> = None;
    // The derived pipeline section: a workspace's `Pipelines` group, then a
    // lane, then its referenced worktrees. (The tail `Pipeline` door row is a
    // board shortcut, not an ancestor — a lane match does not surface it.)
    let mut last_group: Option<usize> = None;
    let mut last_lane: Option<usize> = None;
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => {
                last_workspace = Some(i);
                last_folder = None; // folders don't span workspaces
                last_group = None;
            }
            RowKind::Folder => {
                last_folder = Some(i);
                // A folder that matches on its own label surfaces its parent
                // workspace header, so it never floats orphaned at depth 1.
                if keep[i]
                    && let Some(w) = last_workspace
                {
                    keep[w] = true;
                }
            }
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
                if keep[i] {
                    if let Some(w) = last_workspace {
                        keep[w] = true; // surface the parent repo header
                    }
                    // A filed worktree (depth 2) also surfaces its folder
                    // header so it isn't shown parentless; a loose worktree
                    // (depth 1) has no folder to surface.
                    if rows[i].depth >= 2
                        && let Some(f) = last_folder
                    {
                        keep[f] = true;
                    }
                }
            }
            RowKind::PipelineSummary => {}
            RowKind::PipelineGroup => {
                last_group = Some(i);
                last_lane = None; // lanes don't span groups
                if keep[i]
                    && let Some(w) = last_workspace
                {
                    keep[w] = true; // surface the parent repo header
                }
            }
            RowKind::PipelineLane => {
                last_lane = Some(i);
                if keep[i] {
                    if let Some(g) = last_group {
                        keep[g] = true;
                    }
                    if let Some(w) = last_workspace {
                        keep[w] = true;
                    }
                }
            }
            RowKind::PipelineWorktree => {
                if keep[i] {
                    if let Some(l) = last_lane {
                        keep[l] = true;
                    }
                    if let Some(g) = last_group {
                        keep[g] = true;
                    }
                    if let Some(w) = last_workspace {
                        keep[w] = true;
                    }
                }
            }
            RowKind::EmptyHint => {}
        }
    }
    // Reveal children only for headers/groups that matched on their own label.
    let mut reveal_ws = false; // inside a self-matched workspace
    let mut reveal_folder = false; // inside a self-matched folder
    let mut reveal_section = false; // inside a self-matched TERMINALS banner
    let mut reveal_host = false; // inside a self-matched host group
    let mut reveal_group = false; // inside a self-matched Pipelines group
    let mut reveal_lane = false; // inside a self-matched pipeline lane
    for i in 0..n {
        match rows[i].kind {
            RowKind::Workspace => {
                reveal_ws = self_match[i];
                reveal_folder = false; // folders don't span workspaces
                reveal_group = false;
            }
            RowKind::Folder => {
                // A folder that matched on its own label reveals its children,
                // and (like any surfaced folder) is itself surfaced above.
                reveal_folder = self_match[i] || reveal_ws;
                if reveal_ws {
                    keep[i] = true;
                }
            }
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
                // Loose worktrees (depth 1) follow the workspace's reveal;
                // filed worktrees (depth 2) also reveal when their folder did.
                if reveal_ws || (rows[i].depth >= 2 && reveal_folder) {
                    keep[i] = true;
                }
            }
            // A group that matched on its own label (or a self-matched
            // workspace) reveals its lanes, and each revealed lane reveals its
            // worktrees — the folder rule, one level deeper.
            RowKind::PipelineGroup => {
                reveal_group = self_match[i] || reveal_ws;
                reveal_lane = false;
                if reveal_group {
                    keep[i] = true;
                }
            }
            RowKind::PipelineLane => {
                reveal_lane = self_match[i] || reveal_group;
                if reveal_lane {
                    keep[i] = true;
                }
            }
            RowKind::PipelineWorktree => {
                if reveal_lane {
                    keep[i] = true;
                }
            }
            RowKind::EmptyHint | RowKind::PipelineSummary => {}
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
        assert_eq!(strip_prompt_sigil("thegn dev $"), "thegn dev");
        assert_eq!(strip_prompt_sigil("  build  % "), "build");
        assert_eq!(strip_prompt_sigil("plain title"), "plain title");
        assert_eq!(strip_prompt_sigil("root #"), "root");
        assert_eq!(strip_prompt_sigil(">"), "");
        // Only one trailing sigil is stripped.
        assert_eq!(strip_prompt_sigil("a $$"), "a $");
    }

    #[test]
    fn compose_row_label_prefers_dynamic_name_else_branch() {
        // Window title (dynamic name), sigil-stripped, wins over the branch.
        assert_eq!(
            compose_row_label(Some("thegn dev $"), "feat/x"),
            "thegn dev"
        );
        assert_eq!(
            compose_row_label(Some("cargo build"), "feat/x"),
            "cargo build"
        );
        // No title → branch.
        assert_eq!(compose_row_label(None, "feat/x"), "feat/x");
        // A title that strips to empty → branch fallback (PR is no longer wrapped
        // into the title; it shows as a right-cluster chip + detail line).
        assert_eq!(compose_row_label(Some(" $"), "main"), "main");
        assert_eq!(compose_row_label(Some("   "), "feat/x"), "feat/x");
    }

    fn tab(name: &str, wt: &str) -> WorktreeGroup {
        WorktreeGroup::new(name, GroupKind::Branch, wt)
    }

    #[test]
    fn row_display_branch_prefers_the_live_head() {
        // The regression: `git checkout` inside a worktree moves HEAD but never
        // the `{slug}/{branch}` tab name, so a label-only row went stale forever.
        let mut row = SidebarRow::base(RowKind::Worktree, 1, "tg/old", "app");
        row.branch = Some("tg/new".into());
        assert_eq!(row_display_branch(&row), "tg/new");
        // Before the first scan lands (or on a detached HEAD) the tab name is
        // still the best name we have.
        row.branch = None;
        assert_eq!(row_display_branch(&row), "tg/old");
        row.branch = Some(String::new());
        assert_eq!(row_display_branch(&row), "tg/old");
    }

    #[test]
    fn row_display_branch_keeps_home_named_home() {
        // `home` is an identity, not a branch: substituting its live branch would
        // rename the row to `main` in every workspace.
        let mut row = SidebarRow::base(RowKind::Worktree, 1, "home", "app");
        row.branch = Some("main".into());
        assert_eq!(row_display_branch(&row), "home");
    }

    #[test]
    fn worktree_rows_carry_the_live_branch_not_the_tab_name() {
        let s = session(vec![tab("app/tg-old", "/wt/a")], 0);
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let mut status = no_activity();
        status
            .branches
            .insert("/wt/a".into(), "tg/checked-out-later".into());
        let rows = build_rows(&s, &ws, &ViewState::default(), &status, &[], &[], &[]);
        let row = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some("/wt/a"))
            .expect("worktree row");
        assert_eq!(row.branch.as_deref(), Some("tg/checked-out-later"));
        // `label` stays the tab identity — sort order, `pin_key`, the filter and
        // the `== "home"` checks all key off it.
        assert_eq!(row.label, "tg-old");
        assert_eq!(row.pin_key, "app/tg-old");
    }

    #[test]
    fn worktree_rows_fall_back_to_the_tab_name_before_the_scan_lands() {
        let s = session(vec![tab("app/tg-old", "/wt/a")], 0);
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
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
        let row = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some("/wt/a"))
            .expect("worktree row");
        assert_eq!(row.branch.as_deref(), Some("tg-old"));
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
            env_degraded: false,
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
        let urgent_score = thegn_core::attention::AttentionScore {
            tier: thegn_core::attention::AttentionTier::Blocked,
            sub: 0,
            reason: thegn_core::attention::AttentionReason::AgentNeedsInput,
            since: Some(100),
            episode: 0,
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

        // The default display sort is Manual — an explorer that never
        // reshuffles itself; Attention is one `s`-menu pick away.
        assert_eq!(ViewState::default().sort, SortMode::Manual);
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
    fn sort_mode_migrates_and_roundtrips() {
        // The old persisted "activity" value parses as Attention (the ui_state
        // migration); unknown strings fall back to the (Manual) default; and
        // every mode round-trips through its canonical string (the sort-menu
        // persistence contract).
        assert_eq!(SortMode::from_str("activity"), SortMode::Attention);
        assert_eq!(SortMode::from_str("bogus"), SortMode::default());
        assert_eq!(SortMode::default(), SortMode::Manual);
        // "live" is its own mode — it must not steal "recent" or "activity".
        assert_eq!(SortMode::from_str("live"), SortMode::Live);
        assert_eq!(SortMode::from_str("recent"), SortMode::Recent);
        for m in [
            SortMode::Manual,
            SortMode::Name,
            SortMode::Recent,
            SortMode::Attention,
            SortMode::Live,
        ] {
            assert_eq!(SortMode::from_str(m.as_str()), m, "round-trip {m:?}");
        }
    }

    #[test]
    fn live_sort_orders_by_recency() {
        // The Live sort ranks by the off-loop activity timestamp
        // (`activity_recency`), most-recent first — and a more-recently-active
        // feature worktree outranks "home" (recency is the primary key).
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
        for (p, t) in [
            ("/wt/home", 100.0),
            ("/wt/calm", 50.0),
            ("/wt/urgent", 300.0),
        ] {
            status.activity_recency.insert(p.into(), t);
        }
        let view = ViewState {
            sort: SortMode::Live,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &status, &[], &[], &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, vec!["urgent", "home", "calm"]);
    }

    #[test]
    fn live_sort_without_recency_keeps_manual_order() {
        // No FSM activity yet (empty recency): Live degrades to the manual
        // order — home first, then session order — so a fresh launch never
        // flashes a reshuffle.
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
        let view = ViewState {
            sort: SortMode::Live,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, vec!["home", "zebra", "alpha"]);
    }

    #[test]
    fn live_sort_ties_break_stably_by_session_slot() {
        // Two worktrees in the same recency bucket hold their session-slot
        // order (the anti-churn tie-break), so co-active worktrees don't
        // leapfrog frame-to-frame.
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/first", "/wt/first"),
                tab("app/second", "/wt/second"),
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
        // first and second share a bucket; home is older.
        for (p, t) in [
            ("/wt/home", 10.0),
            ("/wt/first", 200.0),
            ("/wt/second", 200.0),
        ] {
            status.activity_recency.insert(p.into(), t);
        }
        let view = ViewState {
            sort: SortMode::Live,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &status, &[], &[], &[]);
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(labels, vec!["first", "second", "home"]);
    }

    #[test]
    fn flat_layout_interleaves_across_workspaces() {
        // Flat mode drops the per-repo grouping: one banner + every worktree
        // from every workspace, each tagged with its repo, and no Workspace
        // header rows.
        let s = session(
            vec![
                tab("app/home", "/wt/app-home"),
                tab("app/feat", "/wt/app-feat"),
                tab("web/home", "/wt/web-home"),
                tab("web/spike", "/wt/web-spike"),
            ],
            0,
        );
        let ws = vec![
            (
                "app".to_string(),
                "app".to_string(),
                "repo".to_string(),
                String::new(),
            ),
            (
                "web".to_string(),
                "web".to_string(),
                "repo".to_string(),
                String::new(),
            ),
        ];
        let view = ViewState {
            flat: true,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
        // No workspace header rows in flat mode.
        assert_eq!(
            rows.iter().filter(|r| r.kind == RowKind::Workspace).count(),
            0
        );
        // The WORKTREES banner is present.
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::SectionHeading && r.label == "WORKTREES")
        );
        // Every worktree from both repos appears, each with a repo prefix.
        let wt: Vec<&SidebarRow> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .collect();
        assert_eq!(wt.len(), 4, "rows: {rows:?}");
        for r in &wt {
            assert!(
                r.repo_prefix.is_some(),
                "flat worktree row {:?} missing repo prefix",
                r.label
            );
        }
    }

    #[test]
    fn flat_layout_global_recency_order() {
        // In flat + Live, a more-recently-active worktree in one repo outranks
        // one in another — cross-workspace recency ordering.
        let s = session(
            vec![
                tab("app/home", "/wt/app-home"),
                tab("web/home", "/wt/web-home"),
            ],
            0,
        );
        let ws = vec![
            (
                "app".to_string(),
                "app".to_string(),
                "repo".to_string(),
                String::new(),
            ),
            (
                "web".to_string(),
                "web".to_string(),
                "repo".to_string(),
                String::new(),
            ),
        ];
        let mut status = no_activity();
        status.activity_recency.insert("/wt/app-home".into(), 100.0);
        status.activity_recency.insert("/wt/web-home".into(), 500.0);
        let view = ViewState {
            flat: true,
            sort: SortMode::Live,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &status, &[], &[], &[]);
        let first = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree)
            .expect("a worktree row");
        // web's worktree (more recent) leads, ahead of app's — cross-repo.
        assert_eq!(first.repo_prefix.as_deref(), Some("web"), "rows: {rows:?}");
    }

    #[test]
    fn flat_layout_suppresses_folders() {
        // A filed worktree still appears (at depth 1) in flat mode, but no
        // Folder header rows are emitted — flat is a single flat list.
        let (s, ws, dbw, folders) = folder_fixture();
        let view = ViewState {
            flat: true,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &folders, &[]);
        assert_eq!(
            rows.iter().filter(|r| r.kind == RowKind::Folder).count(),
            0,
            "flat mode emits no folder headers: {rows:?}"
        );
        // The filed worktree is present at depth 1, not nested at depth 2.
        let feat = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.label == "feat")
            .expect("filed worktree still listed");
        assert_eq!(feat.depth, 1);
    }

    #[test]
    fn grouped_mode_has_no_repo_prefix() {
        // Regression guard: the default grouped layout never sets repo_prefix,
        // so the dim `repo/` tag only ever renders in flat mode.
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
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
        assert!(
            rows.iter()
                .filter(|r| r.kind == RowKind::Worktree)
                .all(|r| r.repo_prefix.is_none())
        );
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
                env_degraded: false,
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
                env_degraded: false,
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

    fn term(name: &str, kind: &str, conn: &str) -> thegn_core::models::TerminalRow {
        thegn_core::models::TerminalRow {
            id: 0,
            name: name.into(),
            kind: kind.into(),
            connection_string: conn.into(),
            folder_id: None,
            created_at: 0,
            last_active: 0,
            position: 0,
            sandbox_backend: String::new(),
            observed_backend: String::new(),
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
    fn terminals_section_nonempty_hides_banner_until_a_terminal_exists() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let view = ViewState {
            terminals_section: thegn_core::config::TerminalsSection::NonEmpty,
            ..Default::default()
        };
        // No terminals + "nonempty": the whole section (banner AND hint) is gone.
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
        assert!(
            !rows
                .iter()
                .any(|r| matches!(r.kind, RowKind::SectionHeading | RowKind::EmptyHint)),
            "empty TERMINALS section must vanish under nonempty",
        );
        // A terminal exists: the section is back, without the hint.
        let terms = vec![term("local", "local", "")];
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &terms);
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::SectionHeading && r.label == "TERMINALS")
        );
        assert!(!rows.iter().any(|r| r.kind == RowKind::EmptyHint));
        // The default ("always") keeps banner + hint even when empty.
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
        assert!(rows.iter().any(|r| r.kind == RowKind::SectionHeading));
        assert!(rows.iter().any(|r| r.kind == RowKind::EmptyHint));
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
    fn the_pipeline_row_appears_only_with_live_roster_rows() {
        let s = session(vec![], 0);
        let ws: Vec<(String, String, String, String)> = vec![];
        let view = ViewState::default();

        // Nothing dispatched ⇒ no row at all. The affordance must not be
        // permanent chrome for the (many) users who never run an agent.
        let rows = build_rows(&s, &ws, &view, &no_activity(), &[], &[], &[]);
        assert!(!rows.iter().any(|r| r.kind == RowKind::PipelineSummary));

        let status = SidebarStatus {
            pipeline: PipelineSummary {
                active: 3,
                waiting_human: 1,
            },
            ..no_activity()
        };
        let rows = build_rows(&s, &ws, &view, &status, &[], &[], &[]);
        let ix = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineSummary)
            .expect("a live roster earns the row");
        assert_eq!(rows[ix].label, "Pipeline");
        assert_eq!(rows[ix].pipeline.map(|p| p.active), Some(3));
        // It is a door, never a destination or a bulk-select target.
        assert!(rows[ix].tab_target.is_none());
        assert!(!rows[ix].is_markable());
        assert!(!rows[ix].kind.is_collapsible());
        // Placed at the TAIL — above TERMINALS, below every workspace row — so
        // it appearing/vanishing never shifts the rows the cursor sits on.
        let terminals = rows
            .iter()
            .position(|r| r.kind == RowKind::SectionHeading && r.label == "TERMINALS")
            .expect("terminals banner");
        assert!(ix < terminals);
        assert!(
            !rows[..ix]
                .iter()
                .any(|r| r.kind == RowKind::SectionHeading || r.kind == RowKind::Terminal)
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

    /// A workspace with one loose worktree (`home`) and one worktree (`feat`)
    /// filed under a 📂 folder. Returns `(session, workspaces, db_worktrees,
    /// db_folders)` ready for `build_rows`.
    #[allow(clippy::type_complexity)]
    fn folder_fixture() -> (
        Session,
        Vec<(String, String, String, String)>,
        Vec<DbWorktree>,
        Vec<thegn_core::models::FolderRow>,
    ) {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/feat", "/wt/feat")],
            1,
        );
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let dbw = vec![DbWorktree {
            slug: "app".into(),
            branch: "feat".into(),
            repo_path: "/repos/app".into(),
            tab_name: "app/feat".into(),
            path: "/wt/feat".into(),
            folder_id: Some(1),
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        }];
        let folders = vec![thegn_core::models::FolderRow {
            folder_id: 1,
            repo_path: "/repos/app".into(),
            name: "Backend".into(),
            position: 0,
            created_at: 0,
        }];
        (s, ws, dbw, folders)
    }

    #[test]
    fn folder_collapse_key_is_per_kind_and_hides_filed_children() {
        let (s, ws, dbw, folders) = folder_fixture();

        // Expanded: folder header + its filed child both render.
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &folders,
            &[],
        );
        let folder = rows
            .iter()
            .find(|r| r.kind == RowKind::Folder)
            .expect("folder row present");
        // A folder keys collapse on its own pin_key, NOT the workspace slug.
        assert_eq!(folder.collapse_key(), "app/folder:1");
        assert!(!folder.collapsed);
        let workspace = rows
            .iter()
            .find(|r| r.kind == RowKind::Workspace)
            .expect("workspace row present");
        assert_eq!(workspace.collapse_key(), "app");
        // The filed worktree renders under the folder (depth 2).
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::Worktree && r.label == "feat" && r.depth == 2)
        );

        // Collapse the folder only: its child hides, but the folder row, the
        // workspace, and the loose `home` sibling stay visible.
        let view = ViewState {
            collapsed: ["app/folder:1".to_string()].into_iter().collect(),
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &folders, &[]);
        let folder = rows
            .iter()
            .find(|r| r.kind == RowKind::Folder)
            .expect("folder row still present");
        assert!(folder.collapsed);
        // The filed child row is still emitted (so the filter can find/reveal
        // it), but hidden while the folder is collapsed.
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::Worktree && r.label == "feat" && !r.visible),
            "collapsed folder's child is emitted but not visible"
        );
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::Worktree && r.label == "home" && r.visible)
        );
    }

    #[test]
    fn filter_finds_worktree_in_collapsed_folder() {
        // Regression: a worktree filed into a *collapsed* folder used to be
        // unfindable — its row wasn't even emitted. Now the filter surfaces it
        // (and its folder + workspace headers), matching the collapsed-workspace
        // contract.
        let (s, ws, dbw, folders) = folder_fixture();
        let view = ViewState {
            collapsed: ["app/folder:1".to_string()].into_iter().collect(),
            filter: "feat".into(),
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &folders, &[]);
        let visible: Vec<&str> = rows
            .iter()
            .filter(|r| r.visible)
            .map(|r| r.label.as_str())
            .collect();
        assert!(
            visible.contains(&"feat"),
            "filtered filed worktree surfaces"
        );
        assert!(visible.contains(&"Backend"), "its folder header surfaces");
        assert!(visible.contains(&"app"), "its workspace header surfaces");
        assert!(
            !visible.contains(&"home"),
            "non-matching sibling stays hidden"
        );
    }

    #[test]
    fn filter_on_folder_name_surfaces_workspace_and_children() {
        // Filtering by a folder's own label surfaces its parent workspace (so it
        // isn't orphaned at depth 1) and reveals its children.
        let (s, ws, dbw, folders) = folder_fixture();
        let view = ViewState {
            filter: "backend".into(),
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &folders, &[]);
        let visible: Vec<&str> = rows
            .iter()
            .filter(|r| r.visible)
            .map(|r| r.label.as_str())
            .collect();
        assert!(visible.contains(&"Backend"), "matched folder shows");
        assert!(visible.contains(&"app"), "parent workspace surfaces");
        assert!(visible.contains(&"feat"), "folder's children revealed");
    }

    #[test]
    fn worktree_with_orphaned_folder_id_falls_back_to_loose() {
        // Regression: a worktree whose `folder_id` points at a folder that is
        // NOT in this workspace's folder set (e.g. a merge-queue `file_into`
        // that recorded the folder under a `repo_path` string that doesn't
        // byte-match the workspace's, so the header is filtered out) must still
        // render — as a loose row — never vanish.
        let (s, ws, mut dbw, folders) = folder_fixture();
        // The worktree is filed into folder 1, but the only folder row we pass
        // belongs to a DIFFERENT repo_path, so it yields no header here.
        dbw[0].folder_id = Some(1);
        let mismatched = vec![thegn_core::models::FolderRow {
            folder_id: 1,
            repo_path: "/some/other/path".into(),
            name: "Backend".into(),
            position: 0,
            created_at: 0,
        }];
        let _ = folders; // fixture's matching folder intentionally unused here.

        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &mismatched,
            &[],
        );
        // No folder header renders (repo_path mismatch)…
        assert!(
            !rows.iter().any(|r| r.kind == RowKind::Folder),
            "mismatched folder must not render a header"
        );
        // …but the filed worktree is still visible, as a loose (depth 1) row.
        let feat = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.label == "feat")
            .expect("filed worktree must not vanish when its folder has no header");
        assert_eq!(
            feat.depth, 1,
            "orphaned-folder worktree falls back to loose"
        );
    }

    #[test]
    fn dormant_workspace_pins_float_like_live() {
        // Pin identity parity: the same `pin_key` (`slug/branch`) must exist —
        // and float the row identically — whether the workspace is live or
        // reconstructed from the DB. A dormant tree that dropped pins would
        // silently reshuffle on every workspace switch.
        let mk_dbw = |branch: &str, path: &str| DbWorktree {
            slug: "app".into(),
            branch: branch.into(),
            repo_path: "/repos/app".into(),
            tab_name: format!("app/{branch}"),
            path: path.into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        };
        let live = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/feat", "/wt/feat"),
                tab("app/zeta", "/wt/zeta"),
            ],
            0,
        );
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let dbw = vec![mk_dbw("feat", "/wt/feat"), mk_dbw("zeta", "/wt/zeta")];
        let view = ViewState {
            pins: vec!["app/zeta".to_string()],
            ..Default::default()
        };
        let order = |s: &Session| -> Vec<String> {
            build_rows(s, &ws, &view, &no_activity(), &dbw, &[], &[])
                .into_iter()
                .filter(|r| r.kind == RowKind::Worktree)
                .map(|r| r.label)
                .collect()
        };
        let live_order = order(&live);
        assert_eq!(
            live_order,
            order(&session(vec![], 0)),
            "pinned order must match live vs dormant",
        );
        // And the pin actually floated zeta above feat.
        let zi = live_order.iter().position(|l| l == "zeta").unwrap();
        let fi = live_order.iter().position(|l| l == "feat").unwrap();
        assert!(zi < fi, "pinned zeta floats above feat: {live_order:?}");
    }

    #[test]
    fn dormant_workspace_renders_same_structure_as_live() {
        // The same workspace rendered live (loaded in the session) and dormant
        // (parked → reconstructed from the DB) must produce the same tree
        // shape. Only the row targets / active flag legitimately differ, so we
        // compare (kind, depth, label) — folders, the loose/filed split, and
        // sort order all have to match.
        let (live_s, ws, dbw, folders) = folder_fixture();
        let shape = |s: &Session| -> Vec<(RowKind, u8, String)> {
            build_rows(
                s,
                &ws,
                &ViewState::default(),
                &no_activity(),
                &dbw,
                &folders,
                &[],
            )
            .into_iter()
            .take_while(|r| r.kind != RowKind::SectionHeading)
            .map(|r| (r.kind, r.depth, r.label.clone()))
            .collect()
        };
        assert_eq!(shape(&live_s), shape(&session(vec![], 0)));
    }

    #[test]
    fn dormant_workspace_files_worktrees_under_folders() {
        // Regression: a dormant workspace used to render a flat, folder-less
        // list. It must now emit the folder header + its filed child at depth 2,
        // with a workspace-switch target (landing on the named group).
        let (_s, ws, dbw, folders) = folder_fixture();
        let rows = build_rows(
            &session(vec![], 0),
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &folders,
            &[],
        );
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::Folder && r.label.starts_with("Backend"))
        );
        let feat = rows.iter().find(|r| r.label == "feat").expect("filed row");
        assert_eq!(feat.depth, 2);
        assert_eq!(
            feat.tab_target,
            Some(RowTarget::Workspace {
                repo_path: "/repos/app".into(),
                group: Some("app/feat".into()),
            })
        );
    }

    #[test]
    fn dormant_workspace_respects_sort_mode() {
        // Under Name sort a dormant workspace's non-home worktrees alphabetize
        // (they previously rendered in raw DB order, ignoring the sort).
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let mk = |branch: &str, path: &str| DbWorktree {
            slug: "app".into(),
            branch: branch.into(),
            repo_path: "/repos/app".into(),
            tab_name: format!("app/{branch}"),
            path: path.into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        };
        let dbw = vec![mk("zebra", "/wt/zebra"), mk("alpha", "/wt/alpha")];
        let view = ViewState {
            sort: SortMode::Name,
            ..Default::default()
        };
        let rows = build_rows(
            &session(vec![], 0),
            &ws,
            &view,
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
        assert_eq!(labels, vec!["home", "alpha", "zebra"]);
    }

    #[test]
    fn active_reveal_keys_covers_workspace_and_folder() {
        let dbw = vec![DbWorktree {
            slug: "app".into(),
            branch: "feat".into(),
            repo_path: "/repos/app".into(),
            tab_name: "app/feat".into(),
            path: "/wt/feat".into(),
            folder_id: Some(3),
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
        }];
        // Filed worktree → both its workspace and its folder key.
        assert_eq!(
            active_reveal_keys("app/feat", &dbw),
            vec!["app".to_string(), "app/folder:3".to_string()]
        );
        // Loose worktree (no matching db row) → workspace only.
        assert_eq!(
            active_reveal_keys("app/home", &dbw),
            vec!["app".to_string()]
        );
        // A non-worktree name (no slash) → nothing to reveal.
        assert!(active_reveal_keys("scratch", &dbw).is_empty());
    }

    #[test]
    fn parent_collapsible_index_walks_up_to_the_group() {
        let (s, ws, dbw, folders) = folder_fixture();
        let terms = vec![term("t1", "remote", "ssh prod")];
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &folders,
            &terms,
        );
        let visible: Vec<&SidebarRow> = rows.iter().filter(|r| r.visible).collect();
        let idx = |kind: RowKind, label: &str| {
            visible
                .iter()
                .position(|r| r.kind == kind && r.label == label)
                .unwrap_or_else(|| panic!("row {label:?} present"))
        };

        // Filed worktree → its Folder; loose worktree → its Workspace.
        let feat = idx(RowKind::Worktree, "feat");
        assert_eq!(
            parent_collapsible_index(&visible, feat),
            Some(idx(RowKind::Folder, "Backend"))
        );
        let home = idx(RowKind::Worktree, "home");
        assert_eq!(
            parent_collapsible_index(&visible, home),
            Some(idx(RowKind::Workspace, "app"))
        );
        // Folder → its Workspace; a terminal → its TerminalHost.
        assert_eq!(
            parent_collapsible_index(&visible, idx(RowKind::Folder, "Backend")),
            Some(idx(RowKind::Workspace, "app"))
        );
        assert_eq!(
            parent_collapsible_index(&visible, idx(RowKind::Terminal, "t1")),
            Some(idx(RowKind::TerminalHost, "prod"))
        );
        // A top-level workspace has no collapsible ancestor.
        assert_eq!(
            parent_collapsible_index(&visible, idx(RowKind::Workspace, "app")),
            None
        );
    }

    // ---- derived pipeline folders (THE-74) -------------------------------

    use crate::sidebar_pipeline::{Lane, LaneWorktree};

    fn lane_status(lanes: Vec<Lane>) -> SidebarStatus {
        SidebarStatus {
            pipeline: PipelineSummary {
                active: 0,
                waiting_human: 0,
            },
            pipeline_lanes: lanes,
            ..SidebarStatus::default()
        }
    }

    /// Same lanes, but with live dispatches — the door row shows.
    fn live_lane_status(lanes: Vec<Lane>) -> SidebarStatus {
        SidebarStatus {
            pipeline: PipelineSummary {
                active: 1,
                waiting_human: 0,
            },
            ..lane_status(lanes)
        }
    }

    fn lane(key: &str, worktrees: &[&str]) -> Lane {
        Lane {
            key: key.into(),
            label: key.into(),
            worktrees: worktrees
                .iter()
                .enumerate()
                .map(|(i, wt)| LaneWorktree {
                    path: (*wt).into(),
                    name: thegn_core::util::basename(wt).to_string(),
                    at_ms: 1_000 + i as i64,
                })
                .collect(),
        }
    }

    fn one_lane_status(worktree: &str) -> SidebarStatus {
        lane_status(vec![lane("THE-74", &[worktree])])
    }

    fn app_workspace() -> Vec<(String, String, String, String)> {
        vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )]
    }

    /// The USER-DIRECTIVE shape: a lane's rows are inside the workspace that
    /// owns its worktrees — `[Workspace] → "Pipelines" → [pipeline] →
    /// [worktrees]` — and the tail door row is untouched after them.
    #[test]
    fn a_lane_emits_group_lane_and_worktree_inside_its_workspace() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &live_lane_status(vec![lane("THE-74", &["/wt/home"])]),
            &[],
            &[],
            &[],
        );
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        let group = kinds
            .iter()
            .position(|k| *k == RowKind::PipelineGroup)
            .expect("the Pipelines group sits inside the workspace");
        // Workspace first, then its own worktree rows, then the group chain.
        assert_eq!(kinds[0], RowKind::Workspace);
        assert!(group > 1, "the group is not the workspace itself");
        assert_eq!(
            &kinds[group..group + 3],
            &[
                RowKind::PipelineGroup,
                RowKind::PipelineLane,
                RowKind::PipelineWorktree,
            ],
        );
        assert_eq!(rows[group].label, "Pipelines");
        assert_eq!(rows[group].depth, 1);
        assert_eq!(rows[group].child_count, 1, "the group counts its lanes");
        assert_eq!(rows[group + 1].label, "THE-74", "named from the issue id");
        assert_eq!(rows[group + 1].depth, 2);
        assert_eq!(
            rows[group + 1].child_count,
            1,
            "the lane counts its worktrees"
        );
        assert_eq!(rows[group + 2].depth, 3);
        assert_eq!(rows[group + 2].label, "home");
        // Everything is visible BY DEFAULT.
        for r in &rows[group..group + 3] {
            assert!(r.visible, "{:?} must be visible by default", r.kind);
        }
        // The tail door row is unchanged and comes after the workspace tree.
        let door = kinds
            .iter()
            .position(|k| *k == RowKind::PipelineSummary)
            .expect("the board door stays");
        assert!(door > group + 2);
    }

    #[test]
    fn a_terminal_roster_still_produces_the_lane_folders() {
        // The directive's restart-survival property: the folders ride the
        // roster's rows of ANY status, so they are there with zero active
        // dispatches — even when the `Pipeline ▸ N running` door row is not.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let status = lane_status(vec![lane("THE-74", &["/wt/home"])]);
        assert_eq!(status.pipeline.active, 0, "no live agents");
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &status,
            &[],
            &[],
            &[],
        );
        assert!(
            !rows.iter().any(|r| r.kind == RowKind::PipelineSummary),
            "no door row without live rows"
        );
        let lane = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineLane)
            .expect("the lane folder survives");
        assert!(lane.visible);
        let mirror = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .expect("its worktree survives with it");
        assert!(mirror.visible);
    }

    #[test]
    fn a_lane_files_under_the_workspace_that_owns_its_worktrees() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let workspaces = vec![
            (
                "app".to_string(),
                "app".to_string(),
                "repo".to_string(),
                String::new(),
            ),
            (
                "lib".to_string(),
                "lib".to_string(),
                "repo".to_string(),
                String::new(),
            ),
        ];
        let status = lane_status(vec![lane("THE-74", &["/wt/home"])]);
        let rows = build_rows(
            &s,
            &workspaces,
            &ViewState::default(),
            &status,
            &[],
            &[],
            &[],
        );
        let app_ws = rows
            .iter()
            .position(|r| r.kind == RowKind::Workspace && r.label == "app")
            .unwrap();
        let lib_ws = rows
            .iter()
            .position(|r| r.kind == RowKind::Workspace && r.label == "lib")
            .unwrap();
        let group = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineGroup)
            .unwrap();
        assert!(
            group > app_ws && group < lib_ws,
            "one group, inside app (the worktree owner), not lib"
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == RowKind::PipelineGroup)
                .count(),
            1,
            "exactly one Pipelines group"
        );
    }

    #[test]
    fn a_worktree_no_roster_row_references_stays_where_it_was() {
        // Two worktrees; only one is referenced. The other must keep its
        // primary row and gain nothing under Pipelines.
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/other", "/wt/other")],
            0,
        );
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let mirrors: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::PipelineWorktree)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(
            mirrors,
            vec!["home"],
            "only the referenced worktree mirrors"
        );
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::Worktree && r.label == "other"),
            "the unreferenced worktree keeps its primary row"
        );
    }

    #[test]
    fn a_lane_worktree_mirrors_the_primary_rows_target_without_its_identity() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let primary = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some("/wt/home"))
            .expect("the primary worktree row");
        let mirror = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .expect("the lane's worktree row");
        assert_eq!(mirror.tab_target, primary.tab_target, "same door");
        // The mirror's `pin_key` is its OWN identity — lane-scoped and
        // path-qualified — so the menu/double-click/cursor anchors resolve the
        // mirror and never the primary row (or a sibling mirror).
        assert_ne!(primary.pin_key, mirror.pin_key);
        assert!(mirror.pin_key.starts_with("pipeline/lane:THE-74/wt:"));
        assert_eq!(mirror.pin_key, "pipeline/lane:THE-74/wt:/wt/home");
        // …while remaining unpinnable/unmarkable: the kind gates, not the
        // empty key, are what keep it out of the pin and mark sets.
        assert!(!mirror.is_markable());
        assert!(!mirror.is_pinnable());
    }

    #[test]
    fn two_mirrors_of_one_worktree_in_different_lanes_never_share_a_pin_key() {
        // The context menu re-anchors by `pin_key`; two mirrors sharing a key
        // would fire one lane's menu entry at the other lane's worktree.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &lane_status(vec![
                lane("THE-74", &["/wt/home"]),
                lane("THE-9", &["/wt/home"]),
            ]),
            &[],
            &[],
            &[],
        );
        let mut keys: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::PipelineWorktree)
            .map(|r| r.pin_key.as_str())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), 2, "one unique anchor per mirror: {keys:?}");
    }

    #[test]
    fn a_lane_worktree_with_no_primary_row_stays_but_opens_nothing() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &one_lane_status("/elsewhere/tg-gone"),
            &[],
            &[],
            &[],
        );
        let mirror = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .expect("row is still emitted");
        assert_eq!(mirror.label, "tg-gone");
        assert!(mirror.tab_target.is_none());
    }

    #[test]
    fn an_unresolvable_lane_falls_back_to_the_tail_group() {
        // A lane none of whose worktrees resolve to any registration must
        // still be reachable: it groups at the tail under the board door.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &live_lane_status(vec![lane("THE-74", &["/elsewhere/tg-gone"])]),
            &[],
            &[],
            &[],
        );
        let group = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineGroup)
            .expect("the tail Pipelines group");
        assert_eq!(group.workspace_slug, "pipeline");
        assert_eq!(group.pin_key, "pipeline/group:unfiled");
        let door = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineSummary)
            .expect("the door row");
        let gi = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineGroup)
            .unwrap();
        assert!(gi > door, "the tail group rides under the door");
    }

    #[test]
    fn no_lanes_means_no_pipeline_folder_rows() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
        assert!(
            !rows.iter().any(|r| matches!(
                r.kind,
                RowKind::PipelineGroup | RowKind::PipelineLane | RowKind::PipelineWorktree
            )),
            "no roster means no derived rows at all"
        );
    }

    #[test]
    fn collapsing_a_lane_hides_its_worktrees_but_still_emits_them() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let mut view = ViewState::default();
        view.collapsed.insert("pipeline/lane:THE-74".into());
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let lane = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineLane)
            .unwrap();
        assert!(lane.visible && lane.collapsed);
        assert_eq!(lane.collapse_key(), "pipeline/lane:THE-74");
        let wt = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .unwrap();
        assert!(!wt.visible, "the leaf hides under its collapsed lane");
    }

    #[test]
    fn collapsing_the_pipelines_group_hides_its_lanes_but_still_emits_them() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let mut view = ViewState::default();
        view.collapsed.insert("pipeline/group:app".into());
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let group = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineGroup)
            .unwrap();
        assert!(group.visible && group.collapsed);
        assert_eq!(group.collapse_key(), "pipeline/group:app");
        for r in rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::PipelineLane | RowKind::PipelineWorktree))
        {
            assert!(!r.visible, "{:?} hides under a collapsed group", r.kind);
        }
    }

    #[test]
    fn two_lanes_collapse_independently() {
        // They share one `workspace_slug`, so keying collapse on it would fold
        // every lane at once — the folder precedent (`pin_key`) is why not.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let mut view = ViewState::default();
        view.collapsed.insert("pipeline/lane:THE-9".into());
        let status = lane_status(vec![
            lane("THE-74", &["/wt/home"]),
            lane("THE-9", &["/wt/home"]),
        ]);
        let rows = build_rows(&s, &app_workspace(), &view, &status, &[], &[], &[]);
        let lanes: Vec<(&str, bool)> = rows
            .iter()
            .filter(|r| r.kind == RowKind::PipelineLane)
            .map(|r| (r.pin_key.as_str(), r.collapsed))
            .collect();
        assert_eq!(
            lanes,
            vec![
                ("pipeline/lane:THE-74", false),
                ("pipeline/lane:THE-9", true)
            ]
        );
    }

    #[test]
    fn no_pipeline_row_is_markable_pinnable_or_reorderable() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let derived: Vec<&SidebarRow> = rows
            .iter()
            .filter(|r| {
                matches!(
                    r.kind,
                    RowKind::PipelineGroup | RowKind::PipelineLane | RowKind::PipelineWorktree
                )
            })
            .collect();
        assert_eq!(derived.len(), 3);
        for r in derived {
            assert!(
                !r.is_markable(),
                "{:?} must never join the mark set",
                r.kind
            );
            assert!(!r.is_pinnable(), "{:?} must never be pinned", r.kind);
            assert!(r.folder_id.is_none(), "{:?} is not a real folder", r.kind);
        }
    }

    #[test]
    fn a_lane_row_is_skipped_by_the_pin_path() {
        // `apply_pins` floats rows by `pin_key`; a lane's key exists only for
        // collapse, so even a stale persisted pin must not hoist it.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            pins: vec!["pipeline/lane:THE-74".into()],
            ..ViewState::default()
        };
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        assert_eq!(rows[0].kind, RowKind::Workspace, "the tree is unchanged");
        let group = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineGroup)
            .unwrap();
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == RowKind::PipelineGroup)
                .count(),
            1,
            "the stale pin hoisted nothing"
        );
        assert!(group > 1, "the group still sits inside its workspace");
    }

    #[test]
    fn the_filter_reveals_a_row_inside_a_collapsed_lane() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let mut view = ViewState::default();
        view.collapsed.insert("pipeline/lane:THE-74".into());
        view.filter = "home".into();
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let mirror = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .unwrap();
        assert!(mirror.visible, "a matching leaf is revealed");
        let lane = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineLane)
            .unwrap();
        assert!(lane.visible, "its lane is surfaced with it");
        let group = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineGroup)
            .unwrap();
        assert!(group.visible, "and so is the Pipelines group");
        let ws = rows.iter().find(|r| r.kind == RowKind::Workspace).unwrap();
        assert!(ws.visible, "and the workspace header above it");
    }

    #[test]
    fn a_lane_match_reveals_its_worktrees() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            filter: "THE-74".into(),
            ..ViewState::default()
        };
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let lane = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineLane)
            .unwrap();
        assert!(lane.visible, "the matching lane stays");
        let wt = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .unwrap();
        assert!(wt.visible, "its worktrees reveal with it");
    }

    #[test]
    fn the_flat_layout_groups_every_lane_at_the_tail() {
        // Flat mode has no workspace rows to nest under, so the Pipelines
        // group rides the tail under the board door.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            flat: true,
            ..ViewState::default()
        };
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &live_lane_status(vec![lane("THE-74", &["/wt/home"])]),
            &[],
            &[],
            &[],
        );
        let group = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineGroup)
            .expect("a tail group in flat mode");
        assert_eq!(group.pin_key, "pipeline/group:unfiled");
        let door = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineSummary)
            .expect("the door row");
        let gi = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineGroup)
            .unwrap();
        assert!(gi > door);
    }
}
