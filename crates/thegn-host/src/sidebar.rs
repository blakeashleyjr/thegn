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
use std::fmt::Write as _;

use crate::session::Session;
use thegn_core::config_ui::ExternalWorktrees;
use thegn_svc::git::{BranchMeta, DiffCount};

pub(crate) mod fold;

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
    /// A pipeline-owned worktree's primary row, nested inside its lane folder.
    /// It carries the same worktree target/status/action contract as an
    /// ordinary row. Its `{workspace}/{label}` pin identity is shared with the
    /// ordinary grouped and flat layouts, so pins survive ownership changes.
    /// Drag reordering remains unavailable because lane order comes from the
    /// dispatch roster.
    PipelineWorktree,
    /// A **derived** "Other (N)" group under a workspace holding its
    /// *external* worktrees — git worktrees thegn did not create (see
    /// [`is_external`]) — when `[ui] sidebar_external_worktrees = "group"`.
    /// Keyed `other:{slug}`; collapsible (collapsed by default, per `[ui]
    /// sidebar_other_group_collapsed`, remembered per project as
    /// `other_open:{slug}`). Like [`RowKind::PipelineGroup`] it carries no
    /// `folder_id` and no `tab_target`, and is not markable, pinnable,
    /// fileable or draggable: there is no `folders` row behind it.
    OtherGroup,
    /// A **derived** batch row under a workspace (grouped layout only)
    /// folding its loose worktrees that share a batch token (`tg/b158-…` ->
    /// `b158`; see [`fold::batch_key`]) once at least `[ui]
    /// sidebar_batch_group_min` share it. Keyed `batch:{slug}/{token}`;
    /// collapsible (collapsed by default, opened state remembered as
    /// `batch_open:{key}`). Like [`RowKind::OtherGroup`] it carries no
    /// `folder_id` and no `tab_target`, and is not markable, pinnable,
    /// fileable or draggable: there is no `folders` row behind it.
    BatchGroup,
    /// A **derived** "… show N more" / "show fewer" toggle closing a group
    /// whose entries exceed `[ui] sidebar_group_row_limit` (or its
    /// `sidebar_group_row_limits` override; see `apply_row_limits`). Sits at
    /// the group's child depth. Its `pin_key` is [`show_more_key`] of the
    /// group key (identity anchor only: it is never pinnable, markable,
    /// draggable or a tab target), `child_count` is how many entries the
    /// limit hides, and `collapsed` is `true` while they are hidden. Enter,
    /// Space and a click flip the group's `more_open:{group key}` state.
    ShowMore,
}

impl RowKind {
    /// Whether this row is the actionable row for a worktree. Pipeline-owned
    /// worktrees retain the same activation and navigation behavior as the
    /// ordinary worktree rows they replace.
    pub(crate) fn is_worktree(self) -> bool {
        matches!(self, RowKind::Worktree | RowKind::PipelineWorktree)
    }

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
                | RowKind::OtherGroup
                | RowKind::BatchGroup
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
/// `add`/`del` are typed uncommitted working-tree counts vs HEAD; `branch_diff`
/// is the typed `(added, deleted)` total vs the default branch (`None` when no base).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitGlyphs {
    pub dirty: bool,
    pub ahead: usize,
    pub behind: usize,
    pub add: DiffCount,
    pub del: DiffCount,
    pub branch_diff: Option<(DiffCount, DiffCount)>,
    /// A submodule gitlink is moved, dirty, untracked, conflicted, unavailable,
    /// or uninitialized. Kept separate from ordinary-file dirtiness.
    pub submodule_dirty: bool,
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
    /// True for the sorts whose keys are **computed off-loop** and can therefore
    /// re-rank on their own between keystrokes (`Attention` from the hydration
    /// thread's tier ranks, `Live` from the activity FSM's `last_active_at`).
    /// The other three key on data that only changes when the user changes it,
    /// so they need no freeze — and must not advertise one (see
    /// `crate::sidebar_freeze`).
    pub fn is_computed(self) -> bool {
        matches!(self, SortMode::Attention | SortMode::Live)
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

/// The latest cached CI result for one worktree, as the sidebar `ci` detail
/// field shows it. Derived off-loop from the same newest valid-id cached run
/// the attention scorer reads (`attention_status::ci_badge`). Absent means
/// unknown, never a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiBadge {
    Pass,
    Fail,
    Running,
    Pending,
}

/// A value supplied by a plugin for one worktree row field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginFieldValue {
    pub plugin: String,
    pub field: String,
    pub text: String,
    pub role: thegn_core::plugin_api::RowRole,
    pub slot: thegn_core::plugin_api::RowFieldSlot,
}

/// One row in the workspace tree.
#[derive(Debug, Clone)]
pub struct SidebarRow {
    /// Zone chip for Workspace rows (`None` = unzoned or no repo path).
    pub zone: Option<ZoneTag>,
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
    pub branch_meta: Option<BranchMeta>,
    pub git: Option<GitGlyphs>,
    pub sandbox_backend: Option<String>,
    /// Selected execution environment (`[env.<name>]`); `None`/`"default"` ⇒
    /// the implicit default (no badge shown).
    pub env_name: Option<String>,
    /// Explicit worktree profile pin, shown only when present.
    pub sandbox_profile: Option<String>,
    /// The env is a managed provider but content resolved local (degraded to the
    /// host) — renders the `«env»` badge as `«env ✗»`.
    pub env_degraded: bool,
    /// Read-only devcontainer variant/provider status from hydration.
    pub devcontainer_status: Option<String>,
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
    /// Worktree rows filed under the `Merged` folder: the detail line drops the
    /// last commit subject (a truncated "chore(r…" says nothing) and keeps the
    /// age and size fields.
    pub hide_subject: bool,
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
    /// The worktree's latest cached CI result, denormalized from
    /// `SidebarStatus::ci_states` in the same pass. `None` = no cached run.
    pub ci_state: Option<CiBadge>,
    /// The worktree's newest linked issue id and total linked count,
    /// denormalized from `SidebarStatus::linked_issues` in the same pass.
    /// `None` = no linked issue.
    pub linked_issue: Option<(String, usize)>,
    /// Flat-layout only: the owning workspace's display name, rendered as a dim
    /// prefix so a flat cross-repo worktree row still shows which repo it
    /// belongs to. `None` in grouped mode (the workspace header gives context).
    pub repo_prefix: Option<String>,
    /// Workspace-only merge-queue rollup, derived from the statuses of its
    /// child worktree rows. `None` for worktrees and workspaces with no active
    /// queue entries.
    pub mq_rollup: Option<thegn_core::merge_queue_view::MqRollup>,
    /// Resident plugin values resolved for this worktree. Empty when no
    /// installed plugin has published a value for the row.
    pub plugin_fields: Vec<PluginFieldValue>,
    /// Batch members: the leading part of the label hidden on screen (`tg/b161-b161-`).
    pub batch_prefix: Option<String>,
    /// BatchGroup rows: the members' rollup (filled by `fold::rollup_batch_stats`).
    pub group_stats: Option<fold::GroupStats>,
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
            zone: None,
            kind,
            depth,
            label: label.into(),
            workspace_slug: workspace_slug.into(),
            tab_target: None,
            active: false,
            worktree_path: None,
            pin_key: String::new(),
            branch: None,
            branch_meta: None,
            git: None,
            sandbox_backend: None,
            env_name: None,
            sandbox_profile: None,
            env_degraded: false,
            devcontainer_status: None,
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
            hide_subject: false,
            attention: None,
            mq_status: None,
            pipeline_stage: None,
            ci_state: None,
            linked_issue: None,
            repo_prefix: None,
            mq_rollup: None,
            plugin_fields: Vec::new(),
            batch_prefix: None,
            group_stats: None,
        }
    }

    /// Whether this row can join the multi-select set: a workspace or worktree
    /// with a stable identity. Excludes section headings / empty hints (no
    /// `pin_key`) and folders / terminals (no bulk or reorder action).
    pub fn is_markable(&self) -> bool {
        !self.pin_key.is_empty() && (self.kind == RowKind::Workspace || self.kind.is_worktree())
    }

    /// Whether this row can be pinned (floated to the top of its sibling run).
    /// Groups carry a key for collapse state. Worktree leaves share one
    /// identity across ordinary and pipeline layouts.
    pub fn is_pinnable(&self) -> bool {
        !self.pin_key.is_empty()
            && !matches!(
                self.kind,
                RowKind::PipelineGroup
                    | RowKind::PipelineLane
                    | RowKind::OtherGroup
                    | RowKind::BatchGroup
                    | RowKind::ShowMore
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
            RowKind::Folder
            | RowKind::PipelineGroup
            | RowKind::PipelineLane
            | RowKind::OtherGroup
            | RowKind::BatchGroup => &self.pin_key,
            _ => &self.workspace_slug,
        }
    }
}

/// A workspace row's zone chip state.
#[derive(Debug, Clone, PartialEq)]
pub enum ZoneTag {
    /// Member of zone `name`; `policy` is false when config has no
    /// `[zone.<name>]` table for it.
    Named { name: String, policy: bool },
    /// The zone catalog could not be read.
    Unknown,
}

/// Resolve a workspace's chip from the hydrated catalog.
fn zone_tag(
    zones: &crate::chrome::ZoneCatalog,
    unpoliced: &std::collections::BTreeSet<String>,
    repo_path: &str,
) -> Option<ZoneTag> {
    if repo_path.is_empty() {
        return None;
    }
    if let Some(name) = zones.by_repo.get(repo_path) {
        return Some(ZoneTag::Named {
            name: name.clone(),
            policy: !unpoliced.contains(name),
        });
    }
    zones.read_failed.then_some(ZoneTag::Unknown)
}

/// Per-worktree status sourced from the (possibly slow) git/activity scan on
/// the hydration thread, merged onto rows at build time. `git`/`agent` are
/// keyed by worktree path; `activity` by tab name (matching the `activity`
/// state machine's TSV keys).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SidebarStatus {
    /// Zone catalog mirrored from `FrameModel.zone_catalog` by the hydrators
    /// (`build_rows` runs on-loop without the model, so it reads it here).
    pub zones: crate::chrome::ZoneCatalog,
    /// Zone names that have no `[zone.<name>]` config policy (chip shows `!`).
    pub zones_unpoliced: std::collections::BTreeSet<String>,
    /// The Lead indicator's facts (batch 180), gathered off-loop by `lead::status::gather`. None without `[lead]`, on stable, or under the e2e freeze. In `PartialEq`, so a change repaints (model_eq.rs:123).
    pub lead: Option<thegn_core::lead_bar::LeadBar>,
    pub git: std::collections::BTreeMap<String, GitGlyphs>,
    pub branch_meta: std::collections::BTreeMap<String, BranchMeta>,
    /// The worktree's LIVE `HEAD` branch, keyed by path — read by the same git
    /// scan that produces `git`. The row's `{slug}/{branch}` tab name is only a
    /// creation-time identity (it never moves when you `git checkout` inside the
    /// worktree), so the displayed branch must come from here or it goes stale
    /// forever. Absent for a detached HEAD or a worktree never scanned.
    pub branches: std::collections::BTreeMap<String, String>,
    /// Read-only devcontainer variant/provider status, keyed by worktree path.
    pub devcontainer_status: std::collections::BTreeMap<String, String>,
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
    /// Per-worktree latest cached CI result (keyed by path), filled by the
    /// attention scan from the CI cache it already reads. A worktree with no
    /// cached run has no entry.
    pub ci_states: std::collections::BTreeMap<String, CiBadge>,
    /// Per-worktree newest linked issue id and total linked count (keyed by
    /// path), filled by the attention scan from the local issue-link table
    /// (no fetch). A worktree with no linked issue has no entry.
    pub linked_issues: std::collections::BTreeMap<String, (String, usize)>,
    /// Resident plugin row-field values keyed by worktree path.
    pub plugin_fields: std::collections::BTreeMap<String, Vec<PluginFieldValue>>,
    /// The derived lane folders under each project's Pipelines group. The
    /// bounded roster names issue lanes; persisted-only ownership paths get a
    /// stable path lane. See [`crate::sidebar_pipeline::lanes_with_persisted_paths`].
    pub pipeline_lanes: Vec<crate::sidebar_pipeline::Lane>,
    /// Durable ownership from every persisted staged dispatch row. This is
    /// deliberately independent of the bounded roster used to name lanes.
    pub pipeline_worktree_paths: std::collections::BTreeSet<String>,
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
    /// The Lead section shows only when `[lead]` is configured (mirrored from
    /// `crate::lead::enabled` on startup/reload like `terminals_section`).
    pub lead_enabled: bool,
    /// Show the "Lead — set up" row (the Lead is not configured). Opens the
    /// in-app setup flow; mirrored from `!crate::lead::enabled` like
    /// `lead_enabled`, on both channels.
    pub lead_setup_row: bool,
    /// Flat cross-workspace layout (`g`): drop per-repo grouping and show one
    /// recency-ordered list of every worktree, each tagged with its repo.
    /// Persisted as the `sidebar_flat` ui_state key; independent of `sort`.
    pub flat: bool,
    /// Worktree-row display options, from `[ui]` config (mirrored here on
    /// startup/reload like `workspace_sort`). Which right-cluster fields show,
    /// the focused-detail policy, and glyph overrides.
    pub display: crate::sidebar_view::SidebarDisplay,
    /// Per-project expansion override of the derived "Other" group, keyed by
    /// workspace slug: `true` = open, `false` = collapsed. Absent = follow
    /// `[ui] sidebar_other_group_collapsed` (`display.other_group_collapsed`).
    /// Persisted as `other_open:{slug}` (`"1"`/`"0"`) in `ui_state`.
    pub other_open: std::collections::HashMap<String, bool>,
    /// Groups the user expanded past their row limit ("show more"), keyed by
    /// the group head's [`SidebarRow::collapse_key`]. Absent = truncated.
    /// Persisted as `more_open:{group key}` (`"1"`) in `ui_state`.
    pub more_open: HashSet<String>,
    /// Frozen sort keys, armed while the user is navigating the bar (see
    /// [`crate::sidebar_freeze`]). While `Some`, the computed sorts read this
    /// snapshot instead of the live [`SidebarStatus`] maps, so rows cannot move
    /// under the cursor. `Arc` so a rebuild clones a pointer, not three maps.
    ///
    /// Loop-owned and transient: never persisted to `ui_state`, and never part
    /// of hydration equality.
    pub freeze: Option<std::sync::Arc<crate::sidebar_freeze::SortFreeze>>,
    /// Batch groups the user opened (`batch_open:{key}` = "1"); absent = collapsed.
    pub batch_open: HashSet<String>,
    /// "Clear finished" cut-offs, keyed by workspace slug (epoch ms): a
    /// finished pipeline lane whose last activity is at or before the cut-off
    /// leaves that workspace's sidebar (its worktree is untouched). Persisted
    /// as `finished_cleared:{slug}` in `ui_state`.
    pub finished_cleared: std::collections::HashMap<String, i64>,
    /// The focused cursor row's pin_key at rebuild time; the row limit never folds it.
    /// Loop-owned and transient, never persisted.
    pub cursor_key: Option<String>,
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
    /// Explicit worktree profile pin, if one is stored.
    pub sandbox_profile: Option<String>,
    /// The env is a managed PROVIDER but the worktree's content resolved local
    /// (degraded to the host / never provisioned) — renders the `«env»` badge as
    /// `«env ✗»`. Computed in `hydrate::db_worktree_list_with_snapshot`.
    pub env_degraded: bool,
    /// A worktree thegn did not create (see [`is_external`]): placed per
    /// `[ui] sidebar_external_worktrees`. Computed in
    /// `hydrate::db_worktree_list_with_snapshot`.
    pub external: bool,
}

/// The label of a workspace's derived pipeline group. A user folder spelled
/// the same is folded into it rather than shown as a second `Pipelines`.
pub const PIPELINES_LABEL: &str = "Pipelines";

/// The `pin_key` (and collapse key) of a workspace's `Finished` pipelines
/// sub-group.
pub fn finished_group_key(slug: &str) -> String {
    format!("pipeline/finished:{slug}")
}

/// The `ViewState::collapsed`-style key of a workspace's derived "Other"
/// group (also its `pin_key`).
pub fn other_group_key(slug: &str) -> String {
    format!("other:{slug}")
}

/// Inputs to [`is_external`]: one registered worktree row and the
/// workspace context it is classified against.
pub struct ExternalCheck<'a> {
    /// The workspace's repo root (its home checkout).
    pub root: &'a std::path::Path,
    /// The readable per-repo directory name thegn files checkouts under.
    pub repo_dir_name: &'a str,
    /// `[project.<slug>] worktree_roots`, already tilde-expanded.
    pub roots: &'a [std::path::PathBuf],
    pub cfg: &'a thegn_core::config::Config,
}

/// Whether a registered worktree at `path` (filed into `folder_id`) is
/// **external**: outside thegn's own checkout layout
/// ([`thegn_core::worktree::is_managed_checkout`]), outside every
/// `worktree_roots` entry, not filed into a folder (a row the user filed is
/// adopted), and not the repo's home checkout. Pure string/path-component
/// comparison — no canonicalisation, no I/O — so hydration stays cheap.
pub fn is_external(path: &str, folder_id: Option<i64>, ctx: &ExternalCheck<'_>) -> bool {
    if path.is_empty() || folder_id.is_some() {
        return false;
    }
    let path = std::path::Path::new(path);
    if path == ctx.root
        || thegn_core::worktree::is_managed_checkout(ctx.root, ctx.repo_dir_name, path, ctx.cfg)
    {
        return false;
    }
    !ctx.roots.iter().any(|r| path.starts_with(r))
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
    let row = db_worktrees.iter().find(|w| w.tab_name == active_name);
    if let Some(fid) = row.and_then(|w| w.folder_id) {
        keys.push(format!("{slug}/folder:{fid}"));
    }
    // An external worktree lives under the derived "Other" group, which
    // `build_rows` already renders open while it holds the active row; the
    // key is reported so callers that clear collapse state see the whole
    // ancestry.
    if row.is_some_and(|w| w.external) {
        keys.push(other_group_key(&slug));
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
    ///
    /// **Not the manual-order key** — see [`Group::manual_rank`]. `gi` is also
    /// the group index [`RowTarget::Tab`] activates, so it must stay the
    /// session slot.
    gi: usize,
    /// The worktree's rank in the persistent registry (`db.worktrees()` order,
    /// i.e. its `position`), when it has a registry row. `None` = never
    /// registered — a legacy or purely in-session group — which sorts *before*
    /// registered rows, the same tier order `Session::resurrect` uses.
    ///
    /// This, not `gi`, is what [`SortMode::Manual`] keys on. `gi` conflates two
    /// independent sequences: live groups are numbered by their slot in
    /// `session.worktrees`, while rows reconstructed from the registry are
    /// numbered *after* all the live ones. A worktree that migrates between
    /// those two blocks — `Session::adopt_missing_registered` appends when you
    /// activate a dormant row, a failed create removes one — therefore
    /// re-sorted against its siblings even though its persisted `position`
    /// never changed. Ranking on the registry makes the manual order the same
    /// whether a worktree is live, dormant, warm-restored or cold-resurrected.
    manual_rank: Option<u32>,
    /// Worktree path — the key into the attention-rank map for the
    /// [`SortMode::Attention`] ordering (and into the per-worktree status maps).
    path: String,
    activity: ActivityState,
    sandbox_backend: Option<String>,
    env_name: Option<String>,
    sandbox_profile: Option<String>,
    /// The env is a provider but content is local (degraded) — drives the `✗` on
    /// the env badge. See [`DbWorktree::env_degraded`].
    env_degraded: bool,
    folder_id: Option<i64>,
    /// A worktree thegn did not create ([`DbWorktree::external`]); placed per
    /// `[ui] sidebar_external_worktrees`.
    external: bool,
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

/// The worktree row's second line. Composition lives in the pure detail module;
/// this wrapper preserves the sidebar module's existing call surface.
#[cfg(test)]
pub(crate) fn compose_detail_line(
    row: &SidebarRow,
    disp: &crate::sidebar_view::SidebarDisplay,
) -> Option<crate::seg::Line> {
    crate::sidebar_detail::compose_detail_line(row, disp)
}

/// The detail line's merge-queue chip for a status: the shared
/// [`thegn_core::attention::MqStatus::glyph`] vocabulary (also used by
/// `panel/sections/merge_queue.rs`), minus `Landed` — a finished row is panel
/// detail, not sidebar-worthy signal.
pub(crate) fn mq_chip(
    mq: thegn_core::attention::MqStatus,
) -> Option<(&'static str, thegn_core::theme::Hue)> {
    if mq == thegn_core::attention::MqStatus::Landed {
        return None;
    }
    Some(mq.glyph(crate::caps::active_glyphs()))
}

/// Whether a registry row is the Lead's (rendered in its own section).
fn is_lead_terminal(t: &thegn_core::models::TerminalRow) -> bool {
    t.kind == crate::lead::group::LEAD_TERMINAL_KIND
}

/// The pinned `LEAD` section: a banner plus one row. The row is a plain
/// terminal-target row, so the existing terminal activation ladder owns it.
fn push_lead_section(
    rows: &mut Vec<SidebarRow>,
    session: &Session,
    db_terminals: &[thegn_core::models::TerminalRow],
    lead: Option<&thegn_core::lead_bar::LeadBar>,
) {
    use crate::lead::group::LEAD_GROUP_NAME;
    rows.push(SidebarRow::base(RowKind::SectionHeading, 0, "LEAD", "lead"));
    let live = session
        .worktrees
        .iter()
        .position(|w| w.name == LEAD_GROUP_NAME && w.kind == crate::session::GroupKind::Terminal);
    let registered = crate::lead::group::lead_row(db_terminals).is_some();
    let mut label = match live {
        Some(i) => {
            let n = session.worktrees[i].tabs.len();
            format!("Lead ({n} {})", if n == 1 { "tab" } else { "tabs" })
        }
        None if registered => "Lead".to_string(),
        None => "Lead \u{2014} Enter to start".to_string(),
    };
    let state = lead.map(|bar| bar.state).filter(|state| {
        matches!(
            state,
            thegn_core::lead_bar::LeadState::Working
                | thegn_core::lead_bar::LeadState::WaitingOnYou
                | thegn_core::lead_bar::LeadState::Paused
                | thegn_core::lead_bar::LeadState::Unknown
                | thegn_core::lead_bar::LeadState::Error
        )
    });
    if (live.is_some() || registered)
        && let Some(state) = state
    {
        write!(
            label,
            " {} {}",
            crate::caps::active_glyphs().middot,
            state.word()
        )
        .expect("writing to String cannot fail");
    }
    let active = live.is_some_and(|i| i == session.active);
    rows.push(SidebarRow {
        tab_target: Some(match live {
            Some(i) => RowTarget::Tab(i, session.worktrees[i].active_tab),
            None => RowTarget::Workspace {
                repo_path: "terminal".into(),
                group: Some(LEAD_GROUP_NAME.into()),
            },
        }),
        active,
        worktree_path: Some(LEAD_GROUP_NAME.into()),
        terminal_connection: Some(String::new()),
        ..SidebarRow::base(RowKind::Terminal, 1, label, "lead/row")
    });
}

/// The label of the row that opens the Lead setup flow.
pub(crate) const LEAD_SETUP_LABEL: &str = "Lead \u{2014} set up";

/// The `LEAD` section while no `[lead]` is configured: one row whose activation
/// the loop turns into the in-app setup flow (never a Lead launch).
fn push_lead_setup_section(rows: &mut Vec<SidebarRow>) {
    use crate::lead::group::LEAD_GROUP_NAME;
    rows.push(SidebarRow::base(RowKind::SectionHeading, 0, "LEAD", "lead"));
    rows.push(SidebarRow {
        tab_target: Some(RowTarget::Workspace {
            repo_path: "terminal".into(),
            group: Some(LEAD_GROUP_NAME.into()),
        }),
        worktree_path: Some(LEAD_GROUP_NAME.into()),
        terminal_connection: Some(String::new()),
        ..SidebarRow::base(RowKind::Terminal, 1, LEAD_SETUP_LABEL, "lead/row")
    });
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
    for t in db_terminals.iter().filter(|t| !is_lead_terminal(t)) {
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

    // The Lead is pinned at the very top: outside workspace ordering/reorder.
    if view.lead_enabled {
        push_lead_section(&mut rows, session, db_terminals, status.lead.as_ref());
    } else if view.lead_setup_row {
        push_lead_setup_section(&mut rows);
    }

    // Every ordering key the computed sorts read, in one place: the live status
    // maps, or the frozen snapshot while the user is navigating the bar (see
    // `crate::sidebar_freeze`). Unarmed, this is exactly the live maps.
    let keys = crate::sidebar_freeze::SortKeys::new(view.freeze.as_deref(), status);

    // Workspace bubbling (`[ui] sidebar_workspace_sort = "attention"`): order
    // workspaces by their most-urgent worktree's tier. The sort is stable and
    // tier-granular, so equal-urgency workspaces keep their manual order and a
    // workspace only moves on a real tier change — hysteresis for free.
    // Routed through `keys` like the worktree sorts, or whole project blocks
    // would still slide under the cursor while their worktrees were held.
    let mut workspaces: Vec<&(String, String, String, String)> = workspaces.iter().collect();
    if view.workspace_sort == thegn_core::config::WorkspaceSort::Attention {
        workspaces.sort_by_key(|(slug, ..)| keys.workspace_tier(slug));
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
    // The persisted DB paths cover pipeline worktrees whose rows have aged out
    // of the bounded sidebar roster. Roster paths are included as a fallback
    // if the ownership query failed during hydration.
    let mut pipeline_paths: std::collections::HashSet<String> = status
        .pipeline_worktree_paths
        .iter()
        .cloned()
        .chain(status.pipeline_lanes.iter().flat_map(|lane| {
            lane.worktrees
                .iter()
                .map(|worktree| crate::sidebar_pipeline::path_identity(&worktree.path))
        }))
        .collect();
    // Native pipeline dispatches persist their reserved environment on the
    // worktree before their first dispatch row exists. That exact binding is
    // the ownership witness until durable stage metadata appears.
    let environment_paths: Vec<String> = db_worktrees
        .iter()
        .filter(|worktree| worktree.env_name.as_deref() == Some("pipeline"))
        .map(|worktree| worktree.path.clone())
        .filter(|path| !path.trim().is_empty())
        .collect();
    pipeline_paths.extend(
        environment_paths
            .iter()
            .map(|path| crate::sidebar_pipeline::path_identity(path)),
    );
    // Integration and release worktrees (`tg/bNNN-integration`, `-release`) are
    // not driven by any issue pipeline: they stay with the ordinary worktrees.
    pipeline_paths.retain(|path| !crate::sidebar_pipeline::is_integration_path(path));
    // An explicit user folder is an override to the default Pipelines filing.
    // Keep that worktree in its chosen folder while leaving all other rows and
    // folder state untouched. A user folder that is itself named `Pipelines`
    // is not such an override: a roster-owned worktree filed there joins the
    // one derived Pipelines section instead of a second group of that name.
    let pipelines_named: std::collections::HashSet<i64> = db_folders
        .iter()
        .filter(|folder| folder.name.trim().eq_ignore_ascii_case(PIPELINES_LABEL))
        .map(|folder| folder.folder_id)
        .collect();
    for worktree in db_worktrees.iter().filter(|worktree| {
        worktree
            .folder_id
            .is_some_and(|id| !pipelines_named.contains(&id))
    }) {
        pipeline_paths.remove(&crate::sidebar_pipeline::path_identity(&worktree.path));
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
            &keys,
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
    // registration for) gets a second chance from `dir_owner` below, and is
    // otherwise NOT emitted: a `Pipelines` folder only ever hangs under the
    // workspace that owns its worktrees. There is deliberately no top-level
    // fallback group — the whole point is that pipelines never surface at the
    // root of the tree — and the pipeline board (`Alt b`) remains the complete
    // view of the roster, including lanes no workspace claims. For the same
    // reason the flat layout, which has no workspace rows to nest under, emits
    // no pipeline rows at all.
    let mut lane_targets: std::collections::HashMap<String, (String, RowTarget)> =
        std::collections::HashMap::new();
    for (gi, g) in session.worktrees.iter().enumerate() {
        if let Some((repo, _)) = split_tab(&g.name) {
            lane_targets
                .entry(crate::sidebar_pipeline::path_identity(&g.path))
                .or_insert_with(|| (repo, RowTarget::Tab(gi, g.active_tab)));
        }
    }
    for w in db_worktrees {
        lane_targets
            .entry(crate::sidebar_pipeline::path_identity(&w.path))
            .or_insert_with(|| {
                (
                    w.slug.clone(),
                    RowTarget::Workspace {
                        repo_path: w.repo_path.clone(),
                        group: Some(w.tab_name.clone()),
                    },
                )
            });
    }
    // Second-chance attribution, consulted only when the direct lookup fails.
    // A pipeline creates its worktree as a SIBLING of the workspace's existing
    // ones, so the directory holding them names the workspace even when the new
    // path itself is in neither the session nor the DB (the case that used to
    // dump a lane in the top-level tail group). A directory two workspaces
    // share is ambiguous, and is dropped rather than guessed.
    let mut dir_owner: std::collections::HashMap<std::path::PathBuf, Option<&str>> =
        std::collections::HashMap::new();
    for (path, (slug, _)) in &lane_targets {
        if let Some(dir) = std::path::Path::new(path).parent() {
            dir_owner
                .entry(dir.to_path_buf())
                .and_modify(|owner| {
                    if *owner != Some(slug.as_str()) {
                        *owner = None;
                    }
                })
                .or_insert(Some(slug.as_str()));
        }
    }
    let mut lanes_by_ws: std::collections::BTreeMap<String, Vec<crate::sidebar_pipeline::Lane>> =
        std::collections::BTreeMap::new();
    let mut assigned_pipeline_paths = std::collections::HashSet::new();
    let environment_lanes =
        crate::sidebar_pipeline::lanes_with_persisted_paths(&[], &environment_paths);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let finished_window_ms = i64::from(view.display.finished_pipeline_days) * 86_400_000;
    for lane in status.pipeline_lanes.iter().chain(&environment_lanes) {
        let worktrees: Vec<_> = lane
            .worktrees
            .iter()
            .filter(|worktree| {
                let identity = crate::sidebar_pipeline::path_identity(&worktree.path);
                pipeline_paths.contains(&identity) && assigned_pipeline_paths.insert(identity)
            })
            .cloned()
            .collect();
        if worktrees.is_empty() {
            continue;
        }
        let lane = crate::sidebar_pipeline::Lane {
            worktrees,
            ..(*lane).clone()
        };
        let home = if view.flat {
            None
        } else {
            lane.worktrees
                .iter()
                .find_map(|w| lane_targets.get(&crate::sidebar_pipeline::path_identity(&w.path)))
                .map(|(slug, _)| slug.clone())
                .or_else(|| {
                    lane.worktrees.iter().find_map(|w| {
                        let identity = crate::sidebar_pipeline::path_identity(&w.path);
                        std::path::Path::new(&identity)
                            .parent()
                            .and_then(|dir| dir_owner.get(dir).copied().flatten())
                            .map(str::to_string)
                    })
                })
        };
        // A finished lane ages out of the sidebar after the configured window
        // (`[ui] sidebar_finished_pipeline_days`; `0` = never). Its worktree is
        // untouched; only an explicit `wt prune` removes it. The active
        // worktree's lane always stays. "Clear finished" (the `Finished`
        // row's menu) hides every lane finished by the time it was run.
        let cleared = home
            .as_deref()
            .and_then(|slug| view.finished_cleared.get(slug))
            .is_some_and(|&cut| lane.last_ms <= cut);
        let expired =
            finished_window_ms > 0 && now_ms.saturating_sub(lane.last_ms) > finished_window_ms;
        let aged_out = lane.finished
            && (expired || cleared)
            && !session.active_group().is_some_and(|group| {
                let active = crate::sidebar_pipeline::path_identity(&group.path);
                lane.worktrees
                    .iter()
                    .any(|w| crate::sidebar_pipeline::path_identity(&w.path) == active)
            });
        if aged_out {
            continue;
        }
        // No home ⇒ no rows. See the note above: there is no top-level group to
        // fall back to, by design.
        if let Some(slug) = home {
            lanes_by_ws.entry(slug).or_default().push(lane);
        }
    }

    for (repo_slug, display, kind, repo_path) in if view.flat { Vec::new() } else { workspaces } {
        let collapsed = view.collapsed.contains(repo_slug);
        rows.push(SidebarRow {
            // Workspace rows carry the repo path (not a worktree path) so the
            // remove-workspace action can resolve its DB target without a
            // slug→path lookup. Empty for live fallbacks with no DB row yet.
            worktree_path: (!repo_path.is_empty()).then(|| repo_path.clone()),
            zone: zone_tag(&status.zones, &status.zones_unpoliced, repo_path),
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
        let pipeline_groups: std::collections::HashMap<String, Group> = groups
            .iter()
            .filter(|group| {
                pipeline_paths.contains(&crate::sidebar_pipeline::path_identity(&group.path))
            })
            .map(|group| {
                (
                    crate::sidebar_pipeline::path_identity(&group.path),
                    group.clone(),
                )
            })
            .collect();

        // In the default project tree a pipeline-owned worktree has one home:
        // its Pipelines leaf. Explicit flat layout remains a flat list.
        if !view.flat {
            groups.retain(|group| {
                !pipeline_paths.contains(&crate::sidebar_pipeline::path_identity(&group.path))
            });
        }

        sort_groups(&mut groups, view.sort, &keys);

        // One shared worktree-row builder for both the loose (depth 1) and
        // filed (depth 2) placements, and for both live and dormant sources —
        // everything placement-specific (`depth`, `pin_key`) is an argument;
        // everything source-specific (`target`, `active`, `path`) rides on the
        // `Group`. This is the single render routine that keeps a dormant
        // workspace's tree identical to its live one.
        let mk_row = |gr: &Group, depth: u8, pin_key: String| -> SidebarRow {
            worktree_row(gr, status, repo_slug, depth, pin_key, !collapsed, None)
        };

        // External worktrees (`[ui] sidebar_external_worktrees`): `show` keeps
        // them among the loose rows, `group` moves them under the derived
        // "Other" row emitted after the folders, `hide` drops them. The active
        // worktree is never hidden: under `hide` it stays inline.
        let external_mode = view.display.external_worktrees;
        let inline = |g: &Group| match external_mode {
            ExternalWorktrees::Show => true,
            ExternalWorktrees::Group => !g.external,
            ExternalWorktrees::Hide => !g.external || g.active,
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
        //
        // Loose worktrees sharing a batch token fold into one derived
        // `BatchGroup` row per batch (`fold::plan_loose`); pinned worktrees
        // never join one. Single rows keep the sorted order; batch rows follow.
        let loose: Vec<&Group> = groups
            .iter()
            .filter(|g| {
                g.folder_id
                    .is_none_or(|fid| !rendered_folder_ids.contains(&fid))
                    && inline(g)
            })
            .collect();
        let branches: Vec<&str> = loose.iter().map(|gr| gr.label.as_str()).collect();
        let keep: Vec<bool> = loose
            .iter()
            .map(|gr| view.pins.contains(&format!("{repo_slug}/{}", gr.label)))
            .collect();
        let plan = fold::plan_loose(&branches, &keep, view.display.batch_group_min);
        for item in plan {
            match item {
                fold::LooseItem::Single(i) => {
                    let gr = loose[i];
                    let pin_key = format!("{repo_slug}/{}", gr.label);
                    rows.push(mk_row(gr, 1, pin_key));
                }
                fold::LooseItem::Batch { key, members } => {
                    let members: Vec<SidebarRow> = members
                        .into_iter()
                        .map(|i| {
                            let gr = loose[i];
                            let pin_key = format!("{repo_slug}/{}", gr.label);
                            mk_row(gr, 1, pin_key)
                        })
                        .collect();
                    fold::push_batch_group(&mut rows, repo_slug, &key, members, collapsed, view);
                }
            }
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
                if !view.flat
                    && pipeline_paths.contains(&crate::sidebar_pipeline::path_identity(&w.path))
                {
                    continue;
                }
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
                row.hide_subject = folder.name.trim().eq_ignore_ascii_case("merged");
                row.visible = !collapsed && !folder_collapsed;
                rows.push(row);
            }
        }

        if external_mode == ExternalWorktrees::Group {
            push_other_group(&mut rows, &groups, repo_slug, collapsed, view, &mk_row);
        }

        // The workspace's derived `Pipelines` folder (THE-74), at the tail
        // of its own tree: a lane lives under the workspace that owns its
        // worktrees, so a pipeline's spawns read as part of that repo — and
        // a worktree no roster row references stays exactly where it was.
        // Children are always emitted with `visible` tracking collapsed
        // ancestors (the folder precedent), so the filter can reveal a row
        // inside a collapsed group.
        if let Some(ws_lanes) = lanes_by_ws.get(repo_slug) {
            let active_path = session.active_group().map(|group| group.path.as_str());
            push_pipeline_group(
                &mut rows,
                ws_lanes,
                PipelineGroupContext {
                    group_slug: repo_slug,
                    parent_collapsed: collapsed,
                    view,
                    lane_targets: &lane_targets,
                    active_path,
                    pipeline_groups: &pipeline_groups,
                    status,
                },
            );
        }
    }

    if rows.is_empty() {
        rows.push(SidebarRow::base(RowKind::Workspace, 0, "no projects", ""));
    }

    // Nothing pipeline-shaped is emitted at depth 0. There used to be two such
    // rows here — a `Pipeline ▸ N running` roster rollup and a `Pipelines`
    // group for lanes no workspace claimed — and between them the tree grew a
    // second, top-level pipeline section competing with the per-workspace
    // folders. Both are gone: the rollup's job (open the board) belongs to its
    // keybind, `Alt b` / `Action::OpenPipelineBoard`, and the lanes belong
    // under their workspace or nowhere.

    // TERMINALS is a first-class, static category banner (a peer of the
    // "WORKSPACES" title), never collapsible and never a nav target. By
    // default it is always shown — even with no terminals — so the section
    // (and its "New terminal…" entry point) never silently vanishes;
    // `[ui] sidebar_terminals_section = "nonempty"` opts into hiding the
    // whole section until a terminal exists.
    let no_plain_terminals = db_terminals.iter().all(is_lead_terminal);
    let hide_terminals = no_plain_terminals
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
        if no_plain_terminals {
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
                    .map(|i| RowTarget::Tab(i, session.worktrees[i].active_tab));

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
            RowKind::Worktree | RowKind::PipelineWorktree => {
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
                row.ci_state = row
                    .worktree_path
                    .as_deref()
                    .and_then(|p| status.ci_states.get(p))
                    .copied();
                row.linked_issue = row
                    .worktree_path
                    .as_deref()
                    .and_then(|p| status.linked_issues.get(p))
                    .cloned();
            }
            RowKind::Workspace => {
                row.attention = status.workspace_attention.get(&row.workspace_slug).copied();
            }
            _ => {}
        }
    }

    // Roll up only the queue statuses of this workspace's child worktree rows.
    // Children are emitted even when their workspace is collapsed, so this
    // projection remains complete before visibility/filtering is applied.
    for index in 0..rows.len() {
        if rows[index].kind != RowKind::Workspace {
            continue;
        }
        let workspace_slug = &rows[index].workspace_slug;
        let mq_rollup = thegn_core::merge_queue_view::rollup(
            rows.iter()
                .filter(|child| child.kind.is_worktree() && child.workspace_slug == *workspace_slug)
                .filter_map(|child| child.mq_status),
        );
        rows[index].mq_rollup = mq_rollup;
    }

    fold::rollup_batch_stats(&mut rows);

    apply_pins(&mut rows, &view.pins);
    apply_row_limits(&mut rows, view);
    apply_filter(&mut rows, &view.filter);
    rows
}

/// The `pin_key` of the [`RowKind::ShowMore`] row closing the group whose
/// collapse key is `group_key`.
pub fn show_more_key(group_key: &str) -> String {
    format!("more:{group_key}")
}

/// The group key a [`RowKind::ShowMore`] row toggles (`None` for any other
/// row).
pub fn show_more_group(row: &SidebarRow) -> Option<&str> {
    if row.kind == RowKind::ShowMore {
        row.pin_key.strip_prefix("more:")
    } else {
        None
    }
}

/// The row-limit pass; see [`fold::apply_row_limits`].
fn apply_row_limits(rows: &mut Vec<SidebarRow>, view: &ViewState) {
    fold::apply_row_limits(rows, view)
}

/// Emit a workspace's derived "Other (N)" group and its external worktree
/// rows (in the workspace's sort order, at depth 2). Nothing when the
/// workspace has no external worktree. Collapsed per the persisted override
/// (`ViewState::other_open`), else `[ui] sidebar_other_group_collapsed` — but
/// rendered open for this frame while it holds the active worktree, so the
/// active row is never hidden. Children are always emitted (the folder
/// precedent) so the filter can reveal one inside the collapsed group.
fn push_other_group(
    rows: &mut Vec<SidebarRow>,
    groups: &[Group],
    repo_slug: &str,
    parent_collapsed: bool,
    view: &ViewState,
    mk_row: &dyn Fn(&Group, u8, String) -> SidebarRow,
) {
    let others: Vec<&Group> = groups.iter().filter(|g| g.external).collect();
    if others.is_empty() {
        return;
    }
    let holds_active = others.iter().any(|g| g.active);
    let open = view
        .other_open
        .get(repo_slug)
        .copied()
        .unwrap_or(!view.display.other_group_collapsed);
    let group_collapsed = !open && !holds_active;
    rows.push(SidebarRow {
        pin_key: other_group_key(repo_slug),
        visible: !parent_collapsed,
        collapsed: group_collapsed,
        child_count: others.len(),
        ..SidebarRow::base(
            RowKind::OtherGroup,
            1,
            view.display.other_group_label.clone(),
            repo_slug,
        )
    });
    for gr in others {
        let mut row = mk_row(gr, 2, format!("{repo_slug}/{}", gr.label));
        row.visible = !parent_collapsed && !group_collapsed;
        rows.push(row);
    }
}

/// Borrowed per-workspace state used to emit the derived Pipelines folder.
struct PipelineGroupContext<'a> {
    group_slug: &'a str,
    parent_collapsed: bool,
    view: &'a ViewState,
    lane_targets: &'a std::collections::HashMap<String, (String, RowTarget)>,
    active_path: Option<&'a str>,
    pipeline_groups: &'a std::collections::HashMap<String, Group>,
    status: &'a SidebarStatus,
}

/// Emit one derived `Pipelines` group: the folder head, each lane folder
/// named from the roster's issue id, and each lane's referenced worktrees as
/// leaves. These leaves are the primary sidebar rows for pipeline-owned
/// worktrees in the default grouped layout.
///
/// Lane rows come from the bounded roster plus durable path ownership. The
/// leaves resolve to the same targets as ordinary worktree rows, so opening
/// one keeps the existing keyboard, mouse, and action behavior.
///
/// Visibility follows the folder precedent: children are ALWAYS emitted and
/// only their `visible` flag tracks a collapsed ancestor, so the sidebar
/// filter can still find and reveal a row inside a collapsed group or lane.
fn push_pipeline_group(
    rows: &mut Vec<SidebarRow>,
    lanes: &[crate::sidebar_pipeline::Lane],
    context: PipelineGroupContext<'_>,
) {
    if lanes.is_empty() {
        return;
    }
    let group_key = format!("pipeline/group:{}", context.group_slug);
    let holds_active = context.active_path.is_some_and(|path| {
        let active = crate::sidebar_pipeline::path_identity(path);
        lanes.iter().any(|lane| {
            lane.worktrees
                .iter()
                .any(|worktree| crate::sidebar_pipeline::path_identity(&worktree.path) == active)
        })
    });
    let group_collapsed = context.view.collapsed.contains(&group_key) && !holds_active;
    rows.push(SidebarRow {
        pin_key: group_key,
        collapsed: group_collapsed,
        visible: !context.parent_collapsed,
        child_count: lanes.len(),
        ..SidebarRow::base(
            RowKind::PipelineGroup,
            1,
            PIPELINES_LABEL,
            context.group_slug,
        )
    });
    // A finished lane (every roster row terminal) moves into one `Finished`
    // sub-group so the live pipelines read first.
    let (finished, live): (Vec<_>, Vec<_>) = lanes.iter().partition(|lane| lane.finished);
    for lane in live {
        let lane_key = format!("pipeline/lane:{}", lane.key);
        // A single-worktree lane is one row: the worktree leaf carries the
        // lane's name instead of sitting under a header that repeats it.
        if let [wt] = lane.worktrees.as_slice() {
            let visible = !context.parent_collapsed && !group_collapsed;
            push_pipeline_leaf(rows, &context, wt, &lane.label, 2, visible);
            continue;
        }
        let lane_holds_active = context.active_path.is_some_and(|path| {
            let active = crate::sidebar_pipeline::path_identity(path);
            lane.worktrees
                .iter()
                .any(|worktree| crate::sidebar_pipeline::path_identity(&worktree.path) == active)
        });
        let lane_collapsed = context.view.collapsed.contains(&lane_key) && !lane_holds_active;
        rows.push(SidebarRow {
            pin_key: lane_key,
            collapsed: lane_collapsed,
            visible: !context.parent_collapsed && !group_collapsed,
            child_count: lane.worktrees.len(),
            ..SidebarRow::base(
                RowKind::PipelineLane,
                2,
                lane.label.clone(),
                context.group_slug,
            )
        });
        for wt in &lane.worktrees {
            let visible = !context.parent_collapsed && !group_collapsed && !lane_collapsed;
            push_pipeline_leaf(rows, &context, wt, &wt.name, 3, visible);
        }
    }
    if !finished.is_empty() {
        let key = finished_group_key(context.group_slug);
        let holds_active = context.active_path.is_some_and(|path| {
            let active = crate::sidebar_pipeline::path_identity(path);
            finished.iter().any(|lane| {
                lane.worktrees.iter().any(|worktree| {
                    crate::sidebar_pipeline::path_identity(&worktree.path) == active
                })
            })
        });
        let collapsed = context.view.collapsed.contains(&key) && !holds_active;
        rows.push(SidebarRow {
            pin_key: key,
            collapsed,
            visible: !context.parent_collapsed && !group_collapsed,
            child_count: finished.len(),
            ..SidebarRow::base(RowKind::PipelineLane, 2, "Finished", context.group_slug)
        });
        let visible = !context.parent_collapsed && !group_collapsed && !collapsed;
        for lane in finished {
            for wt in &lane.worktrees {
                let label = if lane.worktrees.len() == 1 {
                    lane.label.as_str()
                } else {
                    wt.name.as_str()
                };
                push_pipeline_leaf(rows, &context, wt, label, 3, visible);
            }
        }
    }
}

/// Emit one pipeline-owned worktree leaf at `depth`, labelled `label`.
fn push_pipeline_leaf(
    rows: &mut Vec<SidebarRow>,
    context: &PipelineGroupContext<'_>,
    wt: &crate::sidebar_pipeline::LaneWorktree,
    label: &str,
    depth: u8,
    visible: bool,
) {
    let hit = context
        .lane_targets
        .get(&crate::sidebar_pipeline::path_identity(&wt.path));
    let identity = crate::sidebar_pipeline::path_identity(&wt.path);
    let mut row = context
        .pipeline_groups
        .get(&identity)
        .map(|group| {
            worktree_row(
                group,
                context.status,
                context.group_slug,
                depth,
                format!("pipeline/worktree:{identity}"),
                visible,
                None,
            )
        })
        .unwrap_or_else(|| {
            SidebarRow::base(
                RowKind::PipelineWorktree,
                depth,
                wt.name.clone(),
                context.group_slug,
            )
        });
    row.kind = RowKind::PipelineWorktree;
    row.pin_key = format!("{}/{}", context.group_slug, row.label);
    row.label = label.to_string();
    row.visible = visible;
    row.worktree_path = Some(wt.path.clone());
    row.tab_target = hit.map(|(_, target)| target.clone());
    rows.push(row);
}

/// Build the unsorted worktree `Group` list for one workspace: the **union** of
/// the session's live groups and the DB registry, never one source or the other.
///
/// Live groups come first (live `Tab` targets + real active flag), then every
/// registered worktree of this slug the session does not already carry — `home`
/// first, then each registered non-home worktree in position order, each with a
/// `Workspace` switch target. A *dormant* workspace (parked into the
/// `WorkspacePool` when another became active) has no session slots at all, so
/// it reconstructs entirely from the registry — the same shape it renders live.
///
/// The union is the point: git/the registry is the source of truth for which
/// worktrees exist, so a registered worktree is never invisible merely because
/// the session missed it at resurrect time (THE-73 — a real, git-listed
/// worktree that `Session::resurrect` failed to adopt used to vanish the moment
/// the workspace went live, and come back when it went dormant). An adoption
/// miss now degrades to "the row switches the workspace instead of focusing a
/// live tab", not "the row disappeared".
///
/// Both the grouped and the flat `build_rows` paths call this, so the tree never
/// rearranges just because a different workspace is active.
fn gather_groups(
    session: &Session,
    repo_slug: &str,
    repo_path: &str,
    activity: &std::collections::BTreeMap<String, ActivityState>,
    db_by_tab: &std::collections::HashMap<&str, &DbWorktree>,
    db_by_slug: &std::collections::HashMap<&str, Vec<&DbWorktree>>,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    // What the live session already covers, so the registry fill below can skip
    // it. A `Group` doesn't retain its tab name, so collect it here while we
    // still have `g.name`. Paths are the second key: a group renamed in-session
    // keeps its path, so matching on it too stops a duplicate row. Empty paths
    // never key anything (they'd collide with every other pathless row).
    let mut live_tabs: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut live_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
    // The registry's own order for this repo, keyed by tab name — the stable
    // manual-order rank (see `Group::manual_rank`). Keyed on the tab name and
    // not the path because that is the identity BOTH sources carry here, and it
    // survives a checkout being moved on disk. Built once; the live loop and the
    // registry fill below both stamp from it, which is the whole point: the two
    // blocks end up on ONE axis.
    let slug_rows_for_rank = db_by_slug
        .get(repo_slug)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let rank_by_tab: std::collections::HashMap<&str, u32> = slug_rows_for_rank
        .iter()
        .enumerate()
        .map(|(i, w)| (w.tab_name.as_str(), i as u32))
        .collect();
    for (gi, g) in session.worktrees.iter().enumerate() {
        let Some((repo, branch)) = split_tab(&g.name) else {
            continue;
        };
        if repo != repo_slug {
            continue;
        }
        live_tabs.insert(g.name.as_str());
        if !g.path.is_empty() {
            live_paths.insert(g.path.as_str());
        }
        let dbw = db_by_tab.get(g.name.as_str());
        groups.push(Group {
            label: branch,
            gi,
            manual_rank: rank_by_tab.get(g.name.as_str()).copied(),
            path: g.path.clone(),
            sandbox_backend: dbw.and_then(|w| w.sandbox_backend.clone()),
            env_name: dbw.and_then(|w| w.env_name.clone()),
            sandbox_profile: dbw.and_then(|w| w.sandbox_profile.clone()),
            env_degraded: dbw.is_some_and(|w| w.env_degraded),
            activity: activity.get(&g.name).copied().unwrap_or_default(),
            folder_id: dbw.and_then(|w| w.folder_id),
            external: dbw.is_some_and(|w| w.external),
            target: RowTarget::Tab(gi, g.active_tab),
            active: gi == session.active,
        });
    }
    // Fill from the DB registry: `home` first (pull its folder/backend/env from
    // the `home` DB row when present, else default), then every registered
    // non-home worktree the live loop above didn't already emit. A *dormant*
    // workspace has no live groups, so this reconstructs the whole tree; a live
    // one gets only the rows the session missed.
    //
    // The `repo_path` guard stays: a *live-fallback* workspace entry carries an
    // empty `repo_path` (`hydrate.rs`), so it has no switch target to hand a
    // synthetic row — it must contribute none.
    if !repo_path.is_empty() {
        let slug_rows = db_by_slug
            .get(repo_slug)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let db_home = slug_rows
            .iter()
            .find(|w| is_home_tab(repo_slug, &w.tab_name));
        // `gi` is a sort tie-break only (see [`Group::gi`]). Live groups hold
        // real session indices, so the appended rows are numbered after the
        // highest of them — an appended row then sorts after its live siblings
        // under `SortMode::Manual`. With no live groups this is the historical
        // dormant numbering, unchanged: `home` = 0, then 1, 2, ….
        let mut next_gi = groups.iter().map(|g| g.gi).max().map_or(0, |m| m + 1);
        let home_tab = thegn_core::repo::home_tab(repo_slug);
        if !live_tabs.contains(home_tab.as_str()) {
            groups.push(Group {
                label: "home".into(),
                gi: next_gi,
                manual_rank: rank_by_tab.get(home_tab.as_str()).copied(),
                path: repo_path.to_string(),
                sandbox_backend: db_home.and_then(|w| w.sandbox_backend.clone()),
                env_name: db_home.and_then(|w| w.env_name.clone()),
                sandbox_profile: db_home.and_then(|w| w.sandbox_profile.clone()),
                env_degraded: db_home.is_some_and(|w| w.env_degraded),
                // Keyed by tab name, same source the live rows use — so a
                // workspace you switched away from keeps its activity dot.
                activity: activity.get(home_tab.as_str()).copied().unwrap_or_default(),
                folder_id: db_home.and_then(|w| w.folder_id),
                // The home checkout is never external.
                external: false,
                target: RowTarget::Workspace {
                    repo_path: repo_path.to_string(),
                    group: Some(home_tab.clone()),
                },
                active: false,
            });
            next_gi += 1;
        }
        for w in slug_rows
            .iter()
            .filter(|w| !is_home_tab(repo_slug, &w.tab_name))
        {
            if live_tabs.contains(w.tab_name.as_str())
                || (!w.path.is_empty() && live_paths.contains(w.path.as_str()))
            {
                continue;
            }
            groups.push(Group {
                // The tab, not the branch, is the identity — the live path
                // labels the same worktree by `split_tab` too, so a branch
                // named `home` (tab `{slug}/home~`) never claims the home
                // row's label, pin key, or sort tier.
                label: split_tab(&w.tab_name).map_or_else(|| w.branch.clone(), |(_, b)| b),
                gi: next_gi,
                manual_rank: rank_by_tab.get(w.tab_name.as_str()).copied(),
                path: w.path.clone(),
                sandbox_backend: w.sandbox_backend.clone(),
                env_name: w.env_name.clone(),
                sandbox_profile: w.sandbox_profile.clone(),
                env_degraded: w.env_degraded,
                activity: activity
                    .get(w.tab_name.as_str())
                    .copied()
                    .unwrap_or_default(),
                folder_id: w.folder_id,
                external: w.external,
                target: RowTarget::Workspace {
                    repo_path: repo_path.to_string(),
                    group: Some(w.tab_name.clone()),
                },
                active: false,
            });
            next_gi += 1;
        }
    }
    groups
}

/// Whether a registry tab carries the workspace's stable home identity.
/// Branch names are mutable checkout state, while the canonical home tab is
/// fixed for the workspace and cannot alias a real branch named `home`.
pub(crate) fn is_home_tab(repo_slug: &str, tab_name: &str) -> bool {
    tab_name == thegn_core::repo::home_tab(repo_slug)
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
    // `plugin_row_sync::project_plugin_fields` keys this map by worktree path
    // only (a plugin's branch-name key is resolved to the path there).
    let plugin_fields = wt_path
        .as_deref()
        .and_then(|p| status.plugin_fields.get(p))
        .cloned()
        .unwrap_or_default();
    let branch_meta = wt_path
        .as_deref()
        .and_then(|p| status.branch_meta.get(p))
        .cloned();
    let devcontainer_status = wt_path
        .as_deref()
        .and_then(|p| status.devcontainer_status.get(p))
        .cloned();
    SidebarRow {
        tab_target: Some(gr.target.clone()),
        active: gr.active,
        worktree_path: wt_path,
        pin_key,
        branch: Some(live_branch.unwrap_or_else(|| gr.label.clone())),
        branch_meta,
        git,
        sandbox_backend: gr.sandbox_backend.clone(),
        env_name: gr.env_name.clone(),
        sandbox_profile: gr.sandbox_profile.clone(),
        env_degraded: gr.env_degraded,
        devcontainer_status,
        activity: gr.activity,
        visible,
        pr_count,
        pr_number,
        unread_count,
        alert_count,
        repo_prefix,
        plugin_fields,
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
#[allow(clippy::too_many_arguments)]
fn build_rows_flat(
    rows: &mut Vec<SidebarRow>,
    session: &Session,
    workspaces: &[&(String, String, String, String)],
    view: &ViewState,
    status: &SidebarStatus,
    keys: &crate::sidebar_freeze::SortKeys<'_>,
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
        let mut gs = gather_groups(
            session,
            slug,
            repo_path,
            &status.activity,
            db_by_tab,
            db_by_slug,
        );
        // Order each repo's groups BEFORE pooling. `gather_groups` emits live
        // groups first and registry-only ones after, which is not an order the
        // user chose — and `sort_groups_flat`'s Manual arm is a deliberate
        // no-op ("keep the gathered interleave"), so without this the flat view
        // kept the very live-vs-dormant instability the grouped view just lost.
        // For the other modes the global pass below is a stable sort that
        // simply overrides this, so the result is unchanged.
        if view.sort == SortMode::Manual {
            sort_groups(&mut gs, view.sort, keys);
        }
        // `hide` drops external worktrees here too (the active one stays);
        // `show` and `group` both list them, tagged with their repo as usual.
        let hide = view.display.external_worktrees == ExternalWorktrees::Hide;
        for g in gs {
            if hide && g.external && !g.active {
                continue;
            }
            pool.push((slug.clone(), display.clone(), g));
        }
    }

    sort_groups_flat(&mut pool, view.sort, keys);

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
    keys: &crate::sidebar_freeze::SortKeys<'_>,
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
                let r = |g: &Group| keys.rank(&g.path);
                r(&a.2).cmp(&r(&b.2))
            });
        }
        SortMode::Live => {
            pool.sort_by(|a, b| {
                let t = |g: &Group| keys.recency(&g.path);
                t(&b.2)
                    .partial_cmp(&t(&a.2))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
}

fn sort_groups(groups: &mut [Group], sort: SortMode, keys: &crate::sidebar_freeze::SortKeys<'_>) {
    match sort {
        SortMode::Manual => {
            // "home" first, then the REGISTRY order (`manual_rank`) — the
            // persisted `position` sequence that Shift+↑/↓ rearranges. `gi` is
            // only the last tie-break, because it is not one axis: a live group
            // is numbered by its session slot while a registry-reconstructed row
            // is numbered after every live one, so keying on it re-sorted a
            // worktree whenever it crossed between the two (activating a dormant
            // row appends it to the session; a failed create removes it) even
            // though its position never moved.
            //
            // Unregistered groups (`None`) sort ahead of registered ones,
            // matching the tier order `Session::resurrect` uses, so the tree
            // reads the same warm, cold, live and dormant.
            groups.sort_by_key(|a| {
                (
                    a.label != "home",
                    a.manual_rank.is_some(),
                    a.manual_rank.unwrap_or(0),
                    a.gi,
                )
            });
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
                let r = |g: &Group| keys.rank(&g.path);
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
                let t = |g: &Group| keys.recency(&g.path);
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
                last_lane = None;
            }
            // The derived "Other" group nests its rows at depth 2 exactly like
            // a folder, so the filter surfaces and reveals it the same way.
            RowKind::Folder | RowKind::OtherGroup | RowKind::BatchGroup => {
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
            RowKind::PipelineWorktree => {
                // A one-row lane sits directly under its group (depth 2):
                // the previous lane folder is not its parent.
                if rows[i].depth <= 2 {
                    last_lane = None;
                }
                if keep[i] {
                    if let Some(w) = last_workspace {
                        keep[w] = true;
                    }
                    if let Some(g) = last_group {
                        keep[g] = true;
                    }
                    if let Some(l) = last_lane {
                        keep[l] = true;
                    }
                }
            }
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
            RowKind::EmptyHint | RowKind::ShowMore => {}
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
                reveal_lane = false;
            }
            RowKind::Folder | RowKind::OtherGroup | RowKind::BatchGroup => {
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
            RowKind::PipelineWorktree => {
                if rows[i].depth <= 2 {
                    reveal_lane = false; // a one-row lane: no lane folder above it
                }
                if reveal_ws || reveal_group || reveal_lane {
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
            RowKind::EmptyHint | RowKind::ShowMore => {}
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

    type WorkspaceFixtureRows = Vec<(String, String, String, String)>;

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
    fn worktree_row_carries_branch_meta() {
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
        status.branch_meta.insert(
            "/wt/a".into(),
            BranchMeta {
                ahead_base: Some(5),
                ..BranchMeta::default()
            },
        );
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
        assert_eq!(
            row.branch_meta.as_ref().and_then(|meta| meta.ahead_base),
            Some(5)
        );
    }

    #[test]
    fn worktree_row_carries_linked_issue_by_path() {
        let s = session(
            vec![
                tab("app/tg-linked", "/wt/linked"),
                tab("app/tg-bare", "/wt/bare"),
            ],
            0,
        );
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let mut status = no_activity();
        status
            .linked_issues
            .insert("/wt/linked".into(), ("ABC-9".into(), 3));
        let rows = build_rows(&s, &ws, &ViewState::default(), &status, &[], &[], &[]);
        let linked = |path: &str| {
            rows.iter()
                .find(|r| r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some(path))
                .expect("worktree row")
                .linked_issue
                .clone()
        };
        assert_eq!(linked("/wt/linked"), Some(("ABC-9".to_string(), 3_usize)));
        assert_eq!(linked("/wt/bare"), None);
    }

    #[test]
    fn worktree_row_carries_ci_state_by_path() {
        let s = session(
            vec![tab("app/tg-red", "/wt/red"), tab("app/tg-none", "/wt/none")],
            0,
        );
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let mut status = no_activity();
        status.ci_states.insert("/wt/red".into(), CiBadge::Fail);
        let rows = build_rows(&s, &ws, &ViewState::default(), &status, &[], &[], &[]);
        let ci = |path: &str| {
            rows.iter()
                .find(|r| r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some(path))
                .expect("worktree row")
                .ci_state
        };
        assert_eq!(ci("/wt/red"), Some(CiBadge::Fail));
        assert_eq!(ci("/wt/none"), None);
    }

    #[test]
    fn worktree_row_looks_up_plugin_fields_by_path_only() {
        let s = session(vec![tab("app/tg-old", "/wt/a")], 0);
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let value = |text: &str| PluginFieldValue {
            plugin: "ci".into(),
            field: "status".into(),
            text: text.into(),
            role: thegn_core::plugin_api::RowRole::Good,
            slot: thegn_core::plugin_api::RowFieldSlot::Detail,
        };
        let find = |rows: &[SidebarRow]| {
            rows.iter()
                .find(|r| {
                    r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some("/wt/a")
                })
                .expect("worktree row")
                .plugin_fields
                .clone()
        };

        // A branch-name or tab-label key in the status map no longer resolves.
        let mut status = no_activity();
        status.branches.insert("/wt/a".into(), "tg/live".into());
        status
            .plugin_fields
            .insert("tg/live".into(), vec![value("by-branch")]);
        status
            .plugin_fields
            .insert("tg-old".into(), vec![value("by-label")]);
        let rows = build_rows(&s, &ws, &ViewState::default(), &status, &[], &[], &[]);
        assert!(find(&rows[..]).is_empty());

        // The path key does.
        status
            .plugin_fields
            .insert("/wt/a".into(), vec![value("by-path")]);
        let rows = build_rows(&s, &ws, &ViewState::default(), &status, &[], &[], &[]);
        assert_eq!(find(&rows[..]), vec![value("by-path")]);
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

    #[test]
    fn workspace_rows_roll_up_only_their_children_even_when_collapsed() {
        let s = session(
            vec![
                tab("alpha/queued", "/wt/alpha-queued"),
                tab("alpha/folding", "/wt/alpha-folding"),
                tab("beta/blocked", "/wt/beta-blocked"),
            ],
            0,
        );
        let ws = vec![
            (
                "alpha".to_string(),
                "alpha".to_string(),
                "repo".to_string(),
                "/repos/alpha".to_string(),
            ),
            (
                "beta".to_string(),
                "beta".to_string(),
                "repo".to_string(),
                "/repos/beta".to_string(),
            ),
        ];
        let view = ViewState {
            collapsed: ["alpha".to_string()].into_iter().collect(),
            ..ViewState::default()
        };
        let mut status = no_activity();
        status.mq.insert(
            "/wt/alpha-queued".into(),
            thegn_core::attention::MqStatus::Queued,
        );
        status.mq.insert(
            "/wt/alpha-folding".into(),
            thegn_core::attention::MqStatus::Folding,
        );
        status.mq.insert(
            "/wt/beta-blocked".into(),
            thegn_core::attention::MqStatus::NeedsHuman,
        );

        let rows = build_rows(&s, &ws, &view, &status, &[], &[], &[]);
        let alpha = rows
            .iter()
            .find(|row| row.kind == RowKind::Workspace && row.workspace_slug == "alpha")
            .expect("alpha workspace row");
        assert_eq!(
            alpha.mq_rollup,
            Some(thegn_core::merge_queue_view::MqRollup {
                tier: thegn_core::merge_queue_view::MqTier::Working,
                count: 1,
            })
        );
        assert!(alpha.collapsed);

        let beta = rows
            .iter()
            .find(|row| row.kind == RowKind::Workspace && row.workspace_slug == "beta")
            .expect("beta workspace row");
        assert_eq!(
            beta.mq_rollup,
            Some(thegn_core::merge_queue_view::MqRollup {
                tier: thegn_core::merge_queue_view::MqTier::Blocked,
                count: 1,
            })
        );
    }

    #[test]
    fn workspace_rows_keep_empty_and_landed_only_queues_silent() {
        let s = session(
            vec![
                tab("alpha/landed", "/wt/landed"),
                tab("beta/none", "/wt/none"),
            ],
            0,
        );
        let ws = vec![
            (
                "alpha".to_string(),
                "alpha".to_string(),
                "repo".to_string(),
                "/repos/alpha".to_string(),
            ),
            (
                "beta".to_string(),
                "beta".to_string(),
                "repo".to_string(),
                "/repos/beta".to_string(),
            ),
        ];
        let mut status = no_activity();
        status
            .mq
            .insert("/wt/landed".into(), thegn_core::attention::MqStatus::Landed);

        let rows = build_rows(&s, &ws, &ViewState::default(), &status, &[], &[], &[]);
        for slug in ["alpha", "beta"] {
            let row = rows
                .iter()
                .find(|row| row.kind == RowKind::Workspace && row.workspace_slug == slug)
                .expect("workspace row");
            assert_eq!(row.mq_rollup, None);
        }
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

    fn bar_with(state: thegn_core::lead_bar::LeadState) -> thegn_core::lead_bar::LeadBar {
        thegn_core::lead_bar::LeadBar {
            state,
            tabs: 1,
            approvals: Some(0),
            first_request: None,
            lanes: None,
            spawns: None,
            last: None,
            last_age: None,
            error: None,
        }
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
                "school/home",
                GroupKind::Home,
                "/repos/SCHOOL",
            )],
            0,
        );
        let ws = vec![(
            "school".to_string(),
            "SCHOOL".to_string(),
            "repo".to_string(),
            "/repos/SCHOOL".to_string(),
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
            sandbox_profile: None,
            env_degraded: false,
            external: false,
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

    fn db_row(slug: &str, branch: &str) -> DbWorktree {
        DbWorktree {
            slug: slug.into(),
            branch: branch.into(),
            repo_path: format!("/repos/{slug}"),
            tab_name: format!("{slug}/{branch}"),
            path: format!("/wt/{branch}"),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            env_degraded: false,
            sandbox_profile: None,
            external: false,
        }
    }

    /// Worktree `(label, depth)` pairs `build_rows` emits for the `app` workspace,
    /// in display order (order is part of the contract, so never sorted here).
    fn app_worktree_rows(s: &Session, dbw: &[DbWorktree]) -> Vec<(String, u8)> {
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        build_rows(s, &ws, &ViewState::default(), &no_activity(), dbw, &[], &[])
            .into_iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| (r.label, r.depth))
            .collect()
    }

    fn ghost_fixture() -> (Session, Vec<DbWorktree>) {
        // The session still carries `ghost` (removed from the DB) and misses
        // `newer` (registered).
        let mut feat = db_row("app", "feat");
        feat.path = "/wt/feat".into();
        let mut newer = db_row("app", "newer");
        newer.path = "/wt/newer".into();
        let live = session(
            vec![
                tab("app/home", "/repos/app"),
                tab("app/feat", "/wt/feat"),
                tab("app/ghost", "/wt/ghost"),
            ],
            0,
        );
        (live, vec![feat, newer])
    }

    #[test]
    fn live_and_dormant_rows_match_when_registry_dropped_a_worktree() {
        let (mut live, dbw) = ghost_fixture();
        let removed = vec!["app/ghost".to_string()];
        crate::workspace_switch_prep::drop_removed_placeholders(&mut live, &removed, |_| false);
        let dormant = session(vec![], 0);
        let live_rows = app_worktree_rows(&live, &dbw);
        assert_eq!(live_rows, app_worktree_rows(&dormant, &dbw));
        // THE-73: the registered worktree the session missed still renders live.
        assert!(live_rows.iter().any(|(l, _)| l == "newer"));
    }

    #[test]
    fn removed_worktree_is_never_rendered_live_or_dormant() {
        let (mut live, dbw) = ghost_fixture();
        let removed = vec!["app/ghost".to_string()];
        crate::workspace_switch_prep::drop_removed_placeholders(&mut live, &removed, |_| false);
        for s in [live, session(vec![], 0)] {
            assert!(
                !app_worktree_rows(&s, &dbw)
                    .iter()
                    .any(|(l, _)| l == "ghost")
            );
        }
    }

    #[test]
    fn live_group_with_panes_but_no_registry_row_still_renders() {
        // A running agent in a worktree the registry does not list (quarantined
        // tab, failed best-effort registration, background add) stays reachable.
        let (mut live, dbw) = ghost_fixture();
        let removed = vec!["app/ghost".to_string()];
        crate::workspace_switch_prep::drop_removed_placeholders(&mut live, &removed, |_| true);
        assert!(
            app_worktree_rows(&live, &dbw)
                .iter()
                .any(|(l, _)| l == "ghost")
        );
    }

    #[test]
    fn dormant_groups_use_tab_identity_for_home_and_branch_named_home() {
        let mut home = db_row("app", "main");
        home.tab_name = "app/home".into();
        home.path = "/wt/home-main".into();
        let mut branch_home = db_row("app", "home");
        branch_home.tab_name = "app/home~".into();
        branch_home.path = "/wt/branch-home".into();
        let rows = [home, branch_home];
        let row_refs: Vec<_> = rows.iter().collect();
        let db_by_slug = std::collections::HashMap::from([("app", row_refs)]);
        let groups = gather_groups(
            &Session {
                id: "s".into(),
                worktrees: Vec::new(),
                active: 0,
            },
            "app",
            "/repos/app",
            &std::collections::BTreeMap::new(),
            &std::collections::HashMap::new(),
            &db_by_slug,
        );

        assert_eq!(
            groups
                .iter()
                .filter(|group| {
                    matches!(
                        &group.target,
                        RowTarget::Workspace { group: Some(tab), .. } if tab == "app/home"
                    )
                })
                .count(),
            1,
            "the home checkout on main must produce one canonical home row"
        );
        assert!(
            groups.iter().any(|group| {
                matches!(
                    &group.target,
                    RowTarget::Workspace { group: Some(tab), .. } if tab == "app/home~"
                ) && group.path == "/wt/branch-home"
            }),
            "an ordinary worktree on branch `home` must remain visible"
        );
        let branch_home = groups
            .iter()
            .find(|group| group.path == "/wt/branch-home")
            .expect("branch-home row");
        assert_eq!(
            branch_home.label, "home~",
            "the branch named `home` keeps its tab identity, not the home label"
        );
        assert_eq!(
            groups.iter().filter(|group| group.label == "home").count(),
            1,
            "exactly one row in the workspace may carry the home label"
        );
    }

    /// The registry order `a, b, c`, and a workspace that owns it.
    fn manual_fixture() -> (WorkspaceFixtureRows, Vec<DbWorktree>, SidebarStatus) {
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let dbw = vec![
            db_row("app", "home"),
            db_row("app", "a"),
            db_row("app", "b"),
            db_row("app", "c"),
        ];
        (ws, dbw, no_activity())
    }

    /// Manual order must be a property of the REGISTRY, not of which worktrees
    /// happen to be loaded. Activating a dormant row makes
    /// `Session::adopt_missing_registered` append it to `session.worktrees`;
    /// keyed on `gi` that append jumped the row up the list permanently, because
    /// live groups are numbered ahead of every registry-reconstructed one.
    #[test]
    fn manual_order_survives_a_dormant_row_being_adopted() {
        let (ws, dbw, status) = manual_fixture();
        let view = ViewState::default();

        // Two live, two still dormant.
        let before = session(vec![tab("app/home", "/wt/home"), tab("app/a", "/wt/a")], 0);
        assert_eq!(
            labels_of(&build_rows(&before, &ws, &view, &status, &dbw, &[], &[])),
            vec!["home", "a", "b", "c"],
        );

        // The user activates `c`; the session APPENDS it (session.rs asserts
        // that append is deliberate). Its registry position did not move, so
        // neither may its row.
        let after = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/a", "/wt/a"),
                tab("app/c", "/wt/c"),
            ],
            2,
        );
        assert_eq!(
            labels_of(&build_rows(&after, &ws, &view, &status, &dbw, &[], &[])),
            vec!["home", "a", "b", "c"],
            "adopting `c` must not move it ahead of `b`"
        );
    }

    /// The same registry must render the same order whether the workspace is
    /// fully loaded or fully dormant — otherwise the manual order silently
    /// depends on how you arrived at the repo (warm switch vs cold resurrect).
    #[test]
    fn manual_order_is_identical_live_and_dormant() {
        let (ws, dbw, status) = manual_fixture();
        let view = ViewState::default();

        let live = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/a", "/wt/a"),
                tab("app/b", "/wt/b"),
                tab("app/c", "/wt/c"),
            ],
            0,
        );
        let dormant = session(vec![], 0);
        assert_eq!(
            labels_of(&build_rows(&live, &ws, &view, &status, &dbw, &[], &[])),
            labels_of(&build_rows(&dormant, &ws, &view, &status, &dbw, &[], &[])),
        );
        // And it is the registry's own order, not the session's.
        assert_eq!(
            labels_of(&build_rows(&live, &ws, &view, &status, &dbw, &[], &[])),
            vec!["home", "a", "b", "c"],
        );
    }

    /// A live group with no registry row keeps its session slot and sorts ahead
    /// of the registered ones — the tier order `Session::resurrect` uses, so an
    /// unregistered worktree never vanishes into the tail.
    #[test]
    fn manual_order_keeps_unregistered_groups_ahead_of_registered_ones() {
        let (ws, dbw, status) = manual_fixture();
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/scratch", "/wt/scratch"), // no DB row
                tab("app/c", "/wt/c"),
            ],
            0,
        );
        assert_eq!(
            labels_of(&build_rows(
                &s,
                &ws,
                &ViewState::default(),
                &status,
                &dbw,
                &[],
                &[]
            )),
            vec!["home", "scratch", "a", "b", "c"],
        );
    }

    /// The flat view gathers the same live-then-dormant sequence, and its own
    /// Manual arm is a no-op, so it needs the per-repo pass to be stable too.
    #[test]
    fn flat_manual_order_survives_a_dormant_row_being_adopted() {
        let (ws, dbw, status) = manual_fixture();
        let view = ViewState {
            flat: true,
            ..Default::default()
        };
        let before = session(vec![tab("app/home", "/wt/home"), tab("app/a", "/wt/a")], 0);
        assert_eq!(
            labels_of(&build_rows(&before, &ws, &view, &status, &dbw, &[], &[])),
            vec!["home", "a", "b", "c"],
        );
        let after = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/a", "/wt/a"),
                tab("app/c", "/wt/c"),
            ],
            2,
        );
        assert_eq!(
            labels_of(&build_rows(&after, &ws, &view, &status, &dbw, &[], &[])),
            vec!["home", "a", "b", "c"],
        );
    }

    /// Fixture for the freeze tests: three worktrees, `urgent` most recent.
    fn freeze_fixture() -> (crate::session::Session, WorkspaceFixtureRows, SidebarStatus) {
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
        (s, ws, status)
    }

    fn labels_of(rows: &[SidebarRow]) -> Vec<String> {
        rows.iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.clone())
            .collect()
    }

    /// The whole point: while the freeze is armed, a re-rank on the hydration
    /// thread cannot move a row out from under the user's cursor.
    #[test]
    fn frozen_live_recency_keeps_the_order_across_a_rerank() {
        let (s, ws, status) = freeze_fixture();
        let freeze = std::sync::Arc::new(crate::sidebar_freeze::SortFreeze::capture(&status));

        // The world moves on: `calm` becomes the most recently active.
        let mut fresh = status.clone();
        fresh.activity_recency.insert("/wt/calm".into(), 900.0);

        let frozen_view = ViewState {
            sort: SortMode::Live,
            freeze: Some(freeze),
            ..Default::default()
        };
        assert_eq!(
            labels_of(&build_rows(&s, &ws, &frozen_view, &fresh, &[], &[], &[])),
            vec!["urgent", "home", "calm"],
            "a frozen order must survive a re-rank"
        );

        // Thawed, the same status produces the new order — the freeze suppresses
        // the move, it does not discard the signal.
        let thawed_view = ViewState {
            sort: SortMode::Live,
            ..Default::default()
        };
        assert_eq!(
            labels_of(&build_rows(&s, &ws, &thawed_view, &fresh, &[], &[], &[])),
            vec!["calm", "urgent", "home"],
        );
    }

    /// Same contract on the Attention axis, whose key is the tier rank.
    #[test]
    fn frozen_attention_ranks_keep_the_order_across_a_rerank() {
        let (s, ws, mut status) = freeze_fixture();
        for (p, r) in [("/wt/home", 0u32), ("/wt/calm", 1), ("/wt/urgent", 2)] {
            status.attention_ranks.insert(p.into(), r);
        }
        let freeze = std::sync::Arc::new(crate::sidebar_freeze::SortFreeze::capture(&status));

        let mut fresh = status.clone();
        fresh.attention_ranks.insert("/wt/urgent".into(), 0);
        fresh.attention_ranks.insert("/wt/home".into(), 2);

        let frozen_view = ViewState {
            sort: SortMode::Attention,
            freeze: Some(freeze),
            ..Default::default()
        };
        assert_eq!(
            labels_of(&build_rows(&s, &ws, &frozen_view, &fresh, &[], &[], &[])),
            vec!["home", "calm", "urgent"],
        );

        let thawed_view = ViewState {
            sort: SortMode::Attention,
            ..Default::default()
        };
        assert_eq!(
            labels_of(&build_rows(&s, &ws, &thawed_view, &fresh, &[], &[], &[])),
            vec!["urgent", "calm", "home"],
        );
    }

    /// Manual/Name/Recent key on data that only the user changes, so a freeze
    /// must be a pure no-op there — never a silently different order.
    #[test]
    fn a_freeze_is_inert_under_the_static_sorts() {
        let (s, ws, status) = freeze_fixture();
        let freeze = std::sync::Arc::new(crate::sidebar_freeze::SortFreeze::capture(&status));
        let mut fresh = status.clone();
        fresh.activity_recency.insert("/wt/calm".into(), 900.0);

        for mode in [SortMode::Manual, SortMode::Name, SortMode::Recent] {
            let frozen = ViewState {
                sort: mode,
                freeze: Some(freeze.clone()),
                ..Default::default()
            };
            let thawed = ViewState {
                sort: mode,
                ..Default::default()
            };
            assert_eq!(
                labels_of(&build_rows(&s, &ws, &frozen, &fresh, &[], &[], &[])),
                labels_of(&build_rows(&s, &ws, &thawed, &fresh, &[], &[], &[])),
                "{mode:?} must not care about the freeze"
            );
        }
    }

    /// A worktree created during the freeze receives a stable unknown key. Its
    /// changing live activity must not renumber the rows the user is navigating.
    #[test]
    fn a_new_worktree_stays_in_a_stable_tier_during_a_freeze() {
        let (_, ws, status) = freeze_fixture();
        let freeze = std::sync::Arc::new(crate::sidebar_freeze::SortFreeze::capture(&status));

        // A fourth worktree appears mid-freeze, more recently active than any.
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/calm", "/wt/calm"),
                tab("app/urgent", "/wt/urgent"),
                tab("app/fresh", "/wt/fresh"),
            ],
            0,
        );
        let mut fresh = status.clone();
        fresh.activity_recency.insert("/wt/fresh".into(), 900.0);

        let view = ViewState {
            sort: SortMode::Live,
            freeze: Some(freeze),
            ..Default::default()
        };
        assert_eq!(
            labels_of(&build_rows(&s, &ws, &view, &fresh, &[], &[], &[])),
            vec!["urgent", "home", "calm", "fresh"],
            "the new row cannot renumber the held rows before thaw"
        );
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
                sandbox_profile: None,
                env_degraded: false,
                external: false,
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
                sandbox_profile: None,
                env_degraded: false,
                external: false,
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
    fn resident_terminal_row_targets_its_remembered_tab() {
        let mut terminal = WorktreeGroup::terminal("local");
        terminal.tabs.push(crate::session::Tab::new("logs"));
        terminal.active_tab = 1;
        let s = session(vec![terminal], 0);
        let rows = build_rows(
            &s,
            &[],
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[term("local", "local", "")],
        );
        let row = rows
            .iter()
            .find(|r| r.kind == RowKind::Terminal)
            .expect("terminal row");
        assert_eq!(row.tab_target, Some(RowTarget::Tab(0, 1)));
    }

    #[test]
    fn lead_section_present_iff_enabled_first_and_not_under_terminals() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let terms = vec![term("lead", "lead", ""), term("local", "local", "")];
        let off = build_rows(
            &s,
            &[],
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &terms,
        );
        assert!(!off.iter().any(|r| r.label == "LEAD"));
        // The lead row never lists under TERMINALS, enabled or not.
        assert!(
            !off.iter()
                .any(|r| r.kind == RowKind::Terminal && r.label == "lead")
        );
        let view = ViewState {
            lead_enabled: true,
            ..Default::default()
        };
        let on = build_rows(&s, &[], &view, &no_activity(), &[], &[], &terms);
        assert_eq!(on[0].kind, RowKind::SectionHeading);
        assert_eq!(on[0].label, "LEAD");
        assert_eq!(on[1].kind, RowKind::Terminal);
        assert!(on[1].label.starts_with("Lead"));
        assert_eq!(
            on.iter()
                .filter(|r| r.kind == RowKind::Terminal && r.label == "lead")
                .count(),
            0
        );
        // Not yet registered: still shown, with the start hint.
        let none = build_rows(&s, &[], &view, &no_activity(), &[], &[], &[]);
        assert!(none[1].label.contains("Enter to start"));
    }

    #[test]
    fn unconfigured_lead_shows_a_setup_row_only_when_asked() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let off = build_rows(
            &s,
            &[],
            &ViewState::default(),
            &no_activity(),
            &[],
            &[],
            &[],
        );
        assert!(!off.iter().any(|r| r.label == LEAD_SETUP_LABEL));
        let view = ViewState {
            lead_setup_row: true,
            ..Default::default()
        };
        let on = build_rows(&s, &[], &view, &no_activity(), &[], &[], &[]);
        assert_eq!(on[0].label, "LEAD");
        assert_eq!(on[1].label, LEAD_SETUP_LABEL);
        assert_eq!(on[1].kind, RowKind::Terminal);
        // A configured Lead wins over the setup row.
        let both = ViewState {
            lead_enabled: true,
            lead_setup_row: true,
            ..Default::default()
        };
        let rows = build_rows(&s, &[], &both, &no_activity(), &[], &[], &[]);
        assert!(!rows.iter().any(|r| r.label == LEAD_SETUP_LABEL));
    }

    #[test]
    fn lead_row_label_carries_the_state() {
        let session = session(vec![WorktreeGroup::terminal("lead")], 0);
        let view = ViewState {
            lead_enabled: true,
            ..Default::default()
        };
        let mut status = no_activity();
        status.lead = Some(bar_with(thegn_core::lead_bar::LeadState::Working));
        let rows = build_rows(&session, &[], &view, &status, &[], &[], &[]);
        assert!(rows[1].label.contains("working"));

        status.lead = Some(bar_with(thegn_core::lead_bar::LeadState::Idle));
        let rows = build_rows(&session, &[], &view, &status, &[], &[], &[]);
        assert_eq!(rows[1].label, "Lead (1 tab)");
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
            sandbox_profile: None,
            env_degraded: false,
            external: false,
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
            sandbox_profile: None,
            external: false,
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

    /// The `(slug, display, kind, repo_path)` workspace tuple `build_rows`
    /// takes — named so the fixture below reads as a signature rather than a
    /// nest of `String`s (and so `clippy::type_complexity` stays quiet).
    type WsRow = (String, String, String, String);

    /// One workspace (`app` at `/repos/app`) whose registry knows a worktree the
    /// session may or may not carry. `path` deliberately sits far outside any
    /// `worktrees_dir` — that is the THE-73 case: a git-listed worktree parked
    /// under some other profile's tree, which the sidebar must still render.
    fn foreign_dir_registry_row() -> (Vec<WsRow>, Vec<DbWorktree>, &'static str) {
        const FOREIGN: &str = "/home/other-profile/.elsewhere/wt/foo";
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let dbw = vec![DbWorktree {
            slug: "app".into(),
            branch: "foo".into(),
            repo_path: "/repos/app".into(),
            tab_name: "app/foo".into(),
            path: FOREIGN.into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: None,
            sandbox_profile: None,
            env_degraded: false,
            external: false,
        }];
        (ws, dbw, FOREIGN)
    }

    #[test]
    fn registered_worktree_renders_even_when_the_session_missed_it() {
        // THE-73. The session adopted only `app/home`, but the registry also
        // holds `app/foo` (a real, git-listed worktree living outside any
        // `worktrees_dir`). The workspace being LIVE used to suppress every
        // registry row, so `foo` vanished on the click that loaded the
        // workspace and reappeared when it went dormant. It must render either
        // way — the sidebar shows the union of session and registry.
        let (ws, dbw, foreign) = foreign_dir_registry_row();
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        let foo = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.label == "foo")
            .expect("registered worktree renders even though the session missed it");
        assert_eq!(foo.depth, 1, "loose row beside its live siblings");
        assert_eq!(foo.worktree_path.as_deref(), Some(foreign));
        assert_eq!(
            foo.tab_target,
            Some(RowTarget::Workspace {
                repo_path: "/repos/app".into(),
                group: Some("app/foo".into()),
            }),
            "no live slot to focus, so activating it switches the workspace",
        );
        // The live sibling is untouched.
        assert!(
            rows.iter()
                .any(|r| r.kind == RowKind::Worktree && r.label == "home")
        );
    }

    #[test]
    fn a_live_group_is_not_duplicated_by_its_registry_row() {
        // The union is a union, not a concatenation: a worktree the session DID
        // adopt keeps exactly one row, and it is the live one (focus the tab,
        // don't re-switch the workspace).
        let (ws, mut dbw, _) = foreign_dir_registry_row();
        dbw[0].path = "/wt/foo".into(); // the session's copy of the same worktree
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/foo", "/wt/foo")],
            0,
        );
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        let foo: Vec<&SidebarRow> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree && r.label == "foo")
            .collect();
        assert_eq!(foo.len(), 1, "one row per worktree: {rows:?}");
        assert!(
            matches!(foo[0].tab_target, Some(RowTarget::Tab(..))),
            "the live group wins: {:?}",
            foo[0].tab_target
        );
    }

    #[test]
    fn a_live_group_renamed_in_session_is_not_duplicated_by_path() {
        // A group renamed in-session keeps its path but changes its tab name,
        // so the tab-name key misses. The path key catches it — otherwise the
        // same worktree would render twice, once per source.
        let (ws, mut dbw, _) = foreign_dir_registry_row();
        dbw[0].path = "/wt/foo".into();
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/renamed", "/wt/foo")],
            0,
        );
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        assert_eq!(
            rows.iter()
                .filter(|r| r.kind == RowKind::Worktree
                    && r.worktree_path.as_deref() == Some("/wt/foo"))
                .count(),
            1,
            "one row per worktree path: {rows:?}"
        );
    }

    #[test]
    fn a_live_fallback_workspace_still_synthesizes_nothing() {
        // A live-fallback workspace entry (built when no workspace pointer is
        // recorded) carries an EMPTY `repo_path`, so it has no switch target to
        // hand a synthetic row. It must contribute none — unchanged behaviour.
        let (_, dbw, _) = foreign_dir_registry_row();
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            String::new(),
        )];
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &ws,
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.kind == RowKind::Worktree && r.label == "foo"),
            "a pathless workspace synthesizes nothing: {rows:?}"
        );
    }

    #[test]
    fn flat_layout_renders_registered_worktrees_the_session_missed() {
        // The flat emitter goes through the same `gather_groups`, so the union
        // rule has to hold there too — with the repo tag flat rows carry.
        let (ws, dbw, foreign) = foreign_dir_registry_row();
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            flat: true,
            ..Default::default()
        };
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        let foo = rows
            .iter()
            .find(|r| r.kind == RowKind::Worktree && r.label == "foo")
            .expect("flat mode renders the missed registry row too");
        assert_eq!(foo.depth, 1);
        assert_eq!(foo.worktree_path.as_deref(), Some(foreign));
        assert_eq!(foo.repo_prefix.as_deref(), Some("app"));
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
            sandbox_profile: None,
            external: false,
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

    // --- external worktrees: the derived "Other" group ---------------------

    /// A dormant `app` workspace with home + 2 internal + 4 external rows.
    fn external_fixture() -> (WorkspaceFixtureRows, Vec<DbWorktree>) {
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let mut dbw = vec![db_row("app", "int-a"), db_row("app", "int-b")];
        for b in ["ext-1", "ext-2", "ext-3", "ext-4"] {
            dbw.push(DbWorktree {
                external: true,
                ..db_row("app", b)
            });
        }
        (ws, dbw)
    }

    fn app_rows(rows: &[SidebarRow]) -> Vec<&SidebarRow> {
        rows.iter().filter(|r| r.workspace_slug == "app").collect()
    }

    fn view_with(mode: ExternalWorktrees) -> ViewState {
        let mut view = ViewState::default();
        view.display.external_worktrees = mode;
        view
    }

    #[test]
    fn other_group_collapsed_shows_one_header_with_the_count() {
        let (ws, dbw) = external_fixture();
        let s = session(vec![], 0);
        let view = view_with(ExternalWorktrees::Group);
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        let app = app_rows(&rows);
        let groups: Vec<&&SidebarRow> = app
            .iter()
            .filter(|r| r.kind == RowKind::OtherGroup)
            .collect();
        assert_eq!(groups.len(), 1);
        let group = groups[0];
        assert_eq!(group.label, "Other");
        assert_eq!(group.child_count, 4);
        assert_eq!(group.pin_key, "other:app");
        assert_eq!(group.collapse_key(), "other:app");
        assert!(group.collapsed, "collapsed by default");
        assert!(group.visible);
        assert!(!group.is_pinnable() && !group.is_markable());
        assert!(RowKind::OtherGroup.is_collapsible());
        // Visible: the workspace, 3 internal rows, the header — no external.
        let visible: Vec<(&str, u8)> = app
            .iter()
            .filter(|r| r.visible)
            .map(|r| (r.label.as_str(), r.depth))
            .collect();
        assert_eq!(
            visible,
            vec![
                ("app", 0),
                ("home", 1),
                ("int-a", 1),
                ("int-b", 1),
                ("Other", 1)
            ]
        );
        // The four external rows are still emitted (hidden) under the header,
        // so the filter can find them.
        let hidden: Vec<(&str, u8)> = app
            .iter()
            .filter(|r| !r.visible && r.kind == RowKind::Worktree)
            .map(|r| (r.label.as_str(), r.depth))
            .collect();
        assert_eq!(
            hidden,
            vec![("ext-1", 2), ("ext-2", 2), ("ext-3", 2), ("ext-4", 2)]
        );
    }

    #[test]
    fn other_group_expanded_by_override_or_config() {
        let (ws, dbw) = external_fixture();
        let s = session(vec![], 0);
        let mut view = view_with(ExternalWorktrees::Group);
        view.other_open.insert("app".into(), true);
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        let shown: Vec<&str> = app_rows(&rows)
            .iter()
            .filter(|r| r.visible && r.depth == 2)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(shown, vec!["ext-1", "ext-2", "ext-3", "ext-4"]);

        // Config default open, no override: expanded too; an explicit
        // collapse override wins over it.
        let mut view = view_with(ExternalWorktrees::Group);
        view.display.other_group_collapsed = false;
        view.display.other_group_label = "Elsewhere".into();
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        let group = rows.iter().find(|r| r.kind == RowKind::OtherGroup).unwrap();
        assert!(!group.collapsed);
        assert_eq!(group.label, "Elsewhere");
        view.other_open.insert("app".into(), false);
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        let group = rows.iter().find(|r| r.kind == RowKind::OtherGroup).unwrap();
        assert!(group.collapsed);
    }

    #[test]
    fn external_hide_and_show_modes() {
        let (ws, dbw) = external_fixture();
        let s = session(vec![], 0);
        let worktrees = |mode| {
            let rows = build_rows(&s, &ws, &view_with(mode), &no_activity(), &dbw, &[], &[]);
            assert!(!rows.iter().any(|r| r.kind == RowKind::OtherGroup));
            rows.iter()
                .filter(|r| r.kind == RowKind::Worktree && r.workspace_slug == "app")
                .map(|r| (r.label.clone(), r.depth))
                .collect::<Vec<_>>()
        };
        let hide = worktrees(ExternalWorktrees::Hide);
        assert_eq!(
            hide,
            vec![
                ("home".to_string(), 1),
                ("int-a".to_string(), 1),
                ("int-b".to_string(), 1)
            ]
        );
        let show = worktrees(ExternalWorktrees::Show);
        assert_eq!(show.len(), 7);
        assert!(show.iter().all(|(_, depth)| *depth == 1));
    }

    #[test]
    fn active_external_row_reveals_its_group_and_survives_hide() {
        let (ws, dbw) = external_fixture();
        // ext-2 is live and active; the group is persisted closed.
        let s = session(vec![tab("app/ext-2", "/wt/ext-2")], 0);
        let mut view = view_with(ExternalWorktrees::Group);
        view.other_open.insert("app".into(), false);
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        let group = rows.iter().find(|r| r.kind == RowKind::OtherGroup).unwrap();
        assert!(!group.collapsed, "the active row's group renders open");
        assert_eq!(group.child_count, 4);
        let active = rows.iter().find(|r| r.active).unwrap();
        assert_eq!((active.label.as_str(), active.depth), ("ext-2", 2));
        assert!(active.visible);
        // The reveal keys name the group too.
        assert!(active_reveal_keys("app/ext-2", &dbw).contains(&other_group_key("app")));

        // `hide` never hides the active worktree: it stays, inline.
        let rows = build_rows(
            &s,
            &ws,
            &view_with(ExternalWorktrees::Hide),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        let ext: Vec<(&str, u8)> = rows
            .iter()
            .filter(|r| r.label.starts_with("ext-"))
            .map(|r| (r.label.as_str(), r.depth))
            .collect();
        assert_eq!(ext, vec![("ext-2", 1)]);
    }

    #[test]
    fn flat_mode_hide_drops_external_rows() {
        let (ws, dbw) = external_fixture();
        let s = session(vec![], 0);
        let count = |mode| {
            let mut view = view_with(mode);
            view.flat = true;
            build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[])
                .iter()
                .filter(|r| r.kind == RowKind::Worktree)
                .count()
        };
        assert_eq!(count(ExternalWorktrees::Hide), 3);
        assert_eq!(count(ExternalWorktrees::Group), 7);
        assert_eq!(count(ExternalWorktrees::Show), 7);
    }

    #[test]
    fn filter_surfaces_the_other_group_for_a_matching_external_row() {
        let (ws, dbw) = external_fixture();
        let s = session(vec![], 0);
        let mut view = view_with(ExternalWorktrees::Group);
        view.filter = "ext-3".into();
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        let visible: Vec<&str> = rows
            .iter()
            .filter(|r| r.visible)
            .map(|r| r.label.as_str())
            .collect();
        assert!(visible.contains(&"Other"), "{visible:?}");
        assert!(visible.contains(&"ext-3"), "{visible:?}");
        assert!(!visible.contains(&"ext-1"), "{visible:?}");
    }

    #[test]
    fn is_external_table() {
        let cfg = thegn_core::config::Config {
            worktrees_dir: "/wt".into(),
            worktree_mode: thegn_core::config::WorktreeMode::Global,
            ..Default::default()
        };
        let roots = vec![std::path::PathBuf::from("/home/u/agents")];
        let ctx = ExternalCheck {
            root: std::path::Path::new("/src/app"),
            repo_dir_name: "app",
            roots: &roots,
            cfg: &cfg,
        };
        let cases: [(&str, Option<i64>, bool); 9] = [
            // thegn's own layout.
            ("/wt/app/feat--abc", None, false),
            // Under the repo root (in-repo `.worktrees`, or any nested dir).
            ("/src/app/.worktrees/x", None, false),
            // The home checkout itself.
            ("/src/app", None, false),
            // Under a `worktree_roots` entry.
            ("/home/u/agents/p1/x", None, false),
            // Filed into a folder: adopted.
            ("/home/u/.other/x", Some(3), false),
            // Another tool's worktrees.
            ("/home/u/.other/x", None, true),
            ("/wt/ms2-feature", None, true),
            // Component-wise prefixes: `/home/u/agents` never claims
            // `/home/u/agents2`, nor `/wt/app` claims `/wt/apple`.
            ("/home/u/agents2/x", None, true),
            ("/wt/apple/x", None, true),
        ];
        for (path, folder, want) in cases {
            assert_eq!(is_external(path, folder, &ctx), want, "{path} {folder:?}");
        }
        // An empty path (a pathless row) is never classified external.
        assert!(!is_external("", None, &ctx));
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
            sandbox_profile: None,
            env_degraded: false,
            external: false,
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
            pipeline_lanes: lanes,
            ..SidebarStatus::default()
        }
    }

    fn lane(key: &str, worktrees: &[&str]) -> Lane {
        Lane {
            key: key.into(),
            label: key.into(),
            finished: false,
            last_ms: 0,
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
        // Two worktrees: a single-worktree lane collapses to one row, and these
        // fixtures exercise the lane folder.
        lane_status(vec![lane("THE-74", &[worktree, "/wt/zz-sibling"])])
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
    /// [worktrees]` — and nothing pipeline-shaped rides at depth 0.
    #[test]
    fn a_lane_emits_group_lane_and_worktree_inside_its_workspace() {
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
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        let group = kinds
            .iter()
            .position(|k| *k == RowKind::PipelineGroup)
            .expect("the Pipelines group sits inside the workspace");
        // Workspace first, then its own worktree rows, then the group chain.
        assert_eq!(kinds[0], RowKind::Workspace);
        assert!(group > 0, "the group is nested after the workspace header");
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
        assert_eq!(rows[group].workspace_slug, rows[0].workspace_slug);
        assert_eq!(rows[group].child_count, 1, "the group counts its lanes");
        assert_eq!(rows[group + 1].label, "THE-74", "named from the issue id");
        assert_eq!(rows[group + 1].depth, 2);
        assert_eq!(
            rows[group + 1].child_count,
            2,
            "the lane counts its worktrees"
        );
        assert_eq!(rows[group + 2].depth, 3);
        assert_eq!(rows[group + 2].label, "home");
        assert_eq!(
            rows.iter()
                .filter(|row| row.worktree_path.as_deref() == Some("/wt/home"))
                .map(|row| row.kind)
                .collect::<Vec<_>>(),
            vec![RowKind::PipelineWorktree],
            "the pipeline leaf is the only actionable row for its worktree"
        );
        // Everything is visible BY DEFAULT.
        for r in &rows[group..group + 3] {
            assert!(r.visible, "{:?} must be visible by default", r.kind);
        }
        // Nothing pipeline-shaped sits at the root of the tree: every such row
        // hangs under the workspace that owns it.
        assert!(
            !rows.iter().any(|r| r.depth == 0
                && matches!(
                    r.kind,
                    RowKind::PipelineGroup | RowKind::PipelineLane | RowKind::PipelineWorktree
                )),
            "pipelines never surface at the top level"
        );
    }

    #[test]
    fn a_single_worktree_lane_is_one_row_named_for_the_lane() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &lane_status(vec![lane("THE-74", &["/wt/home"])]),
            &[],
            &[],
            &[],
        );
        assert!(!rows.iter().any(|r| r.kind == RowKind::PipelineLane));
        let leaf = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .expect("the lone worktree is still a row");
        assert_eq!(leaf.label, "THE-74");
        assert_eq!(leaf.depth, 2);
        assert!(leaf.visible);
    }

    #[test]
    fn finished_lanes_fold_into_one_finished_sub_group() {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/other", "/wt/other")],
            0,
        );
        let mut done = lane("THE-9", &["/wt/other"]);
        done.finished = true;
        let status = lane_status(vec![lane("THE-74", &["/wt/home"]), done]);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &status,
            &[],
            &[],
            &[],
        );
        let finished = rows
            .iter()
            .position(|r| r.kind == RowKind::PipelineLane && r.label == "Finished")
            .expect("a Finished sub-group");
        assert_eq!(rows[finished].child_count, 1);
        assert_eq!(rows[finished].depth, 2);
        assert_eq!(rows[finished + 1].kind, RowKind::PipelineWorktree);
        assert_eq!(rows[finished + 1].label, "THE-9");
        assert_eq!(rows[finished + 1].depth, 3);
        let live = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree && r.label == "THE-74")
            .expect("the live lane stays above");
        assert_eq!(live.depth, 2);
    }

    #[test]
    fn a_finished_lane_older_than_the_window_leaves_the_sidebar() {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/other", "/wt/other")],
            0,
        );
        let mut old = lane("THE-9", &["/wt/other"]);
        old.finished = true;
        old.last_ms = 1;
        let mut view = ViewState::default();
        view.display.finished_pipeline_days = 7;
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &lane_status(vec![lane("THE-74", &["/wt/home"]), old.clone()]),
            &[],
            &[],
            &[],
        );
        assert!(
            !rows
                .iter()
                .any(|r| r.label == "THE-9" || r.label == "Finished")
        );
        view.display.finished_pipeline_days = 0;
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &lane_status(vec![lane("THE-74", &["/wt/home"]), old]),
            &[],
            &[],
            &[],
        );
        assert!(
            rows.iter().any(|r| r.label == "Finished"),
            "0 = never ages out"
        );
    }

    #[test]
    fn integration_worktrees_are_not_pipeline_rows() {
        let s = session(
            vec![tab("app/tg-b171-integration", "/wt/tg-b171-integration")],
            0,
        );
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &lane_status(vec![lane(
                "local:tg-b171-integration",
                &["/wt/tg-b171-integration"],
            )]),
            &[],
            &[],
            &[],
        );
        assert!(!rows.iter().any(|r| matches!(
            r.kind,
            RowKind::PipelineGroup | RowKind::PipelineLane | RowKind::PipelineWorktree
        )));
        assert!(rows.iter().any(|r| r.kind == RowKind::Worktree
            && r.worktree_path.as_deref() == Some("/wt/tg-b171-integration")));
    }

    #[test]
    fn a_terminal_roster_still_produces_the_lane_folders() {
        // The directive's restart-survival property: the folders ride the
        // roster's rows of ANY status, so they are there with zero active
        // dispatches.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let status = one_lane_status("/wt/home");
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &status,
            &[],
            &[],
            &[],
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
    fn a_pipeline_worktree_uses_the_existing_target_and_ordinary_pin_identity() {
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
        let mirror = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree)
            .expect("the lane's worktree row");
        assert_eq!(mirror.tab_target, Some(RowTarget::Tab(0, 0)), "same door");
        assert!(
            !rows
                .iter()
                .any(|r| r.kind == RowKind::Worktree
                    && r.worktree_path.as_deref() == Some("/wt/home")),
            "the pipeline leaf is the only default-layout row for this path"
        );
        // Existing pins use {workspace}/{label}; retaining that identity
        // preserves them through flat/grouped layout and ownership changes.
        assert_eq!(mirror.pin_key, "app/home");
        assert!(mirror.is_markable());
        assert!(mirror.is_pinnable());
    }

    #[test]
    fn existing_worktree_pin_identity_survives_pipeline_layout_transitions() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            pins: vec!["app/home".into()],
            ..ViewState::default()
        };
        let pipeline = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let pipeline_key = pipeline
            .iter()
            .find(|row| row.kind == RowKind::PipelineWorktree)
            .unwrap()
            .pin_key
            .clone();

        let mut flat_view = view.clone();
        flat_view.flat = true;
        let flat = build_rows(
            &s,
            &app_workspace(),
            &flat_view,
            &one_lane_status("/wt/home"),
            &[],
            &[],
            &[],
        );
        let flat_key = flat
            .iter()
            .find(|row| {
                row.kind == RowKind::Worktree && row.worktree_path.as_deref() == Some("/wt/home")
            })
            .unwrap()
            .pin_key
            .clone();

        let ordinary = build_rows(&s, &app_workspace(), &view, &no_activity(), &[], &[], &[]);
        let ordinary_key = ordinary
            .iter()
            .find(|row| {
                row.kind == RowKind::Worktree && row.worktree_path.as_deref() == Some("/wt/home")
            })
            .unwrap()
            .pin_key
            .clone();

        assert_eq!(pipeline_key, "app/home");
        assert_eq!(flat_key, pipeline_key);
        assert_eq!(ordinary_key, pipeline_key);
        assert!(view.pins.contains(&pipeline_key));
    }

    #[test]
    fn one_worktree_referenced_by_multiple_lanes_emits_one_path_identity() {
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
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == RowKind::PipelineWorktree)
                .count(),
            1,
            "one reference survives across lanes"
        );
        assert_eq!(keys.len(), 1, "one sidebar leaf per owned path: {keys:?}");
    }

    #[test]
    fn a_lane_worktree_with_no_primary_row_stays_but_opens_nothing() {
        // The lane is attributed by its FIRST resolvable worktree; a second one
        // thegn has no registration for is still emitted beside it, faint and
        // inert, rather than silently dropped from its lane.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &lane_status(vec![lane("THE-74", &["/wt/home", "/elsewhere/tg-gone"])]),
            &[],
            &[],
            &[],
        );
        let mirror = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree && r.label == "tg-gone")
            .expect("row is still emitted");
        assert!(mirror.tab_target.is_none());
    }

    #[test]
    fn an_unregistered_sibling_worktree_still_files_under_its_workspace() {
        // The common case a pipeline creates: a worktree beside the
        // workspace's registered ones, not yet in the session or the DB. The
        // directory holding its siblings names the workspace, so the lane
        // nests instead of falling out of the tree.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &lane_status(vec![lane("THE-74", &["/wt/tg-brand-new"])]),
            &[],
            &[],
            &[],
        );
        let group = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineGroup)
            .expect("the workspace's Pipelines group");
        assert_eq!(group.workspace_slug, "app");
        assert_eq!(group.pin_key, "pipeline/group:app");
        assert_eq!(group.depth, 1, "nested, never at the root");
    }

    #[test]
    fn a_lane_no_workspace_claims_is_not_emitted() {
        // There is no top-level `Pipelines` group any more: a lane thegn
        // cannot tie to a workspace is absent from the tree rather than
        // opening a second, root-level pipeline section. The board (`Alt b`)
        // is the complete view and still lists it.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &lane_status(vec![lane("THE-74", &["/elsewhere/tg-gone"])]),
            &[],
            &[],
            &[],
        );
        assert!(
            !rows.iter().any(|r| matches!(
                r.kind,
                RowKind::PipelineGroup | RowKind::PipelineLane | RowKind::PipelineWorktree
            )),
            "an unattributable lane contributes no rows"
        );
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
    fn persisted_pipeline_worktree_without_roster_uses_its_pipeline_leaf() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let persisted = vec!["/wt/home".to_string()];
        let mut status = lane_status(crate::sidebar_pipeline::lanes_with_persisted_paths(
            &[],
            &persisted,
        ));
        status.pipeline_worktree_paths = persisted
            .iter()
            .map(|path| crate::sidebar_pipeline::path_identity(path))
            .collect();
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &status,
            &[],
            &[],
            &[],
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.worktree_path.as_deref() == Some("/wt/home"))
                .map(|row| row.kind)
                .collect::<Vec<_>>(),
            vec![RowKind::PipelineWorktree],
            "durable ownership moves the open worktree without a live roster row"
        );
    }

    #[test]
    fn reserved_pipeline_environment_files_native_stage_less_worktree_without_roster() {
        // Native dispatches initially carry no stage or artifact metadata. The
        // persisted reserved environment is enough to place their one real
        // worktree row in Pipelines until roster metadata arrives.
        let s = session(vec![tab("app/feature", "/wt/feature")], 0);
        let dbw = vec![DbWorktree {
            slug: "app".into(),
            branch: "feature".into(),
            repo_path: "/repos/app".into(),
            tab_name: "app/feature".into(),
            path: "/wt/feature".into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: Some("pipeline".into()),
            sandbox_profile: None,
            env_degraded: false,
            external: false,
        }];
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &no_activity(),
            &dbw,
            &[],
            &[],
        );
        let matching: Vec<_> = rows
            .iter()
            .filter(|row| row.worktree_path.as_deref() == Some("/wt/feature"))
            .collect();
        assert_eq!(matching.len(), 1, "no duplicate ordinary row is emitted");
        assert_eq!(matching[0].kind, RowKind::PipelineWorktree);
        assert_eq!(matching[0].depth, 3);
        assert_eq!(matching[0].tab_target, Some(RowTarget::Tab(0, 0)));
        assert!(rows.iter().any(|row| row.kind == RowKind::PipelineGroup));
    }

    #[test]
    fn other_persisted_environments_remain_ordinary_worktrees() {
        let s = session(vec![tab("app/feature", "/wt/feature")], 0);
        let dbw = DbWorktree {
            slug: "app".into(),
            branch: "feature".into(),
            repo_path: "/repos/app".into(),
            tab_name: "app/feature".into(),
            path: "/wt/feature".into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: Some("company-k8s".into()),
            sandbox_profile: None,
            env_degraded: false,
            external: false,
        };
        let rows = build_rows(
            &s,
            &app_workspace(),
            &ViewState::default(),
            &no_activity(),
            &[dbw],
            &[],
            &[],
        );
        assert!(rows.iter().any(|row| {
            row.kind == RowKind::Worktree && row.worktree_path.as_deref() == Some("/wt/feature")
        }));
        assert!(!rows.iter().any(|row| {
            row.kind == RowKind::PipelineWorktree
                && row.worktree_path.as_deref() == Some("/wt/feature")
        }));
    }

    #[test]
    fn explicit_user_folder_filing_overrides_pipeline_default() {
        let (session, workspaces, mut db_worktrees, folders) = folder_fixture();
        db_worktrees[0].env_name = Some("pipeline".into());
        let persisted = vec!["/wt/feat".to_string()];
        let mut status = lane_status(crate::sidebar_pipeline::lanes_with_persisted_paths(
            &[],
            &persisted,
        ));
        status.pipeline_worktree_paths = persisted
            .iter()
            .map(|path| crate::sidebar_pipeline::path_identity(path))
            .collect();
        let rows = build_rows(
            &session,
            &workspaces,
            &ViewState::default(),
            &status,
            &db_worktrees,
            &folders,
            &[],
        );
        assert!(rows.iter().any(|row| {
            row.kind == RowKind::Worktree
                && row.worktree_path.as_deref() == Some("/wt/feat")
                && row.depth == 2
        }));
        assert!(!rows.iter().any(|row| row.kind == RowKind::PipelineWorktree));
    }

    #[test]
    fn a_user_folder_named_pipelines_folds_into_the_one_pipelines_group() {
        let (session, workspaces, mut db_worktrees, mut folders) = folder_fixture();
        folders[0].name = "pipelines".into();
        db_worktrees[0].env_name = Some("pipeline".into());
        let persisted = vec!["/wt/feat".to_string()];
        let mut status = lane_status(crate::sidebar_pipeline::lanes_with_persisted_paths(
            &[],
            &persisted,
        ));
        status.pipeline_worktree_paths = persisted
            .iter()
            .map(|path| crate::sidebar_pipeline::path_identity(path))
            .collect();
        let rows = build_rows(
            &session,
            &workspaces,
            &ViewState::default(),
            &status,
            &db_worktrees,
            &folders,
            &[],
        );
        assert!(rows.iter().any(|row| {
            row.kind == RowKind::PipelineWorktree
                && row.worktree_path.as_deref() == Some("/wt/feat")
        }));
        assert!(!rows.iter().any(|row| {
            row.kind == RowKind::Worktree && row.worktree_path.as_deref() == Some("/wt/feat")
        }));
    }

    #[test]
    fn clear_finished_hides_lanes_finished_by_then_and_keeps_later_ones() {
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/old", "/wt/old"),
                tab("app/late", "/wt/late"),
            ],
            0,
        );
        let mut old = lane("THE-9", &["/wt/old"]);
        old.finished = true;
        old.last_ms = 100;
        let mut late = lane("THE-10", &["/wt/late"]);
        late.finished = true;
        late.last_ms = 300;
        let mut view = ViewState::default();
        view.finished_cleared.insert("app".into(), 200);
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &lane_status(vec![lane("THE-74", &["/wt/home"]), old, late]),
            &[],
            &[],
            &[],
        );
        assert!(!rows.iter().any(|r| r.label == "THE-9"), "cleared");
        let finished = rows
            .iter()
            .find(|r| r.pin_key == finished_group_key("app"))
            .expect("the later lane keeps the Finished group");
        assert_eq!(finished.child_count, 1);
        assert!(rows.iter().any(|r| r.label == "THE-10"));
        assert!(
            rows.iter().any(|r| r.label == "THE-74"),
            "live lanes never clear"
        );
    }

    #[test]
    fn a_matched_lane_does_not_reveal_the_one_row_lane_after_it() {
        let s = session(
            vec![
                tab("app/home", "/wt/home"),
                tab("app/a", "/wt/a"),
                tab("app/b", "/wt/b"),
            ],
            0,
        );
        let view = ViewState {
            filter: "THE-74".into(),
            ..ViewState::default()
        };
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &lane_status(vec![
                lane("THE-74", &["/wt/home", "/wt/a"]),
                lane("ZZZ-1", &["/wt/b"]),
            ]),
            &[],
            &[],
            &[],
        );
        let single = rows
            .iter()
            .find(|r| r.kind == RowKind::PipelineWorktree && r.label == "ZZZ-1")
            .expect("the one-row lane is emitted");
        assert!(!single.visible, "THE-74's match must not leak into ZZZ-1");
    }

    #[test]
    fn collapsing_a_lane_hides_its_worktrees_but_still_emits_them() {
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/other", "/wt/other")],
            0,
        );
        let mut view = ViewState::default();
        view.collapsed.insert("pipeline/lane:THE-74".into());
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/other"),
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
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/other", "/wt/other")],
            0,
        );
        let mut view = ViewState::default();
        view.collapsed.insert("pipeline/group:app".into());
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &one_lane_status("/wt/other"),
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
    fn an_open_pipeline_worktree_stays_visible_inside_collapsed_pipeline_folders() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            collapsed: [
                "pipeline/group:app".to_string(),
                "pipeline/lane:THE-74".to_string(),
            ]
            .into_iter()
            .collect(),
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
        let group = rows
            .iter()
            .find(|row| row.kind == RowKind::PipelineGroup)
            .unwrap();
        let lane = rows
            .iter()
            .find(|row| row.kind == RowKind::PipelineLane)
            .unwrap();
        let worktree = rows
            .iter()
            .find(|row| row.kind == RowKind::PipelineWorktree)
            .unwrap();
        assert!(group.visible && !group.collapsed);
        assert!(lane.visible && !lane.collapsed);
        assert!(worktree.visible, "the active tab remains findable");
        assert!(view.collapsed.contains("pipeline/group:app"));
        assert!(view.collapsed.contains("pipeline/lane:THE-74"));
    }

    #[test]
    fn two_lanes_collapse_independently() {
        // They share one `workspace_slug`, so keying collapse on it would fold
        // every lane at once — the folder precedent (`pin_key`) is why not.
        let s = session(
            vec![tab("app/home", "/wt/home"), tab("app/other", "/wt/other")],
            0,
        );
        let mut view = ViewState::default();
        view.collapsed.insert("pipeline/lane:THE-9".into());
        let status = lane_status(vec![
            lane("THE-74", &["/wt/home", "/wt/zz-a"]),
            lane("THE-9", &["/wt/other", "/wt/zz-b"]),
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
    fn pipeline_worktree_leaf_keeps_worktree_mark_and_pin_actions() {
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
        for r in derived
            .iter()
            .filter(|r| r.kind != RowKind::PipelineWorktree)
        {
            assert!(
                !r.is_markable(),
                "{:?} must never join the mark set",
                r.kind
            );
            assert!(!r.is_pinnable(), "{:?} must never be pinned", r.kind);
            assert!(r.folder_id.is_none(), "{:?} is not a real folder", r.kind);
        }
        let worktree = derived
            .iter()
            .find(|row| row.kind == RowKind::PipelineWorktree)
            .unwrap();
        assert!(worktree.is_markable());
        assert!(worktree.is_pinnable());
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
        assert!(group > 0, "the group still follows its workspace header");
        assert_eq!(rows[group].depth, 1);
        assert_eq!(rows[group].workspace_slug, rows[0].workspace_slug);
        assert_eq!(
            rows.iter()
                .filter(|row| row.worktree_path.as_deref() == Some("/wt/home"))
                .map(|row| row.kind)
                .collect::<Vec<_>>(),
            vec![RowKind::PipelineWorktree],
            "a stale lane pin does not duplicate or move the actionable row"
        );
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
    fn a_pipeline_group_match_reveals_its_lanes_and_worktrees() {
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            filter: "Pipelines".into(),
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
        assert!(
            rows.iter()
                .find(|r| r.kind == RowKind::PipelineGroup)
                .unwrap()
                .visible
        );
        assert!(
            rows.iter()
                .find(|r| r.kind == RowKind::PipelineLane)
                .unwrap()
                .visible
        );
        assert!(
            rows.iter()
                .find(|r| r.kind == RowKind::PipelineWorktree)
                .unwrap()
                .visible
        );
    }

    #[test]
    fn the_flat_layout_emits_no_pipeline_rows() {
        // Flat mode has no workspace rows to nest under, and there is no
        // top-level group to fall back to — so it grows no pipeline rows at
        // all. `Alt b` is the route to the roster there.
        let s = session(vec![tab("app/home", "/wt/home")], 0);
        let view = ViewState {
            flat: true,
            ..ViewState::default()
        };
        let dbw = vec![DbWorktree {
            slug: "app".into(),
            branch: "home".into(),
            repo_path: "/repos/app".into(),
            tab_name: "app/home".into(),
            path: "/wt/home".into(),
            folder_id: None,
            sandbox_backend: None,
            env_name: Some("pipeline".into()),
            sandbox_profile: None,
            env_degraded: false,
            external: false,
        }];
        let rows = build_rows(
            &s,
            &app_workspace(),
            &view,
            &lane_status(vec![lane("THE-74", &["/wt/home"])]),
            &dbw,
            &[],
            &[],
        );
        assert!(
            !rows.iter().any(|r| matches!(
                r.kind,
                RowKind::PipelineGroup | RowKind::PipelineLane | RowKind::PipelineWorktree
            )),
            "the flat layout has nowhere to nest a lane"
        );
        assert!(rows.iter().any(|row| {
            row.kind == RowKind::Worktree && row.worktree_path.as_deref() == Some("/wt/home")
        }));
    }

    #[test]
    fn detail_line_shows_binary_only_branch_diff() {
        use thegn_svc::git::DiffCount;

        let mut row = SidebarRow::base(RowKind::Worktree, 1, "feat", "app");
        row.git = Some(GitGlyphs {
            branch_diff: Some((DiffCount::Binary, DiffCount::Binary)),
            ..GitGlyphs::default()
        });
        let line = compose_detail_line(&row, &crate::sidebar_view::SidebarDisplay::default())
            .expect("default detail display shows branch stats");
        let crate::seg::Line::Segs(segs) = line else {
            panic!("detail line is composed from segments")
        };
        let text = segs.into_iter().map(|seg| seg.text).collect::<String>();
        assert!(text.contains("bin"), "{text:?}");
    }

    #[test]
    fn detail_line_marks_unavailable_branch_diff() {
        use thegn_svc::git::DiffCount;

        let mut row = SidebarRow::base(RowKind::Worktree, 1, "feat", "app");
        row.git = Some(GitGlyphs {
            branch_diff: Some((DiffCount::Unavailable, DiffCount::Unavailable)),
            ..GitGlyphs::default()
        });
        let line = compose_detail_line(&row, &crate::sidebar_view::SidebarDisplay::default())
            .expect("default detail display shows branch stats");
        let crate::seg::Line::Segs(segs) = line else {
            panic!("detail line is composed from segments")
        };
        let unavailable_marks = segs
            .iter()
            .filter(|seg| {
                seg.text == "diff ?"
                    && seg.fg == crate::seg::Tok::Hue(thegn_core::theme::Hue::Amber)
            })
            .count();
        assert_eq!(unavailable_marks, 1, "{segs:?}");
        let text = segs.iter().map(|seg| seg.text.as_str()).collect::<String>();
        assert!(text.contains("diff ?"), "{text:?}");
        assert!(!text.contains("+0"), "{text:?}");
        assert!(!text.contains("-0"), "{text:?}");
    }

    #[test]
    fn detail_line_replaces_hostile_label_cells() {
        let mut row = SidebarRow::base(
            RowKind::Worktree,
            1,
            "feat\u{1b}]52;c;QQ==\u{7}\u{202e}",
            "app",
        );
        row.branch = Some("branch\u{200b}\u{9b}".into());
        let line = compose_detail_line(&row, &crate::sidebar_view::SidebarDisplay::default())
            .expect("hostile branch should still produce detail");
        let crate::seg::Line::Segs(segs) = line else {
            panic!("detail line is composed from segments")
        };
        let text = segs.iter().map(|seg| seg.text.as_str()).collect::<String>();
        assert!(
            text.chars()
                .all(|c| !c.is_control() && !thegn_core::pipeline_report::is_format_control(c)),
            "{text:?}"
        );
        assert!(text.contains('�'), "{text:?}");
    }

    // --- row limits: the derived "show more" row --------------------------

    /// A workspace `app` holding one folder `wip` of `n` worktrees (depth 2),
    /// in final display order — the shape `apply_row_limits` runs on.
    fn folder_of(n: usize) -> Vec<SidebarRow> {
        let mut rows = vec![
            SidebarRow {
                pin_key: "app".into(),
                ..SidebarRow::base(RowKind::Workspace, 0, "app", "app")
            },
            SidebarRow {
                pin_key: "app/folder:1".into(),
                folder_id: Some(1),
                child_count: n,
                ..SidebarRow::base(RowKind::Folder, 1, "wip", "app")
            },
        ];
        for i in 0..n {
            rows.push(SidebarRow {
                pin_key: format!("app/wt{i}"),
                ..SidebarRow::base(RowKind::Worktree, 2, format!("wt{i}"), "app")
            });
        }
        rows
    }

    fn limit_view(limit: usize) -> ViewState {
        let mut view = ViewState::default();
        view.display.group_row_limit = limit;
        view
    }

    fn shown_worktrees(rows: &[SidebarRow]) -> Vec<String> {
        rows.iter()
            .filter(|r| r.visible && r.kind == RowKind::Worktree)
            .map(|r| r.label.clone())
            .collect()
    }

    fn show_more_rows(rows: &[SidebarRow]) -> Vec<&SidebarRow> {
        rows.iter()
            .filter(|r| r.kind == RowKind::ShowMore)
            .collect()
    }

    #[test]
    fn row_limit_truncates_a_folder_and_adds_show_more() {
        let mut rows = folder_of(20);
        apply_row_limits(&mut rows, &limit_view(15));
        let shown = shown_worktrees(&rows);
        assert_eq!(shown.len(), 15);
        assert_eq!(shown.first().map(String::as_str), Some("wt0"));
        assert_eq!(shown.last().map(String::as_str), Some("wt14"));
        let more = show_more_rows(&rows);
        assert_eq!(more.len(), 1);
        let more = more[0];
        assert_eq!(more.child_count, 5);
        assert!(more.collapsed, "truncated reads `show N more`");
        assert!(more.visible);
        assert_eq!(more.depth, 2, "indented at the folder's child depth");
        assert_eq!(more.pin_key, show_more_key("app/folder:1"));
        assert_eq!(show_more_group(more), Some("app/folder:1"));
        assert!(more.tab_target.is_none());
        assert!(!more.is_pinnable() && !more.is_markable());
        assert!(!RowKind::ShowMore.is_collapsible());
        // It sits right after the last shown entry.
        let at = rows.iter().position(|r| r.kind == RowKind::ShowMore);
        assert_eq!(at.map(|i| rows[i - 1].label.as_str()), Some("wt14"));
        assert_eq!(
            crate::sidebar_view::show_more_text(more),
            format!("{} show 5 more", crate::caps::active_glyphs().ellipsis)
        );
    }

    #[test]
    fn row_limit_expanded_group_shows_all_then_show_fewer() {
        let mut rows = folder_of(20);
        let mut view = limit_view(15);
        view.more_open.insert("app/folder:1".into());
        apply_row_limits(&mut rows, &view);
        assert_eq!(shown_worktrees(&rows).len(), 20);
        let more = show_more_rows(&rows);
        assert_eq!(more.len(), 1);
        assert!(!more[0].collapsed);
        assert!(more[0].visible);
        assert_eq!(crate::sidebar_view::show_more_text(more[0]), "show fewer");
        assert_eq!(rows.last().map(|r| r.kind), Some(RowKind::ShowMore));
    }

    #[test]
    fn row_limit_zero_disables_truncation() {
        let mut rows = folder_of(20);
        apply_row_limits(&mut rows, &limit_view(0));
        assert_eq!(shown_worktrees(&rows).len(), 20);
        assert!(show_more_rows(&rows).is_empty());
        // At or under the limit: nothing to hide, no extra row.
        let mut rows = folder_of(15);
        apply_row_limits(&mut rows, &limit_view(15));
        assert_eq!(shown_worktrees(&rows).len(), 15);
        assert!(show_more_rows(&rows).is_empty());
    }

    #[test]
    fn row_limit_per_group_override_wins_by_label() {
        let mut rows = folder_of(20);
        let mut view = limit_view(15);
        view.display.group_row_limits.insert("wip".into(), 5);
        apply_row_limits(&mut rows, &view);
        assert_eq!(shown_worktrees(&rows).len(), 5);
        assert_eq!(show_more_rows(&rows)[0].child_count, 15);
        // An override of 0 lifts the limit for that group alone, even when
        // the default is 0 elsewhere.
        let mut rows = folder_of(20);
        let mut view = limit_view(15);
        view.display.group_row_limits.insert("wip".into(), 0);
        apply_row_limits(&mut rows, &view);
        assert_eq!(shown_worktrees(&rows).len(), 20);
        let mut rows = folder_of(20);
        let mut view = limit_view(0);
        view.display.group_row_limits.insert("wip".into(), 3);
        apply_row_limits(&mut rows, &view);
        assert_eq!(shown_worktrees(&rows).len(), 3);
    }

    #[test]
    fn row_limit_never_hides_the_active_row() {
        let mut rows = folder_of(20);
        rows[2 + 18].active = true; // wt18
        apply_row_limits(&mut rows, &limit_view(15));
        let shown = shown_worktrees(&rows);
        assert!(shown.contains(&"wt18".to_string()), "{shown:?}");
        // Promoted into the slice: the group still shows `limit` entries.
        assert_eq!(shown.len(), 15);
        assert!(!shown.contains(&"wt14".to_string()));
        assert_eq!(show_more_rows(&rows)[0].child_count, 5);
    }

    #[test]
    fn row_limit_never_hides_pinned_rows() {
        let mut rows = folder_of(20);
        let mut view = limit_view(15);
        view.pins = vec!["app/wt17".into(), "app/wt19".into()];
        apply_row_limits(&mut rows, &view);
        let shown = shown_worktrees(&rows);
        assert!(shown.contains(&"wt17".to_string()));
        assert!(shown.contains(&"wt19".to_string()));
        assert_eq!(shown.len(), 15);
    }

    #[test]
    fn row_limit_is_off_while_filtering() {
        let mut rows = folder_of(20);
        let mut view = limit_view(15);
        view.filter = "wt".into();
        apply_row_limits(&mut rows, &view);
        assert_eq!(shown_worktrees(&rows).len(), 20);
        assert!(show_more_rows(&rows).is_empty());
    }

    #[test]
    fn row_limit_toggle_sits_before_following_folder() {
        let mut rows = vec![SidebarRow {
            pin_key: "app".into(),
            ..SidebarRow::base(RowKind::Workspace, 0, "app", "app")
        }];
        for i in 0..20 {
            rows.push(SidebarRow {
                pin_key: format!("app/wt{i}"),
                ..SidebarRow::base(RowKind::Worktree, 1, format!("wt{i}"), "app")
            });
        }
        rows.push(SidebarRow {
            pin_key: "app/folder:1".into(),
            folder_id: Some(1),
            ..SidebarRow::base(RowKind::Folder, 1, "wip", "app")
        });
        apply_row_limits(&mut rows, &limit_view(15));
        let at = rows
            .iter()
            .position(|r| r.kind == RowKind::ShowMore)
            .expect("show more row");
        assert_eq!(rows[at - 1].label, "wt14");
        let folder = rows.iter().position(|r| r.kind == RowKind::Folder);
        assert!(folder > Some(at));
    }

    #[test]
    fn row_limit_toggle_follows_a_collapsed_parent() {
        let mut rows = folder_of(20);
        for r in rows.iter_mut().skip(2) {
            r.visible = false; // the folder is collapsed
        }
        apply_row_limits(&mut rows, &limit_view(15));
        let more = show_more_rows(&rows);
        assert_eq!(more.len(), 1);
        assert!(!more[0].visible, "a collapsed group hides its toggle too");
    }

    /// End to end through `build_rows`: a dormant workspace's loose rows
    /// (home + 20) are a limited group keyed by the workspace slug, and the
    /// filter lifts the limit.
    #[test]
    fn build_rows_applies_the_row_limit_to_loose_worktrees() {
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let dbw: Vec<DbWorktree> = (0..20)
            .map(|i| db_row("app", &format!("b{i:02}")))
            .collect();
        let s = session(vec![], 0);
        let view = limit_view(15);
        let rows = build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[]);
        assert_eq!(shown_worktrees(&rows).len(), 15);
        let more = show_more_rows(&rows);
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].pin_key, show_more_key("app"));
        assert_eq!(more[0].child_count, 6, "home + 20 = 21 entries");
        assert_eq!(more[0].depth, 1);

        let mut filtered = limit_view(15);
        filtered.filter = "b".into();
        let rows = build_rows(&s, &ws, &filtered, &no_activity(), &dbw, &[], &[]);
        assert!(show_more_rows(&rows).is_empty());
        assert!(shown_worktrees(&rows).len() >= 20);
    }

    /// The plan-driven loose emission (`fold::plan_loose`) emits every loose
    /// worktree exactly once, in the workspace's sort order: the 20-row
    /// fixture (home + 20, no batch token) renders 21 entries whether batch
    /// grouping is on or off, and grouping on changes no row.
    #[test]
    fn build_rows_spine_emits_every_loose_row_once() {
        let ws = vec![(
            "app".to_string(),
            "app".to_string(),
            "repo".to_string(),
            "/repos/app".to_string(),
        )];
        let dbw: Vec<DbWorktree> = (0..20)
            .map(|i| db_row("app", &format!("b{i:02}")))
            .collect();
        let s = session(vec![], 0);
        let shape = |min: usize| -> Vec<(RowKind, String, String)> {
            let view = ViewState {
                display: crate::sidebar_view::SidebarDisplay {
                    batch_group_min: min,
                    ..Default::default()
                },
                ..Default::default()
            };
            build_rows(&s, &ws, &view, &no_activity(), &dbw, &[], &[])
                .into_iter()
                .map(|r| (r.kind, r.label, r.pin_key))
                .collect()
        };
        let on = shape(2);
        let worktrees: Vec<&(RowKind, String, String)> =
            on.iter().filter(|r| r.0 == RowKind::Worktree).collect();
        assert_eq!(worktrees.len(), 21, "home + 20 = 21 entries");
        let keys: HashSet<&str> = worktrees.iter().map(|r| r.2.as_str()).collect();
        assert_eq!(keys.len(), 21, "every loose row is emitted once");
        for i in 0..20 {
            assert!(keys.contains(format!("app/b{i:02}").as_str()));
        }
        assert!(!on.iter().any(|r| r.0 == RowKind::BatchGroup));
        assert_eq!(on, shape(0), "no batch token: grouping changes nothing");
    }

    // ---- b164 lane 2: batch folding through build_rows ----

    /// The owner's 50 real worktrees in the `thegn` workspace: three plain
    /// names (with the synthesized `home`, four singles) and five batches.
    fn owner_db_rows() -> Vec<DbWorktree> {
        let mut branches: Vec<String> = vec![
            "merry-stream".into(),
            "tg/b137-the-2001-fix".into(),
            "tg/hardy-onyx".into(),
        ];
        for (b, n) in [(158, 5), (160, 11), (161, 11), (162, 10), (163, 10)] {
            branches.extend((0..n).map(|i| format!("tg/b{b}-b{b}-lane{i:02}")));
        }
        assert_eq!(branches.len(), 50);
        branches.iter().map(|b| db_row("thegn", b)).collect()
    }

    fn thegn_ws() -> Vec<(String, String, String, String)> {
        vec![(
            "thegn".to_string(),
            "thegn".to_string(),
            "repo".to_string(),
            "/repos/thegn".to_string(),
        )]
    }

    #[test]
    fn build_rows_folds_the_owner_list_to_seven() {
        let view = ViewState {
            display: crate::sidebar_view::SidebarDisplay {
                batch_group_min: 2,
                group_row_limit: 7,
                ..Default::default()
            },
            ..Default::default()
        };
        let s = session(vec![], 0);
        let rows = build_rows(
            &s,
            &thegn_ws(),
            &view,
            &no_activity(),
            &owner_db_rows(),
            &[],
            &[],
        );
        let shown: Vec<(RowKind, &str)> = rows
            .iter()
            .filter(|r| {
                r.visible
                    && r.depth == 1
                    && r.workspace_slug == "thegn"
                    && r.kind != RowKind::ShowMore
            })
            .map(|r| (r.kind, r.label.as_str()))
            .collect();
        let batches: Vec<&str> = shown
            .iter()
            .filter(|r| r.0 == RowKind::BatchGroup)
            .map(|r| r.1)
            .collect();
        assert_eq!(shown.len(), 7, "{shown:?}");
        assert_eq!(batches, vec!["b163", "b162", "b161"]);
        assert!(
            shown[..4].iter().all(|r| r.0 == RowKind::Worktree),
            "singles first: {shown:?}"
        );
        let more: Vec<&SidebarRow> = rows
            .iter()
            .filter(|r| r.kind == RowKind::ShowMore)
            .collect();
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].child_count, 2, "b160 and b158 fold");
        let all: Vec<&SidebarRow> = rows
            .iter()
            .filter(|r| r.kind == RowKind::BatchGroup)
            .collect();
        assert_eq!(all.len(), 5);
        assert!(all.iter().all(|r| r.collapsed), "every batch collapsed");
        assert!(all.iter().all(|r| r.group_stats.is_some()));
        assert!(
            rows.iter()
                .filter(|r| r.depth == 2)
                .all(|r| !r.visible && r.batch_prefix.is_some()),
            "members hide under their collapsed batch"
        );
        let member = rows
            .iter()
            .find(|r| r.label == "tg/b161-b161-lane03")
            .expect("member");
        assert_eq!(member.pin_key, "thegn/tg/b161-b161-lane03");
        assert_eq!(member.batch_prefix.as_deref(), Some("tg/b161-b161-"));
    }

    #[test]
    fn build_rows_never_batches_in_the_flat_layout() {
        let view = ViewState {
            flat: true,
            display: crate::sidebar_view::SidebarDisplay {
                batch_group_min: 2,
                ..Default::default()
            },
            ..Default::default()
        };
        let s = session(vec![], 0);
        let rows = build_rows(
            &s,
            &thegn_ws(),
            &view,
            &no_activity(),
            &owner_db_rows(),
            &[],
            &[],
        );
        assert!(!rows.iter().any(|r| r.kind == RowKind::BatchGroup));
        assert!(rows.iter().all(|r| r.batch_prefix.is_none()));
        let labels: HashSet<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Worktree)
            .map(|r| r.label.as_str())
            .collect();
        assert!(labels.contains("tg/b161-b161-lane03"));
        assert!(labels.len() >= 50, "every worktree is its own row");
    }
}
