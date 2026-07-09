//! In-process chrome: the four surfaces (tabbar, sidebar, panel, statusbar)
//! drawn natively into the back-buffer `Surface` around the center pane. No
//! WASM, no IPC, no broadcast — widgets read state directly and draw cells.
//! This replaces the four zellij plugins.

use termwiz::cell::AttributeChange;
use termwiz::color::{ColorAttribute, SrgbaTuple};
use termwiz::surface::{Change, Position, Surface};

use crate::compositor::{Rect, compose_pane};
use crate::emulator::PaneEmulator;
use superzej_core::theme;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct AiMetrics {
    pub agent: String,
    pub session_id: String,
    pub tokens: TokenUsage,
    pub cost: f64,
}

/// The embedded agent's ACP connection state, surfaced in the statusbar chip so
/// a connect/proxy failure is visible (the chip is the *only* native signal — pi
/// owns the conversation in its terminal pane, by design).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AgentConn {
    /// ACP server spawned; client not connected/initialized yet.
    Connecting,
    /// Connected + initialized (+ provider routed when the proxy is enabled).
    #[default]
    Online,
    /// The ACP socket dropped (agent likely went away).
    Exited,
    /// Connect / initialize / provider-routing failed.
    Error,
}

/// Live activity of the embedded `pi` agent, streamed over ACP `session/update`
/// (tool calls + context-window usage) plus its connection lifecycle. Distinct
/// from [`AiMetrics`], which is proxy-side spend; this is the agent's own
/// progress, rendered as a statusbar chip so the user sees what the agent is
/// doing without leaving their pane.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentActivity {
    /// Connection lifecycle (drives the offline/error chip states).
    pub conn: AgentConn,
    /// The most recent tool the agent invoked (e.g. "bash", "edit").
    pub last_tool: Option<String>,
    /// Whether that tool is still running (vs. completed/failed).
    pub running: bool,
    /// Context-window tokens used / total, from `usage_update` (0 = unknown).
    pub context_used: i64,
    pub context_size: i64,
}

/// The resolved chrome palette. A process-global because every draw helper
/// needs it and threading it through each call would touch every signature;
/// the event loop writes it (startup + config reload), render-time code only
/// reads. Defaults match the built-in storm-blue theme.
static PALETTE: std::sync::LazyLock<std::sync::RwLock<theme::Palette>> =
    std::sync::LazyLock::new(|| std::sync::RwLock::new(theme::Palette::default()));

/// Install the resolved palette (startup and live config reload).
pub fn set_palette(p: theme::Palette) {
    if let Ok(mut g) = PALETTE.write() {
        *g = p;
    }
}

/// A palette slot, resolvable to a live color via [`col`]. Kept complete even
/// where a slot has no call site yet, so new chrome picks from one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum S {
    Bg0,
    Bg1,
    Panel,
    Panel2,
    Raise,
    Border,
    Focus,
    Text,
    Dim,
    Faint,
    Ghost,
    Ghost2,
    Ghost3,
    ShadowBg,
    ShadowFg,
    ChipFg,
    Accent,
    ActivityActive,
    ActivityWaiting,
}

/// The "R;G;B" fragment for a slot within a palette (shared by [`col`] and
/// the seg layer's one-lock-per-line resolution).
pub fn slot_rgb(p: &theme::Palette, s: S) -> &str {
    match s {
        S::Bg0 => &p.bg0,
        S::Bg1 => &p.bg1,
        S::Panel => &p.panel,
        S::Panel2 => &p.panel2,
        S::Raise => &p.raise,
        S::Border => &p.border,
        S::Focus => &p.focus,
        S::Text => &p.text,
        S::Dim => &p.dim,
        S::Faint => &p.faint,
        S::Ghost => &p.ghost,
        S::Ghost2 => &p.ghost2,
        S::Ghost3 => &p.ghost3,
        S::ShadowBg => &p.shadow_bg,
        S::ShadowFg => &p.shadow_fg,
        S::ChipFg => &p.chip_fg,
        S::Accent => &p.accent,
        S::ActivityActive => &p.activity_active,
        S::ActivityWaiting => &p.activity_waiting,
    }
}

/// Resolve a palette slot to a termwiz color (reads the live palette).
pub fn col(s: S) -> ColorAttribute {
    let p = PALETTE.read().expect("palette lock");
    theme_color(slot_rgb(&p, s))
}

/// Run `f` with the live palette borrowed (one lock acquisition for a whole
/// line/frame of seg resolution).
pub fn with_palette<R>(f: impl FnOnce(&theme::Palette) -> R) -> R {
    let p = PALETTE.read().expect("palette lock");
    f(&p)
}

/// The focus color's "R;G;B" fragment (for `theme::blend` tints).
pub fn focus_rgb() -> String {
    PALETTE.read().expect("palette lock").focus.clone()
}

/// The panel surface's "R;G;B" fragment (the base for tints on chrome zones).
pub fn panel_rgb() -> String {
    PALETTE.read().expect("palette lock").panel.clone()
}

/// Parse a theme `"r;g;b"` triple into a termwiz color.
pub fn theme_color(triple: &str) -> ColorAttribute {
    let mut it = triple
        .split(';')
        .filter_map(|s| s.trim().parse::<u8>().ok());
    match (it.next(), it.next(), it.next()) {
        (Some(r), Some(g), Some(b)) => ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            1.0,
        )),
        _ => ColorAttribute::Default,
    }
}

/// As [`draw_text`], in bold (section titles, headers).
pub fn draw_text_bold(
    surface: &mut Surface,
    x: usize,
    y: usize,
    text: &str,
    fg: ColorAttribute,
    bg: ColorAttribute,
    max_cols: usize,
) {
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    surface.add_change(Change::Attribute(AttributeChange::Foreground(fg)));
    surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
    surface.add_change(Change::Attribute(AttributeChange::Intensity(
        termwiz::cell::Intensity::Bold,
    )));
    let clipped: String = crate::seg::take_cols(text, max_cols).to_string();
    surface.add_change(Change::Text(clipped));
    surface.add_change(Change::Attribute(AttributeChange::Intensity(
        termwiz::cell::Intensity::Normal,
    )));
}

/// Write `text` at `(x, y)`, clipped to `max_cols`, with the given colors. Does
/// not fill beyond the text — use [`fill`] first for a solid background.
pub fn draw_text(
    surface: &mut Surface,
    x: usize,
    y: usize,
    text: &str,
    fg: ColorAttribute,
    bg: ColorAttribute,
    max_cols: usize,
) {
    surface.add_change(Change::CursorPosition {
        x: Position::Absolute(x),
        y: Position::Absolute(y),
    });
    surface.add_change(Change::Attribute(AttributeChange::Foreground(fg)));
    surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
    let clipped: String = crate::seg::take_cols(text, max_cols).to_string();
    surface.add_change(Change::Text(clipped));
}

/// Draw a transport-neutral plugin [`View`](superzej_core::plugin_api::View)
/// into a host-owned surface rect.
/// Plugins supply semantic roles only; this function resolves them against the
/// current superzej theme/accent and clips to the host-owned slot.
///
/// Not yet wired into the live chrome — the plugin API surface (v0) landed
/// ahead of the host-side contribution renderer; covered by unit tests.
#[allow(dead_code)]
pub fn draw_plugin_view(
    surface: &mut Surface,
    rect: Rect,
    view: &superzej_core::plugin_api::View,
    accent_rgb: &str,
) {
    if rect.rows == 0 || rect.cols == 0 {
        return;
    }
    fill(surface, rect, col(S::Bg1));
    let mut x = rect.x;
    let max_x = rect.x + rect.cols;
    for span in &view.spans {
        if x >= max_x {
            break;
        }
        let fg = plugin_role_color(span.role, accent_rgb);
        let bg = col(S::Bg1);
        let max_cols = max_x.saturating_sub(x);
        draw_text(surface, x, rect.y, &span.text, fg, bg, max_cols);
        x += span.text.chars().take(max_cols).count();
    }
}

#[allow(dead_code)]
fn plugin_role_color(
    role: superzej_core::plugin_api::StyleRole,
    accent_rgb: &str,
) -> ColorAttribute {
    use superzej_core::plugin_api::StyleRole;
    match role {
        StyleRole::Default => col(S::Text),
        StyleRole::Accent => theme_color(accent_rgb),
        StyleRole::Warning => theme_color(theme::AMBER),
        StyleRole::Error => theme_color(theme::RED),
        StyleRole::Faint => col(S::Faint),
    }
}

/// Clear the logical back-buffer before composing a new frame. This is not a
/// physical terminal clear: `BufferedTerminal` still diffs this logical state
/// against its prior frame and emits only changed cells.
pub fn clear_frame(surface: &mut Surface) {
    surface.add_change(Change::ClearScreen(col(S::Bg0)));
}

/// Fill `rect` with spaces on `bg` (a solid background block).
pub fn fill(surface: &mut Surface, rect: Rect, bg: ColorAttribute) {
    let row = " ".repeat(rect.cols);
    for r in 0..rect.rows {
        surface.add_change(Change::CursorPosition {
            x: Position::Absolute(rect.x),
            y: Position::Absolute(rect.y + r),
        });
        surface.add_change(Change::Attribute(AttributeChange::Background(bg)));
        surface.add_change(Change::Attribute(AttributeChange::Foreground(
            ColorAttribute::Default,
        )));
        surface.add_change(Change::Text(row.clone()));
    }
}

/// A row context menu (item 27): a short list of actions scoped to the row the
/// cursor sat on when it opened.
#[derive(Debug, Clone, Default)]
pub struct RowMenu {
    /// Visible-row index the menu is anchored to (where it's drawn).
    pub anchor: usize,
    pub entries: Vec<RowMenuEntry>,
    pub cursor: usize,
    /// The stable pin_key of the row this menu was opened for.
    pub target_pin_key: String,
}

#[derive(Debug, Clone)]
pub struct RowMenuEntry {
    pub label: String,
    /// A stable id the event loop dispatches on (e.g. "open", "close", "pin").
    pub id: String,
}

/// What the chrome needs to paint a frame. Populated from session state + DB +
/// git by the host; kept renderer-agnostic so it's unit-testable.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct FrameModel {
    /// The active worktree group's name ("app/feat") — the tabbar's left label.
    pub worktree: String,
    pub ai_metrics: Option<AiMetrics>,
    /// Live embedded-agent activity (ACP `session/update`), shown as a chip.
    pub agent_activity: Option<AgentActivity>,
    /// The active worktree's tab chip titles (tabs live WITHIN a worktree).
    pub tabs: Vec<String>,
    /// Index of the active chip in `tabs`.
    pub active_tab: usize,
    /// The structured workspace tree. Replaces the old flat `Vec<String>`:
    /// rows carry kind/depth/status so the renderer composes glyphs itself.
    pub sidebar_rows: Vec<crate::sidebar::SidebarRow>,
    /// Selection cursor: an index into the *visible* rows of `sidebar_rows`.
    pub sidebar_selected: usize,
    /// True when the sidebar currently owns keyboard focus (drives the
    /// focus indicator in [`draw_sidebar`]; the Ctrl+1..9 workspace digit hints
    /// are always shown, regardless of focus).
    pub sidebar_focused: bool,
    /// Active fuzzy-filter query echoed in the header (empty = none).
    pub sidebar_filter: String,
    /// True while the filter input sub-mode is capturing keystrokes.
    pub sidebar_filtering: bool,
    /// The current sort mode, shown in the header.
    pub sidebar_sort: crate::sidebar::SortMode,
    /// Row indices (into the visible list) that are multi-selected.
    pub sidebar_marked: std::collections::HashSet<usize>,
    /// When `Some`, an open row context menu: (anchor visible-row index,
    /// entries, menu cursor).
    pub sidebar_menu: Option<RowMenu>,
    /// Top visible-row index of the scroll window (clamped by `build_sidebar`
    /// so the cursor stays in view). Mirrors `SidebarState::scroll`.
    pub sidebar_scroll: usize,
    /// True when the sidebar is in its slim collapsed rail mode (activity dots
    /// + initials only); false renders the full panel.
    pub sidebar_rail: bool,
    /// Pending `superzej open` focus intents claimed from the DB mailbox by
    /// this hydration pass. Drained by the run-loop model drain BEFORE the
    /// model swap (never rendered, never part of `hydration_eq`).
    pub intents: Vec<superzej_core::store::IntentRow>,
    /// A cold worktree switch blanked the panel (switch-cache miss) and its
    /// hydration hasn't landed yet: the panel draws its skeleton placeholder
    /// instead of a void. Loop-transient (set by `WorktreeSlice::clear`,
    /// cleared by the next model swap); never part of `hydration_eq`.
    pub panel_pending: bool,
    /// Data carriers populated by the hydration thread and consumed by the
    /// event loop to (re)derive `sidebar_rows`. The `(slug, display, kind)`
    /// workspace list in display order (`kind` = "repo" | "dir"), and
    /// per-worktree git/agent/activity status.
    pub sidebar_workspaces: Vec<(String, String, String, String)>,
    pub sidebar_status: crate::sidebar::SidebarStatus,
    /// OSC window title per worktree path (the active tab's focused pane).
    /// Collected on the main loop from the live panes table — NOT on the
    /// hydration thread, which has no pane access — and used to compose the
    /// dynamic sidebar row title. Re-filled each frame before rendering.
    pub sidebar_window_titles: std::collections::BTreeMap<String, String>,
    /// Worktrees registered for workspaces NOT loaded in the session (their
    /// sidebar rows switch workspace on activate).
    pub sidebar_db_worktrees: Vec<crate::sidebar::DbWorktree>,
    /// All folders for loaded workspaces, straight from DB, used by row builder.
    pub sidebar_db_folders: Vec<superzej_core::models::FolderRow>,
    /// All terminals, straight from DB, used by row builder.
    pub sidebar_db_terminals: Vec<superzej_core::models::TerminalRow>,
    /// `[disk].warn_threshold_gb`: the worktree-usage warning threshold (GiB)
    /// used by the `szhost disk` CLI. The statusbar disk badge is now a low
    /// *free-space* alert (`stats.disk_free_pct` vs `[stats]` thresholds), not a
    /// usage-sum trip. Config-derived, set in `build_model`.
    pub disk_warn_threshold_gb: u64,
    /// Active worktree's total size (bytes), for the bottom `disk` widget next
    /// to LOC. From the off-loop scan cache; `None` until first scanned.
    pub active_worktree_disk: Option<u64>,
    /// Do-not-disturb active (item 426): drives the statusbar DND chip. Set each
    /// frame from the notification runtime.
    pub notify_dnd: bool,
    /// Active notification routing mode (item 427; `""` = default). Shown as a
    /// statusbar chip when non-empty.
    pub notify_mode: String,
    /// True if the last input was mouse activity.
    pub panel: crate::panel::PanelData,
    /// True when the right panel currently owns keyboard focus.
    pub panel_focused: bool,
    /// True while the masthead / statusbar own the keyboard (Ctrl+Up/Down
    /// from the center) — the bar renders raised so the focus is visible.
    pub masthead_focused: bool,
    pub statusbar_focused: bool,
    /// Index of the selected item in the masthead's navigable cluster (when the
    /// masthead owns focus). Clamped to the visible item count each frame.
    pub masthead_sel: usize,
    /// Index of the selected item in the statusbar's navigable right cluster
    /// (config widgets followed by the always-on badges).
    pub statusbar_sel: usize,
    /// True when the center zone owns keyboard focus (drives the focused
    /// pane's light-blue frame ring; sidebar/panel focus dims every ring).
    pub center_focused: bool,
    /// True while the Ctrl+g keybind lock is on (statusbar indicator).
    pub key_locked: bool,
    /// True while a zone is zoomed fullscreen (statusbar indicator).
    pub zoomed: bool,
    /// True while sync-panes broadcast is active (statusbar indicator, item 96).
    pub sync_panes: bool,
    /// Transient message (errors, "Config reloaded", copy confirmations).
    pub status: String,
    /// Warm-spare-pool state for the active workspace as `(ready, target)`, or
    /// `None` to hide the sidebar chip (no provider env / pool disabled). Set by
    /// the loop's pool maintainer, not hydration.
    pub pool: Option<(usize, usize)>,
    /// Context-dependent keybind hints for the bottom bar as (chord, label)
    /// pairs (rebuilt per focus zone — the dynamic replacement for per-panel
    /// help rows). Rendered as key chips + dim labels.
    pub keyhints: Vec<(String, String)>,
    /// The input-mode chip letter for the statusbar ("N", "V", "I", "E").
    pub mode_chip: String,
    /// Latest system stats reading for the top bar.
    pub stats: superzej_metrics::StatsSnapshot,
    /// Latest Prometheus scrape state for the sidebar metrics section.
    pub metrics: crate::metrics::MetricsState,
    /// tokei per-language report for the active worktree (bottom-bar widget +
    /// detail table).
    pub loc: Option<superzej_core::loc::LocReport>,
    /// Widget-bar layout (`[bars]`) and stat icons (`[stats]`).
    pub bars: superzej_core::config::BarsConfig,
    pub stats_icons: superzej_core::config::StatsConfig,
    pub accent: String,
    /// Pin chips for the tabbar (label + status glyph), in `Alt-N` order.
    pub pins: Vec<crate::pins::PinChip>,
    /// Active ingress shares (`[share]`) for the current worktree — feeds the
    /// statusbar badge + the System ▸ Share panel section. Synced from the
    /// `ShareSupervisor` (loop-local), not from hydration.
    pub shares: Vec<crate::share::ShareView>,
    /// Active auto port forwards (`[forward]`) for the current worktree — feeds
    /// the System ▸ Forward panel section + the `o` open-in-browser action.
    /// Synced from the `ForwardSupervisor` (loop-local), not from hydration.
    pub forwards: Vec<crate::forward::ForwardView>,
    /// Deterministic container name for the active worktree path. The sandbox
    /// panel uses this to show the sandbox for the selected worktree instead of
    /// the first superzej-owned container on the machine.
    pub active_container_name: String,
    /// DB-stored sandbox backend label for the active worktree (e.g. "bwrap",
    /// "podman-rootless", "host"). Used to show non-OCI sandboxes (bwrap,
    /// systemd) as green even though they have no container entry.
    pub active_sandbox_backend: String,
    /// Terse placement kind for the active worktree (`ssh`, `mosh`, `k8s`, or the
    /// provider id like `sprite`); `None` when it runs locally. Shown as a chip in
    /// the center tab bar's right-aligned env cluster.
    pub active_placement_kind: Option<String>,
    /// Full placement detail for the active worktree (`ssh:host`, `k8s:ns/pod`,
    /// `sprite:<id>`); `None` when it runs locally. Shown in the System → Sandbox
    /// panel section (the terse `active_placement_kind` covers the tab-bar chip).
    pub active_placement_label: Option<String>,
    /// Running containers (superzej-owned first) for the SANDBOXES section.
    pub containers: Vec<superzej_core::sandbox::ContainerInfo>,
    /// Health of the active worktree's container (updated on the container refresh tick).
    pub container_health: ContainerHealth,
    /// Recent audit events for the active worktree's container (last 10, newest first).
    pub container_events: Vec<superzej_core::models::ContainerEvent>,
    /// Unified per-worktree activity timeline: the sandbox audit log and the
    /// LLM-proxy request/spend log merged and time-sorted (newest first). The
    /// cross-backend "what is this worktree doing" view, rendered in System →
    /// sandbox. Built off-loop by [`merge_timeline`](superzej_core::models::merge_timeline).
    pub timeline: Vec<superzej_core::models::TimelineEvent>,
    /// Names of orphan containers removed at startup GC (shown once in System panel).
    pub startup_orphans_removed: Vec<String>,
    /// Top-level app-tab chip labels in masthead order: `work` first, then the
    /// embedded apps (`chat`, …). Empty hides the strip entirely.
    pub app_tabs: Vec<String>,
    /// Index of the active app tab in [`Self::app_tabs`] (0 = `work`).
    pub active_app: usize,
    /// Ordered launch steps shown in the loading screen while the first pane
    /// is spawning. Empty = no loading screen. Cleared once a live pane exists.
    pub load_steps: Vec<LoadStep>,
    /// Context shown beneath the loading steps: `(key, value)` facts about where
    /// the pane is coming up — env, placement, provider/sandbox, connect, workdir.
    /// Empty hides the block. Only meaningful while [`Self::load_steps`] is set.
    pub load_context: Vec<(String, String)>,
}

/// Health of the active worktree's container.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ContainerHealth {
    /// No OCI backend in use, or health not yet checked.
    #[default]
    Unknown,
    /// Container is running and all bind-mounts are present.
    Healthy,
    /// Container exists but mounts are stale or the container is paused.
    Degraded(String),
}

/// One step in the pane-launch progress display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadStep {
    pub label: String,
    pub state: StepState,
    /// Optional dim sub-line under the step — a live status for the active step
    /// or the captured error for a failed one. Rendered indented below `label`.
    pub detail: Option<String>,
}

/// Visual state of a [`LoadStep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Not started yet.
    Pending,
    /// Currently running.
    Active,
    /// Completed successfully.
    Done,
    /// Failed.
    Failed,
}

impl LoadStep {
    pub fn pending(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: StepState::Pending,
            detail: None,
        }
    }
    pub fn active(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: StepState::Active,
            detail: None,
        }
    }
    pub fn done(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: StepState::Done,
            detail: None,
        }
    }
    pub fn failed(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: StepState::Failed,
            detail: None,
        }
    }
    /// Attach a dim sub-line (status / error) under the step.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let d = detail.into();
        self.detail = (!d.trim().is_empty()).then_some(d);
        self
    }
}

impl FrameModel {
    pub fn accent_or_default(&self) -> &str {
        if self.accent.is_empty() {
            theme::TEAL
        } else {
            &self.accent
        }
    }

    // NOTE: `hydration_eq` (the idle-guard equality) lives in `model_eq.rs`.
}

use crate::nav::worktree_parts;

/// Where the right-aligned pin chips begin in the tab strip (the tab chips
/// must stop before this column).
fn pin_chips_start(model: &FrameModel, strip: Rect) -> usize {
    let end = strip.x + strip.cols;
    if model.pins.is_empty() {
        return end;
    }
    let total: usize = model
        .pins
        .iter()
        .map(|c| format!(" {} {} ", c.glyph, c.label).chars().count())
        .sum();
    end.saturating_sub(total).max(strip.x)
}

/// The tab chips' `(x, width, tab index)` spans inside the center tab bar —
/// the single source of truth consumed by BOTH [`draw_center_tabs`]
/// (placement) and [`center_tab_hit`] (mouse), so they can never drift apart.
fn strip_chip_spans(model: &FrameModel, strip: Rect) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    if strip.rows == 0 || strip.cols == 0 {
        return spans;
    }
    let end = pin_chips_start(model, strip)
        .saturating_sub(crate::tabbar_env::env_cluster_width(model))
        .max(strip.x);
    let mut x = strip.x + 1;
    if let Some((ws, leaf)) = worktree_parts(model) {
        if !ws.is_empty() {
            x += ws.chars().count() + 3; // "WS ▸ "
        }
        x += leaf.chars().count() + 2; // "leaf" + gap
    }
    for (i, title) in model.tabs.iter().enumerate() {
        let w = title.chars().count() + 2; // " {title} "
        if x + w > end {
            break;
        }
        spans.push((x, w, i));
        x += w + 1;
    }
    spans
}

/// Which tab chip sits at column `x` of the center tab bar (mouse hit-test).
/// Shares its span math with the renderer via [`strip_chip_spans`].
pub fn center_tab_hit(model: &FrameModel, strip: Rect, x: usize) -> Option<usize> {
    strip_chip_spans(model, strip)
        .into_iter()
        .find(|(sx, w, _)| x >= *sx && x < sx + w)
        .map(|(_, _, i)| i)
}

/// Brand slot widths for the masthead text logo.
/// " ◆ superzej v0.0.0 " — glyph + name + version…
const BRAND_FULL_COLS: usize = 20;
/// …or just " ◆ superzej " on narrower screens…
const BRAND_COMPACT_COLS: usize = 13;
/// …and nothing below this (content wins).
const MASTHEAD_FULL_MIN_COLS: usize = 96;
const MASTHEAD_COMPACT_MIN_COLS: usize = 60;

/// How many masthead columns the brand occupies at a given terminal width.
/// The stats cluster starts after these columns.
pub fn masthead_brand_cols(cols: usize) -> usize {
    if cols >= MASTHEAD_FULL_MIN_COLS {
        BRAND_FULL_COLS
    } else if cols >= MASTHEAD_COMPACT_MIN_COLS {
        BRAND_COMPACT_COLS
    } else {
        0
    }
}

/// The total columns a kept subset of the right cluster occupies
/// (widget widths + " · " separators + 1 right margin).
fn cluster_width(parts: &[(String, usize)], kept: &[usize]) -> usize {
    if kept.is_empty() {
        return 0;
    }
    kept.iter().map(|&i| parts[i].1).sum::<usize>() + 3 * (kept.len() - 1) + 1
}

/// Drop right-cluster widgets in priority order until the cluster fits `avail`
/// columns — softest stats shed first, leaving cpu/mem/net/battery longest.
/// (The brand/logo is the caller's final sacrifice.) Returns the surviving
/// indices in display order.
fn fit_stats_cluster(parts: &[(String, usize)], avail: usize) -> Vec<usize> {
    let mut kept: Vec<usize> = (0..parts.len()).collect();
    for victim in [
        "date", "uptime", "load", "freq", "swap", "temp", "disk", "gpu",
    ] {
        if cluster_width(parts, &kept) <= avail {
            break;
        }
        kept.retain(|&i| parts[i].0 != victim);
    }
    kept
}

/// The single-row masthead: a regular-font text brand on the left and the
/// `[bars]`-configured stats cluster on the right. When width runs short the
/// masthead degrades gracefully — date drops first, then GPU, then the brand
/// shrinks and finally disappears — so nothing ever clips mid-glyph. (The
/// pixel wordmark survives on the empty-state splash.)
pub fn draw_masthead(
    surface: &mut Surface,
    layout: &crate::layout::ChromeLayout,
    model: &FrameModel,
) {
    let rect = layout.masthead;
    if rect.rows == 0 || rect.cols == 0 {
        return;
    }
    let bar_bg = if model.masthead_focused {
        S::Raise
    } else {
        S::Panel
    };
    fill(surface, rect, col(bar_bg));
    let accent = theme_color(model.accent_or_default());
    let bg = col(bar_bg);

    // Resolve the visible cluster + brand width once (shared with navigation /
    // hit-testing via `masthead_fit`).
    let (brand_cols, items) = masthead_fit(model, rect.cols);

    if brand_cols > 0 {
        draw_text(
            surface,
            rect.x + 1,
            rect.y,
            &format!("{} ", crate::caps::active_glyphs().diamond_filled),
            accent,
            bg,
            2,
        );
        draw_text(surface, rect.x + 3, rect.y, "superzej", col(S::Text), bg, 8);
        if brand_cols >= BRAND_FULL_COLS {
            draw_text(
                surface,
                rect.x + 12,
                rect.y,
                concat!("v", env!("CARGO_PKG_VERSION")),
                col(S::Ghost),
                bg,
                brand_cols.saturating_sub(13),
            );
        }
    }

    draw_masthead_cluster(
        surface,
        layout.masthead_stats_row(),
        &items,
        brand_cols,
        bg,
        model,
    );
    // Top-level app tabs sit right after the brand; the masthead-left widgets
    // (breadcrumb/clock/…) start after them.
    let stats_row = layout.masthead_stats_row();
    let chips_start = stats_row.x + brand_cols.max(1);
    let chips_w = draw_app_chips(surface, stats_row, model, chips_start);
    draw_masthead_left(surface, stats_row, model, brand_cols + chips_w);
}

/// The top-level app-tab chips (`work`, `chat`, …) in the masthead, just after
/// the brand. Active chip in the focus color on a focus-tinted pill; the rest
/// quiet on the bar. Returns the columns consumed (0 when there are no tabs),
/// so the caller can place the remaining masthead-left widgets after them.
fn draw_app_chips(surface: &mut Surface, rect: Rect, model: &FrameModel, start_x: usize) -> usize {
    if model.app_tabs.is_empty() || rect.rows == 0 {
        return 0;
    }
    let bar_bg = if model.masthead_focused {
        S::Raise
    } else {
        S::Panel
    };
    let focus = col(S::Focus);
    let dim = col(S::Dim);
    let pill = theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.28));
    let end = rect.x + rect.cols;
    let mut x = start_x;
    for (i, label) in model.app_tabs.iter().enumerate() {
        if x >= end {
            break;
        }
        let chip = format!(" {label} ");
        let (fg, chip_bg) = if i == model.active_app {
            (focus, pill)
        } else {
            (dim, col(bar_bg))
        };
        draw_text(
            surface,
            x,
            rect.y,
            &chip,
            fg,
            chip_bg,
            end.saturating_sub(x),
        );
        x += chip.chars().count() + 1; // trailing gap between chips
    }
    x.min(end).saturating_sub(start_x)
}

/// The center column's tab bar, directly below the divider: the worktree
/// label, the tab chips in a recessed container, and the pin chips
/// right-aligned. Tabs live WITHIN the active worktree — `model.tabs` is that
/// worktree's chip strip only.
fn draw_center_tabs(surface: &mut Surface, strip: Rect, model: &FrameModel) {
    if strip.rows == 0 || strip.cols == 0 {
        return;
    }
    let accent = theme_color(model.accent_or_default());
    let dim = col(S::Dim);
    let bg = col(S::Bg0);
    let end = strip.x + strip.cols;
    fill(
        surface,
        Rect {
            x: strip.x,
            y: strip.y,
            cols: strip.cols,
            rows: 1,
        },
        bg,
    );

    draw_pin_chips(surface, strip, end, model, accent, dim);
    // Env cluster (sandbox `(backend)` + remote `[kind]`) right-aligned just
    // left of the pins; its left edge is the boundary the tab chips stop before.
    let pins_start = pin_chips_start(model, strip);
    let chips_end = crate::tabbar_env::draw_env_chips(surface, strip, pins_start, model);

    let mut x = strip.x + 1;
    if let Some((ws, leaf)) = worktree_parts(model) {
        if !ws.is_empty() {
            draw_text(
                surface,
                x,
                strip.y,
                &ws,
                col(S::Dim),
                bg,
                chips_end.saturating_sub(x),
            );
            x += ws.chars().count();
            draw_text(
                surface,
                x,
                strip.y,
                " \u{25b8} ",
                col(S::Ghost),
                bg,
                chips_end.saturating_sub(x).min(3),
            );
            x += 3;
        }
        draw_text(
            surface,
            x,
            strip.y,
            &leaf,
            accent,
            bg,
            chips_end.saturating_sub(x),
        );
        x += leaf.chars().count();
        // Issue badge: show the first linked issue's status + number next to
        // the active worktree name when at least one issue is linked.
        if let Some(issue_id) = model.panel.tracker_links.first()
            && let Some(issue) = model
                .panel
                .tracker_issues
                .iter()
                .find(|i| &i.id == issue_id)
        {
            let badge = format!(" ◈{}", issue.number);
            let avail = chips_end.saturating_sub(x);
            if avail >= badge.chars().count() {
                draw_text(surface, x, strip.y, &badge, col(S::Accent), bg, avail);
            }
        }
    }

    // The chips render as padded pills: the active tab in the focus color on
    // a raised focus tint, inactive tabs quiet on a raised surface — easy to
    // scan, clearly grouped, clearly clickable.
    let focus = col(S::Focus);
    let pill = theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.28));
    for (sx, w, i) in strip_chip_spans(model, strip) {
        let chip = format!(" {} ", model.tabs[i]);
        let (fg, chip_bg) = if i == model.active_tab {
            (focus, pill)
        } else {
            (dim, col(S::Panel))
        };
        draw_text(surface, sx, strip.y, &chip, fg, chip_bg, w);
    }
}

/// Render pin chips (`glyph label`) right-aligned in the tab-strip area.
/// Returns the left-most x the chips occupy, so tab labels can stop before them.
fn draw_pin_chips(
    surface: &mut Surface,
    content: Rect,
    content_end: usize,
    model: &FrameModel,
    accent: ColorAttribute,
    dim: ColorAttribute,
) -> usize {
    if model.pins.is_empty() {
        return content_end;
    }
    // Each chip reads " <glyph> <label> " (the leading index is implicit Alt-N).
    let chips: Vec<String> = model
        .pins
        .iter()
        .map(|c| format!(" {} {} ", c.glyph, c.label))
        .collect();
    let total: usize = chips.iter().map(|s| s.chars().count()).sum();
    let mut x = content_end.saturating_sub(total).max(content.x);
    let chips_start = x;
    let bg = col(S::Panel);
    for (chip, pin) in chips.iter().zip(model.pins.iter()) {
        if x >= content_end {
            break;
        }
        // Running pins read in the accent; stopped/failed read dim.
        let fg = if pin.glyph == crate::pins::PinHealth::Running.glyph() {
            accent
        } else {
            dim
        };
        let max = content_end.saturating_sub(x);
        draw_text(surface, x, content.y, chip, fg, bg, max);
        x += chip.chars().count();
    }
    chips_start
}

/// A resolved masthead widget: its text plus the color it earned (stats turn
/// amber/red as they cross pressure thresholds; quiet otherwise).
pub struct MastheadWidget {
    text: String,
    fg: ColorAttribute,
}

/// The stable identity of a navigable bar item — what focus selects, what Enter
/// (or a click) opens a detail view for, and the key the popup-content mapping
/// dispatches on. Config widgets carry their `[bars]` id string; the always-on
/// statusbar badges are their own enumerated kinds (they are NOT in `[bars]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BarItemId {
    /// A `[bars]` config widget (e.g. "cpu", "mem", "pr", "tests").
    Widget(String),
    /// One of the hard-coded statusbar badge blocks.
    Badge(BarBadge),
}

/// The statusbar's always-on badges, in render order. Each maps to one of the
/// imperative badge blocks in [`draw_statusbar`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarBadge {
    Notifications,
    /// Needs-you rollup: worktrees whose attention tier is T0–T2.
    Attention,
    Ci,
    MergeQueue,
    DiskWarn,
    Ingress,
    Media,
    AiCost,
    Agent,
    Zoom,
    Lock,
    Sync,
}

impl BarItemId {
    /// Whether activating this item (Enter / click) opens a detail view. Every
    /// navigable item has one today; kept as a seam so a future inert item can
    /// opt out (Enter becomes a no-op rather than an empty modal).
    pub fn has_detail(&self) -> bool {
        true
    }
}

/// Utilization pressure for threshold coloring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Normal,
    Warn,
    Crit,
}

/// Percent-based pressure: ≥80% warns, ≥92% is critical.
fn stat_level(pct: u8) -> Level {
    if pct >= 92 {
        Level::Crit
    } else if pct >= 80 {
        Level::Warn
    } else {
        Level::Normal
    }
}

/// Ratio-based pressure (memory): ≥85% warns, ≥95% is critical.
/// Level for a *free-space* percentage: low free is bad, so the thresholds are
/// inverted relative to [`stat_level`] (warn ≥ critical).
fn free_level(free: u8, warn: u8, critical: u8) -> Level {
    if free <= critical {
        Level::Crit
    } else if free <= warn {
        Level::Warn
    } else {
        Level::Normal
    }
}

fn ratio_level(used: f32, total: f32) -> Level {
    if total <= 0.0 {
        return Level::Normal;
    }
    let r = used / total;
    if r >= 0.95 {
        Level::Crit
    } else if r >= 0.85 {
        Level::Warn
    } else {
        Level::Normal
    }
}

/// Temperature pressure (°C): ≥85 is critical, ≥70 warns.
fn temp_level(c: f32) -> Level {
    if c >= 85.0 {
        Level::Crit
    } else if c >= 70.0 {
        Level::Warn
    } else {
        Level::Normal
    }
}

/// Human uptime: `3d4h`, `4h12m`, or `12m`.
fn fmt_uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

fn level_color(level: Level) -> ColorAttribute {
    match level {
        Level::Normal => col(S::Dim),
        Level::Warn => theme_color(theme::AMBER),
        Level::Crit => theme_color(theme::RED),
    }
}

/// Resolve a masthead widget id to its display text + color; `None` hides the
/// widget (no data yet, GPU absent, unknown id).
fn masthead_widget(id: &str, model: &FrameModel) -> Option<MastheadWidget> {
    let s = &model.stats;
    let ic = &model.stats_icons;
    let w = |text: String, fg: ColorAttribute| MastheadWidget { text, fg };
    match id {
        "brand" => Some(w(
            format!("superzej v{}", env!("CARGO_PKG_VERSION")),
            theme_color(model.accent_or_default()),
        )),
        "cpu" => s.cpu_pct.map(|p| {
            w(
                format!("{} {p:>2}%", ic.cpu_icon),
                level_color(stat_level(p)),
            )
        }),
        "mem" => s.mem_gib.map(|(u, t)| {
            w(
                format!("{} {u:.1}/{t:.0}G", ic.mem_icon),
                level_color(ratio_level(u, t)),
            )
        }),
        "gpu" => s.gpu_pct.map(|p| {
            w(
                format!("{} {p:>2}%", ic.gpu_icon),
                level_color(stat_level(p)),
            )
        }),
        "temp" => s.cpu_temp_c.map(|c| {
            w(
                format!("{} {c:.0}\u{00b0}C", ic.temp_icon),
                level_color(temp_level(c)),
            )
        }),
        "swap" => s.swap_gib.map(|(u, t)| {
            w(
                format!("{} {u:.1}/{t:.0}G", ic.swap_icon),
                level_color(ratio_level(u, t)),
            )
        }),
        "freq" => s.cpu_freq_mhz.map(|mhz| {
            w(
                format!("{} {:.1}GHz", ic.freq_icon, mhz as f32 / 1000.0),
                col(S::Dim),
            )
        }),
        "load" => s
            .load_avg
            .map(|(one, _, _)| w(format!("{} {one:.2}", ic.load_icon), col(S::Dim))),
        "uptime" => s.uptime_secs.map(|secs| {
            w(
                format!("{} {}", ic.uptime_icon, fmt_uptime(secs)),
                col(S::Dim),
            )
        }),
        // Disk shows *free* space, so the sense is inverted: low free is bad.
        "disk" => s.disk_free_pct.map(|free| {
            w(
                format!("{} {free:>2}%", ic.disk_icon),
                level_color(free_level(free, ic.disk_free_warn, ic.disk_free_critical)),
            )
        }),
        "net" => s.net_bps.map(|(rx, tx)| {
            w(
                format!(
                    "{} {}{} {}{}",
                    ic.net_icon,
                    crate::caps::active_glyphs().arrow_down,
                    superzej_metrics::fmt_rate(rx),
                    crate::caps::active_glyphs().arrow_up,
                    superzej_metrics::fmt_rate(tx)
                ),
                col(S::Dim),
            )
        }),
        "battery" => s.battery.map(|(p, on_ac)| {
            // On AC wins: bolt glyph + orange text, even when low (this also
            // covers a charge-capped battery, which sits plugged in not
            // charging). Otherwise the battery glyph, red at/below the warn
            // threshold and quiet above.
            let (icon, fg) = if on_ac {
                (&ic.battery_charging_icon, theme_color(theme::HUE_ORANGE))
            } else if p <= ic.battery_warn {
                (&ic.battery_icon, theme_color(theme::RED))
            } else {
                (&ic.battery_icon, col(S::Dim))
            };
            w(format!("{icon} {p:>2}%"), fg)
        }),
        "date" => Some(w(
            chrono::Local::now()
                .format(&model.bars.date_format)
                .to_string(),
            col(S::Dim),
        )),
        "clock" => Some(w(
            chrono::Local::now()
                .format(&model.bars.clock_format)
                .to_string(),
            col(S::Dim),
        )),
        _ => None,
    }
}

/// `owner/repo` parsed from a forge PR/issue URL
/// (`https://github.com/owner/repo/pull/7` → `owner/repo`).
fn forge_repo_from_url(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let mut parts = rest.split('/');
    let _host = parts.next()?;
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Resolve a BOTTOM-bar widget id to its display text + color.
pub fn bottombar_widget(id: &str, model: &FrameModel) -> Option<MastheadWidget> {
    let w = |text: String, fg: ColorAttribute| MastheadWidget { text, fg };
    match id {
        // "keyhints" is special-cased by draw_statusbar (chip + label segs).
        "keyhints" => None,
        "loc" => model
            .loc
            .as_ref()
            .map(|r| w(format!("{} LOC", r.compact_total()), col(S::Dim))),
        // Active worktree's disk usage (size of its checkout incl. target/),
        // from the off-loop scan; sits next to LOC. Hidden until first scanned.
        "disk" => model
            .active_worktree_disk
            .map(|b| w(superzej_core::disk::human(b), col(S::Dim))),
        // Forge + PR number, colored by state: open green, draft/queued
        // amber, closed and merged purple. Hidden when no PR exists.
        "pr" => model.panel.pr.as_ref().map(|pr| {
            let repo = forge_repo_from_url(&pr.url).unwrap_or_default();
            let fg = if pr.is_draft {
                theme_color(theme::AMBER)
            } else {
                match pr.state.as_str() {
                    "OPEN" => theme_color(theme::GREEN),
                    "CLOSED" | "MERGED" => theme_color(theme::PURPLE),
                    _ => theme_color(theme::AMBER),
                }
            };
            if repo.is_empty() {
                w(format!("#{}", pr.number), fg)
            } else {
                w(format!("{repo} #{}", pr.number), fg)
            }
        }),
        // Test rollup (item 517): ✓passed plus ✗failed when any fail; green
        // when the last run was all-pass, red when anything failed. Hidden
        // until a run has produced counts (mirrors the panel Tests section).
        "tests" => model.panel.tests.as_ref().and_then(|t| {
            if t.passed == 0 && t.failed == 0 {
                return None;
            }
            let fg = if t.failed > 0 {
                theme_color(theme::RED)
            } else {
                theme_color(theme::GREEN)
            };
            let g = crate::caps::active_glyphs();
            let text = if t.failed > 0 {
                format!("{}{} {}{}", g.check, t.passed, g.cross, t.failed)
            } else {
                format!("{}{}", g.check, t.passed)
            };
            Some(w(text, fg))
        }),
        "status" => (!model.status.is_empty()).then(|| w(model.status.clone(), col(S::Dim))),
        _ => None,
    }
}

/// `[bars] top_left` widgets drawn after the brand slot (the `brand` id is
/// owned by the masthead's slot logic and skipped here).
fn draw_masthead_left(surface: &mut Surface, rect: Rect, model: &FrameModel, brand_cols: usize) {
    if rect.rows == 0 || rect.cols == 0 {
        return;
    }
    let bg = col(if model.masthead_focused {
        S::Raise
    } else {
        S::Panel
    });
    let sep = " \u{00b7} ";
    let end = rect.x + rect.cols;
    let mut x = rect.x + brand_cols.max(1);
    let mut first = true;
    for id in &model.bars.top_left {
        if id == "brand" {
            continue;
        }
        let Some(wd) = masthead_widget(id, model) else {
            continue;
        };
        if !first && x < end {
            draw_text(surface, x, rect.y, sep, col(S::Ghost), bg, 3);
            x += 3;
        }
        draw_text(
            surface,
            x,
            rect.y,
            &wd.text,
            wd.fg,
            bg,
            end.saturating_sub(x),
        );
        x += wd.text.chars().count();
        first = false;
    }
}

/// The masthead's resolved layout for a width: the brand column count and the
/// visible right-cluster items (stable id + resolved widget) in display order,
/// after the graceful width-degradation drop. The single source of truth shared
/// by [`draw_masthead`] (placement) and [`masthead_item_spans`] (navigation +
/// hit-testing), so which items show — and in what order — never disagrees.
fn masthead_fit(model: &FrameModel, cols: usize) -> (usize, Vec<(BarItemId, MastheadWidget)>) {
    let parts: Vec<(String, MastheadWidget)> = model
        .bars
        .top_right
        .iter()
        .filter_map(|id| masthead_widget(id, model).map(|w| (id.clone(), w)))
        .collect();
    let widths: Vec<(String, usize)> = parts
        .iter()
        .map(|(id, w)| (id.clone(), w.text.chars().count()))
        .collect();
    let mut brand_cols = masthead_brand_cols(cols);
    let kept = loop {
        let avail = cols.saturating_sub(brand_cols.max(1));
        let kept = fit_stats_cluster(&widths, avail);
        if cluster_width(&widths, &kept) <= avail || brand_cols == 0 {
            break kept;
        }
        brand_cols = if brand_cols >= BRAND_FULL_COLS {
            BRAND_COMPACT_COLS
        } else {
            0
        };
    };
    let items = kept
        .into_iter()
        .map(|i| {
            let (id, w) = &parts[i];
            (
                BarItemId::Widget(id.clone()),
                MastheadWidget {
                    text: w.text.clone(),
                    fg: w.fg,
                },
            )
        })
        .collect();
    (brand_cols, items)
}

/// The right cluster's `(x, width)` cell spans, right-aligned with ` · `
/// separators. Mirrors [`draw_masthead_cluster`]'s placement exactly (the +1
/// right margin, the brand floor) so highlight and hit-testing land on the
/// painted glyphs.
fn masthead_cluster_spans(widths: &[usize], rect: Rect, brand_cols: usize) -> Vec<(usize, usize)> {
    if widths.is_empty() {
        return Vec::new();
    }
    let end = rect.x + rect.cols;
    let total: usize = widths.iter().sum::<usize>() + 3 * (widths.len() - 1) + 1;
    let mut rx = end
        .saturating_sub(total)
        .max(rect.x + brand_cols.max(1) + 1);
    let mut out = Vec::with_capacity(widths.len());
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            rx += 3;
        }
        out.push((rx, *w));
        rx += w;
    }
    out
}

/// The masthead right cluster's absolute `(id, Rect)` spans for the loop's mouse
/// hit-testing and detail-popup anchoring.
pub fn masthead_item_spans(
    model: &FrameModel,
    layout: &crate::layout::ChromeLayout,
) -> Vec<(BarItemId, Rect)> {
    let rect = layout.masthead_stats_row();
    if rect.rows == 0 || rect.cols == 0 {
        return Vec::new();
    }
    let (brand_cols, items) = masthead_fit(model, rect.cols);
    let widths: Vec<usize> = items.iter().map(|(_, w)| w.text.chars().count()).collect();
    masthead_cluster_spans(&widths, rect, brand_cols)
        .into_iter()
        .zip(items)
        .map(|((x, w), (id, _))| {
            (
                id,
                Rect {
                    x,
                    y: rect.y,
                    cols: w,
                    rows: 1,
                },
            )
        })
        .collect()
}

/// The pre-fitted right cluster, right-aligned with `·` separators, with the
/// focused item drawn as a focus-tinted selection block. The caller has already
/// dropped whatever wouldn't fit (see `draw_masthead` / `masthead_fit`).
fn draw_masthead_cluster(
    surface: &mut Surface,
    rect: Rect,
    parts: &[(BarItemId, MastheadWidget)],
    brand_cols: usize,
    bg: ColorAttribute,
    model: &FrameModel,
) {
    if rect.rows == 0 || rect.cols == 0 || parts.is_empty() {
        return;
    }
    let sep = " \u{00b7} ";
    let widths: Vec<usize> = parts.iter().map(|(_, w)| w.text.chars().count()).collect();
    let spans = masthead_cluster_spans(&widths, rect, brand_cols);
    let sel = if model.masthead_focused {
        Some(model.masthead_sel.min(parts.len().saturating_sub(1)))
    } else {
        None
    };
    let pill = theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.28));
    let focus_fg = col(S::Focus);
    for (i, ((_, p), (x, w))) in parts.iter().zip(spans.iter()).enumerate() {
        if i > 0 {
            draw_text(
                surface,
                x.saturating_sub(3),
                rect.y,
                sep,
                col(S::Ghost),
                bg,
                3,
            );
        }
        if sel == Some(i) {
            fill(
                surface,
                Rect {
                    x: *x,
                    y: rect.y,
                    cols: *w,
                    rows: 1,
                },
                pill,
            );
            draw_text(surface, *x, rect.y, &p.text, focus_fg, pill, *w);
        } else {
            draw_text(surface, *x, rect.y, &p.text, p.fg, bg, *w);
        }
    }
}

/// The statusbar's ordered navigable right-cluster items — the `bottom_right`
/// config widgets followed by the always-on badges — each as its stable id plus
/// the segments that render it (WITHOUT the inter-item separator, which the
/// layout adds). The single source of truth shared by [`draw_statusbar`]
/// (placement + highlight), navigation (item count), and
/// [`statusbar_item_spans`] (mouse + popup anchoring), so they can never drift.
pub fn statusbar_items(model: &FrameModel) -> Vec<(BarItemId, Vec<crate::seg::Seg>)> {
    use crate::seg::{Seg, Tok, seg};
    let mut items: Vec<(BarItemId, Vec<Seg>)> = Vec::new();

    // Config widgets (PR / tests / LOC / disk / transient status).
    for id in &model.bars.bottom_right {
        if let Some(p) = bottombar_widget(id, model) {
            items.push((
                BarItemId::Widget(id.clone()),
                vec![seg(Tok::Attr(p.fg), p.text)],
            ));
        }
    }

    // Do-not-disturb (item 426): a muted amber chip while quiet hours / the
    // manual toggle suppress ephemeral delivery. The active routing mode (item
    // 427) rides alongside it when non-default.
    if model.notify_dnd {
        let suffix = if model.notify_mode.is_empty() {
            String::new()
        } else {
            format!(":{}", model.notify_mode)
        };
        items.push((
            BarItemId::Badge(BarBadge::Notifications),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Amber),
                format!(" \u{25cf} dnd{suffix} "),
            )],
        ));
    } else if !model.notify_mode.is_empty() {
        items.push((
            BarItemId::Badge(BarBadge::Notifications),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Teal),
                format!(" \u{25c9} {} ", model.notify_mode),
            )],
        ));
    }

    // Red ⚑ flag is reserved for attention (Alert priority); a neutral blue inbox
    // chip carries Notice-priority unread. Info-priority events show in neither.
    if model.panel.alert_notifications > 0 {
        items.push((
            BarItemId::Badge(BarBadge::Notifications),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Red),
                format!(" \u{2691} {} ", model.panel.alert_notifications),
            )],
        ));
    } else if model.panel.unread_notifications > 0 {
        items.push((
            BarItemId::Badge(BarBadge::Notifications),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Blue),
                format!(" \u{2709} {} ", model.panel.unread_notifications),
            )],
        ));
    }
    // Needs-you / CI rollup / merge-queue chips live in `statusbar_badges.rs`
    // (extracted from this ratchet-pinned file).
    crate::statusbar_badges::push_attention_badge(model, &mut items);
    crate::statusbar_badges::push_ci_badge(model, &mut items);
    crate::statusbar_badges::push_mq_badge(model, &mut items);
    // Low-free-space badge: trips when the worktrees' filesystem drops to/below
    // `[stats].disk_free_warn` free — amber at the warn line, red at/below
    // `disk_free_critical`. The badge selects into a detailed modal (free/used/
    // total bytes + worktree usage). Silent above the warn line (clean is quiet)
    // and until the stats sampler has produced a reading.
    if let Some(free) = model.stats.disk_free_pct {
        let ic = &model.stats_icons;
        let hue = match free_level(free, ic.disk_free_warn, ic.disk_free_critical) {
            Level::Crit => Some(superzej_core::theme::Hue::Red),
            Level::Warn => Some(superzej_core::theme::Hue::Amber),
            Level::Normal => None,
        };
        if let Some(hue) = hue {
            items.push((
                BarItemId::Badge(BarBadge::DiskWarn),
                vec![Seg::chip(Tok::Hue(hue), format!(" \u{26c1} {free}% free "))],
            ));
        }
    }
    // Ingress-share badge (`[share]`): a ⇅ chip showing how many ports the
    // current worktree exposes. Coloured by reach as a safety affordance — a
    // worktree exposed to the public internet renders AMBER (caution), private
    // team/peer shares render teal. A failed share also shows amber.
    {
        let up = model.shares.iter().filter(|s| s.url.is_some()).count();
        let any_public = model.shares.iter().any(|s| s.public && s.url.is_some());
        let failed = model.shares.iter().filter(|s| s.failed).count();
        if up > 0 {
            let label = if up == 1 {
                match model.shares.iter().find(|s| s.url.is_some()) {
                    Some(s) => format!(" \u{21c5} {} ", s.port),
                    None => " \u{21c5} ".to_string(),
                }
            } else {
                format!(" \u{21c5} {up} ")
            };
            let hue = if any_public {
                superzej_core::theme::Hue::Amber
            } else {
                superzej_core::theme::Hue::Teal
            };
            items.push((
                BarItemId::Badge(BarBadge::Ingress),
                vec![Seg::chip(Tok::Hue(hue), label)],
            ));
        } else if failed > 0 {
            items.push((
                BarItemId::Badge(BarBadge::Ingress),
                vec![Seg::chip(
                    Tok::Hue(superzej_core::theme::Hue::Amber),
                    " \u{21c5} ! ".to_string(),
                )],
            ));
        }
    }
    // Now-playing badge (optional [media] feature): a compact ▶/❚❚ chip with the
    // current track, green while playing and blue while paused. `badge()` returns
    // `None` when nothing is loaded, so the chip is silent when idle.
    if let Some(m) = &model.panel.media
        && let Some(text) = m.badge()
    {
        use superzej_core::media::PlaybackState;
        let hue = match m.state {
            PlaybackState::Playing => superzej_core::theme::Hue::Green,
            _ => superzej_core::theme::Hue::Blue,
        };
        let clipped: String = {
            let max = 30;
            if crate::seg::take_cols(&text, max) != text.as_str() {
                let g = crate::caps::active_glyphs();
                let body = crate::seg::take_cols(&text, max.saturating_sub(3));
                format!("{}{}", body.trim_end(), g.ellipsis)
            } else {
                text
            }
        };
        items.push((
            BarItemId::Badge(BarBadge::Media),
            vec![Seg::chip(Tok::Hue(hue), format!(" {clipped} "))],
        ));
    }
    if let Some(ref metrics) = model.ai_metrics {
        items.push((
            BarItemId::Badge(BarBadge::AiCost),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Teal),
                format!(
                    " 🤖 {}: ${:.2} ({}t) ",
                    metrics.agent,
                    metrics.cost,
                    metrics.tokens.input + metrics.tokens.output
                ),
            )],
        ));
    }
    if let Some(ref a) = model.agent_activity {
        use superzej_core::theme::Hue;
        // The chip is the only native signal, so it must show failure states too.
        let (hue, label) = match a.conn {
            AgentConn::Error => (Hue::Red, " ⚠ agent error ".to_string()),
            AgentConn::Exited => (Hue::Orange, " ⚠ agent offline ".to_string()),
            AgentConn::Connecting => (Hue::Blue, " 🤖 agent connecting… ".to_string()),
            AgentConn::Online => {
                let tool = match (&a.last_tool, a.running) {
                    (Some(t), true) => format!("🛠 {t}…"),
                    (Some(t), false) => format!("🛠 {t}"),
                    (None, _) => "🤖 agent".to_string(),
                };
                // Append context-window usage as a percentage when reported.
                let label = if a.context_size > 0 {
                    let pct = (a.context_used * 100 / a.context_size).clamp(0, 100);
                    format!(" {tool} · {pct}% ctx ")
                } else {
                    format!(" {tool} ")
                };
                let hue = if a.running { Hue::Amber } else { Hue::Teal };
                (hue, label)
            }
        };
        items.push((
            BarItemId::Badge(BarBadge::Agent),
            vec![Seg::chip(Tok::Hue(hue), label)],
        ));
    }
    if model.zoomed {
        items.push((
            BarItemId::Badge(BarBadge::Zoom),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Purple),
                " \u{26f6} ZOOM ",
            )],
        ));
    }
    if model.key_locked {
        items.push((
            BarItemId::Badge(BarBadge::Lock),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Amber),
                " \u{2301} LOCKED ",
            )],
        ));
    }
    if model.sync_panes {
        items.push((
            BarItemId::Badge(BarBadge::Sync),
            vec![Seg::chip(
                Tok::Hue(superzej_core::theme::Hue::Red),
                " \u{29c9} SYNC ",
            )],
        ));
    }
    items
}

/// Recolor an item's segments as the focused-selection block: bright focus
/// foreground over a focus-tinted pill, matching the masthead app-chips' active
/// look ("reads like a selected tab"). Width is unchanged, so spans are stable.
fn highlight_segs(
    segs: &[crate::seg::Seg],
    pill: ColorAttribute,
    fg: ColorAttribute,
) -> Vec<crate::seg::Seg> {
    use crate::seg::Tok;
    segs.iter()
        .map(|s| {
            let mut h = s.clone();
            h.fg = Tok::Attr(fg);
            h.bg = Some(Tok::Attr(pill));
            h.bold = true;
            h
        })
        .collect()
}

/// Lay out the statusbar right cluster from [`statusbar_items`]: join items with
/// separators (` │ ` between adjacent config widgets, a single space before each
/// badge, trailing space) and, when `sel` is `Some`, highlight that item.
/// Returns the seg run for `draw_line` plus each item's `(id, x_offset, width)`
/// WITHIN the cluster (offset 0 = the cluster's left cell). Separators add width
/// but are not items, so offsets account for them.
fn statusbar_right_layout(
    items: &[(BarItemId, Vec<crate::seg::Seg>)],
    sel: Option<(usize, ColorAttribute, ColorAttribute)>,
) -> (Vec<crate::seg::Seg>, Vec<(BarItemId, usize, usize)>) {
    use crate::seg::{Seg, Tok, seg, seg_width};
    let mut r: Vec<Seg> = Vec::new();
    let mut spans: Vec<(BarItemId, usize, usize)> = Vec::new();
    let mut off = 0usize;
    let mut prev_widget = false;
    for (idx, (id, segs)) in items.iter().enumerate() {
        let is_widget = matches!(id, BarItemId::Widget(_));
        // Separator: ` │ ` between two adjacent config widgets, a single space
        // before any badge (reproduces the historical leading-space per badge).
        if is_widget {
            if prev_widget {
                let s = seg(Tok::Slot(S::Ghost3), " \u{2502} ");
                off += seg_width(std::slice::from_ref(&s));
                r.push(s);
            }
        } else {
            r.push(seg(Tok::Slot(S::Text), " "));
            off += 1;
        }
        let drawn = match sel {
            Some((s, pill, fg)) if s == idx => highlight_segs(segs, pill, fg),
            _ => segs.clone(),
        };
        let w = seg_width(&drawn);
        spans.push((id.clone(), off, w));
        off += w;
        r.extend(drawn);
        prev_widget = is_widget;
    }
    r.push(seg(Tok::Slot(S::Text), " "));
    (r, spans)
}

/// The statusbar right-cluster items' absolute `(id, Rect)` spans for the given
/// statusbar rect — mouse hit-testing and detail-popup anchoring. Mirrors
/// [`draw_statusbar`]'s right-alignment (the cluster hugs the right edge), so a
/// hit/anchor lands exactly where the item is painted.
pub fn statusbar_item_spans(model: &FrameModel, rect: Rect) -> Vec<(BarItemId, Rect)> {
    use crate::seg::seg_width;
    if rect.rows == 0 || rect.cols == 0 {
        return Vec::new();
    }
    let items = statusbar_items(model);
    let (r, spans) = statusbar_right_layout(&items, None);
    let rl = seg_width(&r);
    // `Line::Split` right-aligns the right cluster: it begins at `cols - rl`.
    let base = (rect.x + rect.cols).saturating_sub(rl);
    spans
        .into_iter()
        .map(|(id, off, w)| {
            (
                id,
                Rect {
                    x: base + off,
                    y: rect.y,
                    cols: w,
                    rows: 1,
                },
            )
        })
        .collect()
}

/// The bottom widget bar: mode chip + `[bars] bottom_left` (context keybind
/// hints as key chips + dim labels) left-aligned, `bottom_right` (PR / LOC /
/// transient status) right-aligned with `│` rules, and the zoom/lock badges
/// as inverse chips always outermost-right.
pub fn draw_statusbar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    use crate::seg::{Line, Seg, Tok, draw_line, seg, seg_width};
    if rect.rows == 0 {
        return;
    }

    // Right side: per-widget colors with `│` rules then the badges, built from
    // the shared enumerator so navigation/highlight/hit-test stay in lock-step.
    // The right cluster wins space — the left fits in what's left.
    let items = statusbar_items(model);
    let sel = if model.statusbar_focused {
        let pill = theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.28));
        Some((
            model.statusbar_sel.min(items.len().saturating_sub(1)),
            pill,
            col(S::Focus),
        ))
    } else {
        None
    };
    let (r, _spans) = statusbar_right_layout(&items, sel);

    // Cells the left cluster gets once the right wins its space — mirrors
    // `Line::split`'s math so keyhints can be trimmed at whole-binding
    // boundaries here instead of being cut mid-chord by the generic ellipsis.
    let rl = seg_width(&r);
    let left_budget = rect.cols.saturating_sub(rl + usize::from(rl > 0));

    let mut l: Vec<Seg> = vec![seg(Tok::Slot(S::Text), " ")];
    if !model.mode_chip.is_empty() {
        l.push(Seg::chip(
            Tok::Slot(S::Accent),
            format!(" {} ", model.mode_chip),
        ));
        l.push(seg(Tok::Slot(S::Text), "  "));
    }
    let mut first = true;
    for id in &model.bars.bottom_left {
        if id == "keyhints" {
            for (chord, label) in &model.keyhints {
                // Stage each binding as a unit; only commit it if the whole
                // thing still fits. Once one overflows, stop — never paint a
                // half-cut keybind.
                let mut hint: Vec<Seg> = Vec::new();
                if !first {
                    hint.push(seg(Tok::Slot(S::Text), "   "));
                }
                hint.push(seg(Tok::Slot(S::Faint), chord.clone()));
                hint.push(seg(Tok::Slot(S::Ghost), format!(" {label}")));
                if seg_width(&l) + seg_width(&hint) > left_budget {
                    break;
                }
                l.extend(hint);
                first = false;
            }
            continue;
        }
        let Some(wd) = bottombar_widget(id, model) else {
            continue;
        };
        if !first {
            l.push(seg(Tok::Slot(S::Ghost3), " \u{00b7} "));
        }
        first = false;
        l.push(seg(Tok::Attr(wd.fg), wd.text));
    }

    draw_line(
        surface,
        rect.x,
        rect.y,
        rect.cols,
        &Line::split(l, r),
        Tok::Slot(if model.statusbar_focused {
            S::Raise
        } else {
            S::Panel
        }),
    );
}

pub fn draw_sidebar(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    fill(surface, rect, col(S::Panel));
    if rect.cols == 0 || rect.rows == 0 {
        return;
    }
    // The slim rail is its own compact language (activity dot + initial); the
    // full panel renders the header, the laid-out rows, metrics, and menu.
    if model.sidebar_rail {
        draw_sidebar_rail(surface, rect, model);
        return;
    }
    let accent = theme_color(model.accent_or_default());

    // Header: the live filter input, or the "WORKSPACES" section title — in
    // the accent so the column titles pop against the tinted zone.
    if model.sidebar_filtering || !model.sidebar_filter.is_empty() {
        let header = format!(" /{}", model.sidebar_filter);
        draw_text(
            surface,
            rect.x,
            rect.y,
            &header,
            accent,
            col(S::Panel),
            rect.cols,
        );
    } else {
        draw_text_bold(
            surface,
            rect.x,
            rect.y,
            " WORKSPACES",
            col(S::Text),
            col(S::Panel),
            rect.cols,
        );
    }

    // One layout pass: the renderer and the click hit-test (`sidebar_hits`)
    // both derive geometry from `build_sidebar`, so paint and clicks can never
    // drift apart (the same contract the panel uses via `build_panel`).
    let frame = build_sidebar(model, rect, model.sidebar_scroll);
    for p in &frame.rows {
        // `draw_lines` fills the placement's full width in `bg`; every composed
        // line begins with a 1-col gutter so the cursor bar can overpaint col 0
        // without clobbering content.
        crate::seg::draw_lines(
            surface,
            Rect {
                x: rect.x,
                y: p.y,
                cols: rect.cols,
                rows: p.height,
            },
            &p.lines,
            p.bg,
        );
        // Left-edge accent bar marks the cursor row and spans its full height
        // (including the expanded detail line). It persists when the sidebar
        // loses focus but dims — bright focus color while focused, a muted
        // focus-over-panel tint otherwise — so a resting selection is still
        // visible without being mistaken for focus.
        if p.cursor_bar {
            let bar_fg = if model.sidebar_focused {
                col(S::Focus)
            } else {
                theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.5))
            };
            for dy in 0..p.height {
                draw_text(
                    surface,
                    rect.x,
                    p.y + dy,
                    "\u{2590}",
                    bar_fg,
                    tok_col(p.bg),
                    1,
                );
            }
        }
    }

    // Row context menu overlay (item 27).
    if let Some(menu) = &model.sidebar_menu {
        draw_row_menu(surface, rect, menu, accent);
    }

    if let Some(mrect) = frame.metrics {
        draw_metrics_section(surface, mrect, model);
    }
}

/// The slim collapsed rail: one row per visible worktree, an activity dot in
/// its state color plus the first letter of the label. `model.sidebar_scroll`
/// keeps the cursor in view.
fn draw_sidebar_rail(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    let frame = build_sidebar(model, rect, model.sidebar_scroll);
    for p in &frame.rows {
        crate::seg::draw_lines(
            surface,
            Rect {
                x: rect.x,
                y: p.y,
                cols: rect.cols,
                rows: p.height,
            },
            &p.lines,
            p.bg,
        );
    }
}

/// A laid-out sidebar row: which visible-row it is, where it sits, how tall it
/// is, and the composed line(s) + background to paint. The cursor row may be
/// two lines tall (the expanded detail tier); a section heading carries a
/// leading blank gap.
pub(crate) struct SidebarPlacement {
    pub visible_index: usize,
    pub y: usize,
    pub height: usize,
    pub lines: Vec<crate::seg::Line>,
    pub bg: crate::seg::Tok,
    pub cursor_bar: bool,
}

/// The result of one sidebar layout pass: the on-screen row placements, the
/// (clamped) scroll offset actually used, and the metrics section rect (full
/// mode only). Pure — the renderer paints it and the mouse path hit-tests it.
pub(crate) struct SidebarFrame {
    pub rows: Vec<SidebarPlacement>,
    pub scroll: usize,
    pub metrics: Option<Rect>,
}

/// Lay out the sidebar rows for `rect`, starting from `desired_scroll` (clamped
/// so the cursor row stays fully visible). Variable row heights (the cursor's
/// two-tier expansion, the section-heading gap) are resolved here so render and
/// click share one source.
pub(crate) fn build_sidebar(model: &FrameModel, rect: Rect, desired_scroll: usize) -> SidebarFrame {
    use crate::sidebar::RowKind;
    let rail = model.sidebar_rail;
    let visible: Vec<&crate::sidebar::SidebarRow> =
        model.sidebar_rows.iter().filter(|r| r.visible).collect();

    // Quick-jump digits are revealed only while the sidebar is focused — they
    // declutter the resting view but let you see the Ctrl+N (workspace) and
    // Alt+N (worktree) targets when you're navigating it. Each axis counts
    // independently in visible order, slots 1..=9, matching the dispatch:
    // workspaces follow `sidebar_workspace_order` (switchable = has a
    // `worktree_path`); worktrees follow `sidebar_worktree_order` (Tab targets).
    let slots: Vec<Option<u8>> = if model.sidebar_focused {
        let (mut ws, mut wt): (u8, u8) = (1, 1);
        visible
            .iter()
            .map(|r| match r.kind {
                RowKind::Workspace if r.worktree_path.is_some() => {
                    let s = (ws <= 9).then_some(ws);
                    ws += 1;
                    s
                }
                RowKind::Worktree
                    if matches!(r.tab_target, Some(crate::sidebar::RowTarget::Tab(..))) =>
                {
                    let s = (wt <= 9).then_some(wt);
                    wt += 1;
                    s
                }
                _ => None,
            })
            .collect()
    } else {
        vec![None; visible.len()]
    };

    // The full panel reserves a header + blank row at the top and a metrics
    // section at the bottom; the rail uses the whole column.
    let (head_rows, metrics_rows) = if rail {
        (0, 0)
    } else {
        let m = if rect.rows > 10 && !model.metrics.targets.is_empty() {
            6.min(rect.rows.saturating_sub(4))
        } else {
            0
        };
        (2, m)
    };
    let metrics = (metrics_rows > 0).then_some(Rect {
        x: rect.x,
        y: rect.y + rect.rows - metrics_rows,
        cols: rect.cols,
        rows: metrics_rows,
    });
    let list_y = rect.y + head_rows;
    let list_rows = rect.rows.saturating_sub(head_rows + metrics_rows);
    let cursor = if visible.is_empty() {
        0
    } else {
        model.sidebar_selected.min(visible.len() - 1)
    };

    // Compose every visible row's line(s) + background once; the cursor row
    // expands to a detail line when it has secondary metadata.
    let mut composed: Vec<(Vec<crate::seg::Line>, crate::seg::Tok, bool)> =
        Vec::with_capacity(visible.len());
    // The warm-pool chip rides the ACTIVE workspace's row — the workspace_slug of
    // the active worktree row. (Workspace rows themselves carry `active = false`.)
    let active_ws_slug: Option<String> = visible
        .iter()
        .find(|r| r.active && r.kind == RowKind::Worktree)
        .map(|r| r.workspace_slug.clone());
    for (i, row) in visible.iter().enumerate() {
        let is_cursor = i == cursor;
        // A row is the last child at its depth when the next visible row steps
        // back up the tree (or there is none) — drives the └ vs ├ connector.
        let is_last = visible.get(i + 1).is_none_or(|n| n.depth < row.depth);
        let mut lines = if rail {
            vec![compose_rail_line(row)]
        } else {
            let wt = row
                .worktree_path
                .as_deref()
                .and_then(|p| model.sidebar_window_titles.get(p))
                .map(String::as_str);
            let pool = if row.kind == RowKind::Workspace
                && active_ws_slug.as_deref() == Some(row.workspace_slug.as_str())
            {
                model.pool
            } else {
                None
            };
            compose_row_lines(row, wt, is_cursor, is_last, slots[i], pool)
        };
        // A section banner gets a breathing gap above it (except at the top).
        if !rail && row.kind == RowKind::SectionHeading && i > 0 {
            lines.insert(0, crate::seg::Line::Blank);
        }
        let bg = row_bg(row, i, cursor, model);
        // The cursor row always carries the left-edge bar; focus only tints it.
        let cursor_bar = !rail && is_cursor && !matches!(row.kind, RowKind::SectionHeading);
        composed.push((lines, bg, cursor_bar));
    }
    let heights: Vec<usize> = composed.iter().map(|(l, _, _)| l.len().max(1)).collect();
    let scroll = clamp_sidebar_scroll(&heights, cursor, list_rows, desired_scroll);

    let mut rows = Vec::new();
    let mut y = list_y;
    let bottom = list_y + list_rows;
    for (i, (lines, bg, cursor_bar)) in composed.into_iter().enumerate().skip(scroll) {
        if y >= bottom {
            break;
        }
        let height = heights[i].min(bottom - y); // clip a partly-fitting tail row
        rows.push(SidebarPlacement {
            visible_index: i,
            y,
            height,
            lines,
            bg,
            cursor_bar,
        });
        y += heights[i];
    }
    SidebarFrame {
        rows,
        scroll,
        metrics,
    }
}

/// Pick `scroll` (top visible-row index) so the cursor row fits fully within
/// `list_rows`, honoring `desired` where possible. Heights are per-row (the
/// cursor row may be 2). O(n·window) but `n` is the worktree count — tiny.
fn clamp_sidebar_scroll(
    heights: &[usize],
    cursor: usize,
    list_rows: usize,
    desired: usize,
) -> usize {
    let n = heights.len();
    if n == 0 || list_rows == 0 {
        return 0;
    }
    let cursor = cursor.min(n - 1);
    // Never scroll past the cursor (it must be at least the top row).
    let mut scroll = desired.min(cursor);
    loop {
        // Walk from `scroll`; does the cursor row's last line fit in the window?
        let mut used = 0usize;
        let mut fits = false;
        for (i, h) in heights.iter().enumerate().skip(scroll) {
            if i == cursor {
                fits = used + h <= list_rows;
                break;
            }
            used += h;
            if used >= list_rows {
                break;
            }
        }
        if fits || scroll >= cursor {
            break;
        }
        scroll += 1;
    }
    scroll
}

/// Background token for a row: cursor selection > active worktree > multi-select
/// mark > a recessed band for header rows (workspace/host/folder) > the plain
/// panel tint. Section banners never highlight — they read as titles.
fn row_bg(
    row: &crate::sidebar::SidebarRow,
    i: usize,
    cursor: usize,
    model: &FrameModel,
) -> crate::seg::Tok {
    use crate::seg::Tok;
    use crate::sidebar::RowKind;
    if row.kind == RowKind::SectionHeading {
        return Tok::Slot(S::Panel);
    }
    let header = matches!(
        row.kind,
        RowKind::Workspace | RowKind::TerminalHost | RowKind::Folder
    );
    if i == cursor {
        Tok::Slot(S::Panel2)
    } else if row.active {
        Tok::SelAccent
    } else if model.sidebar_marked.contains(&i) {
        Tok::Slot(S::Raise)
    } else if header {
        Tok::Slot(S::Bg0)
    } else {
        Tok::Slot(S::Panel)
    }
}

/// Resolve a seg color token to a concrete color (for the focus bar's bg).
fn tok_col(t: crate::seg::Tok) -> ColorAttribute {
    with_palette(|p| t.resolve(p))
}

/// The `(visible_index, y, height)` of each rendered sidebar row — the same
/// `build_sidebar` pass the renderer painted, so a click resolves against
/// what is on screen. Pure; the mouse path calls it on demand.
pub(crate) fn sidebar_hits(model: &FrameModel, rect: Rect) -> Vec<(usize, usize, usize)> {
    build_sidebar(model, rect, model.sidebar_scroll)
        .rows
        .iter()
        .map(|p| (p.visible_index, p.y, p.height))
        .collect()
}

fn draw_metrics_section(surface: &mut Surface, rect: Rect, model: &FrameModel) {
    if rect.rows < 2 || rect.cols == 0 {
        return;
    }

    let line = "\u{2500}".repeat(rect.cols);
    draw_text(
        surface,
        rect.x,
        rect.y,
        &line,
        col(S::Border),
        col(S::Panel),
        rect.cols,
    );
    draw_text_bold(
        surface,
        rect.x + 1,
        rect.y,
        " METRICS ",
        col(S::Text),
        col(S::Panel),
        rect.cols.saturating_sub(1),
    );

    let mut y = rect.y + 1;
    let max_y = rect.y + rect.rows;
    for target in &model.metrics.targets {
        if y >= max_y {
            break;
        }
        let gl = crate::caps::active_glyphs();
        let (dot, dot_fg, health) = match target.health {
            crate::metrics::MetricHealth::Up => (gl.dot_filled, theme_color(theme::GREEN), "up"),
            crate::metrics::MetricHealth::Stale => (gl.dot_hollow, col(S::Dim), "stale"),
            crate::metrics::MetricHealth::Error => (gl.dot_hollow, theme_color(theme::RED), "err"),
        };
        draw_text(surface, rect.x + 1, y, dot, dot_fg, col(S::Panel), 1);
        let label = format!("{} {}", target.name, health);
        draw_text(
            surface,
            rect.x + 3,
            y,
            &label,
            col(S::Text),
            col(S::Panel),
            rect.cols.saturating_sub(3),
        );
        y += 1;

        match target.health {
            crate::metrics::MetricHealth::Up => {
                for sample in target.samples.iter().take(3) {
                    if y >= max_y {
                        break;
                    }
                    let value = crate::metrics::format_sample_value(sample.value);
                    let line = format!("  {} {}", sample.name, value);
                    draw_text(
                        surface,
                        rect.x + 1,
                        y,
                        &line,
                        col(S::Dim),
                        col(S::Panel),
                        rect.cols.saturating_sub(1),
                    );
                    y += 1;
                }
            }
            crate::metrics::MetricHealth::Stale | crate::metrics::MetricHealth::Error => {
                if y < max_y {
                    let err = target.error.as_deref().unwrap_or("scrape failed");
                    let line = format!("  err: {err}");
                    draw_text(
                        surface,
                        rect.x + 1,
                        y,
                        &line,
                        col(S::Faint),
                        col(S::Panel),
                        rect.cols.saturating_sub(1),
                    );
                    y += 1;
                }
            }
        }
    }
}

/// The indent + connector segments for a tree row at `depth` (worktree = 1,
/// folder child = 2). Two cells of indent per ancestor level, then a `└`/`├`
/// connector in a ghost tone so the nesting is visible at a glance.
fn tree_lead(depth: u8, is_last: bool) -> Vec<crate::seg::Seg> {
    use crate::seg::{Tok, seg, sp};
    let indent = (depth.saturating_sub(1)) as usize * 2;
    let conn = if is_last { "\u{2514} " } else { "\u{251c} " }; // └ / ├
    vec![sp(indent), seg(Tok::Slot(S::Ghost2), conn)]
}

/// The activity dot glyph (item 20): filled while active/waiting, hollow once
/// read-but-stuck, `↻` while building; `None` = nothing. ASCII-safe glyph set.
fn activity_dot_glyph(state: crate::sidebar::ActivityState) -> &'static str {
    use crate::sidebar::ActivityState::*;
    let g = crate::caps::active_glyphs();
    match state {
        Active | Waiting => g.dot_filled, // ● / *
        Read => g.dot_hollow,             // ○ / o
        Loading => g.refresh,             // ↻ / @ — worktree building
        None => "",
    }
}

/// The activity dot color token per state (`activity_active`/`activity_waiting`;
/// loading = accent). Both red states share the waiting slot (glyph-only diff).
fn activity_dot_tok(state: crate::sidebar::ActivityState) -> crate::seg::Tok {
    use crate::sidebar::ActivityState::*;
    crate::seg::Tok::Slot(match state {
        Active => S::ActivityActive,
        Waiting | Read => S::ActivityWaiting,
        Loading => S::Accent,
        None => S::Dim,
    })
}

/// Compose the on-screen line(s) for one visible row. Headers (workspace / host
/// / folder) are a single bold styled line; section banners render like the
/// "WORKSPACES" title; worktrees are a name/status split, and the cursor row
/// (`expanded`) grows a second detail line carrying the secondary metadata
/// (env / backend / PR / unread / disk). `slot` is the Ctrl+1..9 quick-jump
/// digit for switchable workspace rows. Every line starts with a 1-col gutter
/// so the focus bar can overpaint col 0.
fn compose_row_lines(
    row: &crate::sidebar::SidebarRow,
    window_title: Option<&str>,
    expanded: bool,
    is_last: bool,
    slot: Option<u8>,
    // Warm-spare-pool `(ready, target)` for THIS row — `Some` only on the active
    // workspace's row (pool is per-workspace); `None` hides the chip.
    pool: Option<(usize, usize)>,
) -> Vec<crate::seg::Line> {
    use crate::seg::{Line, Seg, Tok, seg, sp};
    use crate::sidebar::{ActivityState, RowKind};
    let gl = crate::caps::active_glyphs();
    let caret = |collapsed: bool| {
        if collapsed {
            "\u{25b8}" // ▸
        } else {
            "\u{25be}" // ▾
        }
    };

    match row.kind {
        RowKind::Workspace | RowKind::TerminalHost => {
            let mut l = vec![sp(1)];
            // Quick-jump digit on a switchable workspace row (Ctrl+1..9).
            if row.kind == RowKind::Workspace
                && let Some(n) = slot
            {
                // Leading space keeps the digit off the cursor bar (col 0).
                l.push(seg(Tok::Slot(S::Faint), format!(" {n} ")));
            }
            l.push(seg(Tok::Slot(S::Faint), caret(row.collapsed)));
            l.push(sp(1));
            if row.kind == RowKind::TerminalHost {
                // Host group glyph: local vs remote (from the rep connection).
                let local = row
                    .terminal_connection
                    .as_deref()
                    .map(str::is_empty)
                    .unwrap_or(true);
                l.push(seg(
                    Tok::Slot(S::Dim),
                    if local { "\u{1f4bb} " } else { "\u{1f310} " },
                ));
            } else if row.dir {
                // A non-git "dir" workspace gets a folder glyph to read apart.
                l.push(seg(Tok::Slot(S::Text), "\u{1f4c1} "));
            }
            l.push(seg(Tok::Slot(S::Text), row.label.clone()).bold());
            // Warm-spare-pool chip, right-aligned on the active title (accent
            // when full, dim while provisioning).
            match pool.filter(|(_, t)| *t > 0) {
                Some((ready, target)) => {
                    let tok = if ready >= target {
                        Tok::Slot(S::Accent)
                    } else {
                        Tok::Slot(S::Dim)
                    };
                    vec![Line::Split {
                        l,
                        r: vec![seg(tok, format!("warm {ready}/{target} "))],
                    }]
                }
                None => vec![Line::Segs(l)],
            }
        }
        RowKind::SectionHeading => vec![Line::Segs(vec![
            sp(1),
            seg(Tok::Slot(S::Text), row.label.clone()).bold(),
        ])],
        RowKind::EmptyHint => vec![Line::Segs(vec![
            sp(3),
            seg(Tok::Slot(S::Faint), row.label.clone()),
        ])],
        RowKind::Folder => vec![Line::Segs(vec![
            sp(1),
            sp(2),
            seg(Tok::Slot(S::Faint), caret(row.collapsed)),
            sp(1),
            seg(Tok::Slot(S::Dim), "\u{1f4c2} "), // 📂
            seg(Tok::Slot(S::Text), row.label.clone()).bold(),
        ])],
        RowKind::Terminal => {
            let glyph = match row.terminal_connection.as_deref() {
                Some(c) if c.starts_with("ssh") => "\u{1f310} ", // 🌐
                Some(c) if c.starts_with("mosh") => "\u{1f680} ", // 🚀
                _ => "\u{1f4bb} ",                               // 💻
            };
            let mut l = vec![sp(1)];
            l.extend(tree_lead(row.depth, is_last));
            l.push(seg(Tok::Slot(S::Dim), glyph));
            l.push(seg(Tok::Slot(S::Dim), row.label.clone()));
            vec![Line::Segs(l)]
        }
        RowKind::Worktree => {
            // Left cluster: gutter, Alt+1..9 jump digit, tree connector,
            // activity dot, the dynamic name, then the agent glyph.
            let mut left = vec![sp(1)];
            if let Some(n) = slot {
                // Leading space keeps the digit off the cursor bar (col 0).
                left.push(seg(Tok::Slot(S::Faint), format!(" {n} ")));
            }
            left.extend(tree_lead(row.depth, is_last));
            if matches!(row.activity, ActivityState::None) {
                left.push(sp(2)); // keep names aligned with dotted rows
            } else {
                left.push(seg(
                    activity_dot_tok(row.activity),
                    activity_dot_glyph(row.activity),
                ));
                left.push(sp(1));
            }
            let name_fg = if row.active {
                Tok::Slot(S::Focus)
            } else if expanded {
                Tok::Slot(S::Text)
            } else {
                Tok::Slot(S::Dim)
            };
            let label = crate::sidebar::compose_row_label(row.pr_number, window_title, &row.label);
            left.push(seg(name_fg, label));

            // Right cluster (always-on): git status + alert badge (PR/unread/disk move to the detail line).
            let mut right: Vec<Seg> = Vec::new();
            let push_sp = |v: &mut Vec<Seg>| {
                if !v.is_empty() {
                    v.push(sp(1));
                }
            };
            if let Some(g) = row.git {
                if g.dirty {
                    right.push(seg(Tok::Hue(theme::Hue::Amber), gl.dot_filled)); // ●
                }
                if g.ahead > 0 {
                    push_sp(&mut right);
                    right.push(seg(
                        Tok::Slot(S::Dim),
                        format!("{}{}", gl.arrow_up, g.ahead),
                    )); // ↑N
                }
                if g.behind > 0 {
                    push_sp(&mut right);
                    right.push(seg(
                        Tok::Slot(S::Dim),
                        format!("{}{}", gl.arrow_down, g.behind),
                    ));
                    // ↓N
                }
            }
            if row.alert_count > 0 {
                push_sp(&mut right);
                right.push(seg(
                    Tok::Hue(theme::Hue::Red),
                    format!("{} {}", gl.warn, row.alert_count),
                ));
                // ⚠N (caps-routed → `!N` in ASCII)
            }
            if row
                .worktree_path
                .as_deref()
                .is_some_and(crate::hibernator::is_hibernated)
            {
                push_sp(&mut right);
                right.push(seg(Tok::Slot(S::Dim), gl.moon.to_string())); // ⏾ hibernated
            }

            let mut lines = vec![if right.is_empty() {
                Line::Segs(left)
            } else {
                Line::Split { l: left, r: right }
            }];
            if expanded && let Some(detail) = crate::sidebar::compose_detail_line(row) {
                lines.push(detail);
            }
            lines
        }
    }
}

/// The slim-rail line for one row: a colored activity dot (or a faint marker
/// for headers) plus the label's first letter, fitted to the rail's ~4 cols.
fn compose_rail_line(row: &crate::sidebar::SidebarRow) -> crate::seg::Line {
    use crate::seg::{Line, Tok, seg, sp};
    use crate::sidebar::{ActivityState, RowKind};
    let gl = crate::caps::active_glyphs();
    match row.kind {
        RowKind::Worktree => {
            let dot = if matches!(row.activity, ActivityState::None) {
                seg(Tok::Slot(S::Ghost2), gl.middot) // · placeholder keeps the column
            } else {
                seg(
                    activity_dot_tok(row.activity),
                    activity_dot_glyph(row.activity),
                )
            };
            let initial: String = row
                .label
                .chars()
                .next()
                .map(|c| c.to_string())
                .unwrap_or_default();
            let fg = if row.active {
                Tok::Slot(S::Focus)
            } else {
                Tok::Slot(S::Dim)
            };
            Line::Segs(vec![sp(1), dot, sp(1), seg(fg, initial)])
        }
        _ => Line::Segs(vec![sp(1), seg(Tok::Slot(S::Faint), gl.box_h)]), // header marker
    }
}

fn draw_row_menu(surface: &mut Surface, rect: Rect, menu: &RowMenu, accent: ColorAttribute) {
    let width = rect.cols;
    let top = (rect.y + 1 + menu.anchor + 1).min(rect.y + rect.rows.saturating_sub(1));
    for (i, entry) in menu.entries.iter().enumerate() {
        let y = top + i;
        if y >= rect.y + rect.rows {
            break;
        }
        let sel = i == menu.cursor;
        // Panel2/Raise so the menu reads as raised above the Panel-tinted zone.
        let bg = if sel { col(S::Raise) } else { col(S::Panel2) };
        fill(
            surface,
            Rect {
                x: rect.x,
                y,
                cols: width,
                rows: 1,
            },
            bg,
        );
        let fg = if sel { accent } else { col(S::Text) };
        draw_text(
            surface,
            rect.x + 1,
            y,
            &format!("\u{203a} {}", entry.label),
            fg,
            bg,
            width.saturating_sub(1),
        );
    }
}

/// Draw the right panel: the accordion frame (branch header zone, the
/// numbered section rows with the open section's content), rendered
/// row-by-row through the seg layer. `build_panel` is the single source of
/// truth for placement; mouse hit-testing reuses the same pass via
/// [`panel_hits`], so paint and clicks can never drift apart.
pub fn draw_panel(
    surface: &mut Surface,
    rect: Rect,
    model: &FrameModel,
    ui: &crate::panel::PanelUi,
) {
    fill(surface, rect, col(S::Panel));
    if rect.rows == 0 || rect.cols == 0 {
        return;
    }
    if model.panel_pending {
        // Cold-switch skeleton: dim placeholder bars while hydration is in
        // flight (static — no animation, so no timer wakes).
        crate::panel::skeleton::draw(surface, rect);
        return;
    }
    let frame =
        crate::panel::frame::build_panel(model, ui, rect.cols, rect.rows, model.panel_focused);
    for (i, row) in frame.rows.iter().enumerate() {
        crate::seg::draw_line(
            surface,
            rect.x,
            rect.y + i,
            rect.cols,
            &row.line,
            row.bg.unwrap_or(crate::seg::Tok::Slot(S::Panel)),
        );
    }
}

/// The `(absolute row y, hit)` targets of the rendered panel — the exact
/// `build_panel` pass the renderer painted, so a click resolves against what
/// is actually on screen. Pure; the mouse path calls it on demand.
pub fn panel_hits(
    model: &FrameModel,
    ui: &crate::panel::PanelUi,
    rect: Rect,
) -> Vec<(usize, crate::panel::PanelHit)> {
    crate::panel::frame::build_panel(model, ui, rect.cols, rect.rows, model.panel_focused)
        .rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.hit.map(|h| (rect.y + i, h)))
        .collect()
}

/// Resolve a click against the Full view's slim rail (`1 changes · 2 git · …`).
/// `panel_hits` is row-granular and the rail packs several sections onto one
/// row, so the Full view needs this x+y test. Returns the section whose
/// `N label` span the click landed in, if any.
pub fn panel_rail_hit(
    model: &FrameModel,
    ui: &crate::panel::PanelUi,
    rect: Rect,
    x: usize,
    y: usize,
) -> Option<crate::panel::Section> {
    let frame =
        crate::panel::frame::build_panel(model, ui, rect.cols, rect.rows, model.panel_focused);
    let rel_x = x.checked_sub(rect.x)?;
    frame
        .rail
        .iter()
        .find(|s| rect.y + s.row == y && s.cols.contains(&rel_x))
        .map(|s| s.section)
}

/// Resolve a click on the tab bar row (row 0 of the panel in Normal/Half/Full
/// modes). The tab bar row fits on a single terminal line but carries three
/// labeled pills; the x position determines which tab was hit.
///
/// Returns `None` when the click is not on row 0, when the frame has no tab
/// bar (git-family full view), or when the x position falls between pills.
pub fn panel_tab_hit(
    model: &FrameModel,
    ui: &crate::panel::PanelUi,
    rect: Rect,
    x: usize,
    y: usize,
) -> Option<crate::panel::PanelTab> {
    // Tab bar is always row 0 of the panel frame.
    if y != rect.y {
        return None;
    }
    let frame =
        crate::panel::frame::build_panel(model, ui, rect.cols, rect.rows, model.panel_focused);
    let rel_x = x.checked_sub(rect.x)?;
    frame
        .tab_spans
        .iter()
        .find(|(cols, _)| cols.contains(&rel_x))
        .map(|(_, tab)| *tab)
}

/// The context-sensitive help-bar hints for the accordion's current state, as
/// (chord, label) pairs for the statusbar's chip renderer: section-walking
/// keys while the cursor is on the section list, the open section's row
/// actions once Enter drops into its rows.
pub(crate) fn panel_help_pairs(ui: &crate::panel::PanelUi) -> Vec<(String, String)> {
    use crate::panel::Section;
    if !ui.row_mode {
        let jumps = format!("1-{}", ui.visible_section_count());
        return [
            ("j/k", "section"),
            (jumps.as_str(), "jump"),
            ("↵", "open"),
            ("⇥", "tabs"),
            ("e", "expand"),
        ]
        .iter()
        .map(|(c, l)| (c.to_string(), l.to_string()))
        .collect();
    }
    // The git-family lists draw their hints from the focused context's key
    // table (the same data that drives dispatch and the `?` cheatsheet, so
    // the help bar can never drift). The Pr section keeps its PR actions.
    if ui.open.is_git_family() && ui.open != Section::Pr {
        let ctx_keys = crate::panel::gitui::context_keys(ui.git.focus);
        let mut pairs: Vec<(String, String)> = Vec::new();
        // Sequencer flow hint leads: it replaces the generic "m flow menu" in
        // the table so the label reflects what `m` will actually do right now.
        if let Some((chord, label)) = crate::panel::gitui::flow_hint(&ui.git.flow) {
            pairs.push((chord.to_string(), label.to_string()));
            pairs.extend(
                ctx_keys
                    .iter()
                    .filter(|ck| ck.chord != chord)
                    .take(6usize.saturating_sub(1))
                    .map(|ck| (ck.chord.to_string(), ck.label.to_string())),
            );
        } else {
            pairs.extend(
                ctx_keys
                    .iter()
                    .take(6)
                    .map(|ck| (ck.chord.to_string(), ck.label.to_string())),
            );
        }
        return pairs;
    }
    let pairs: &[(&str, &str)] = match ui.open {
        Section::Changes | Section::Commits | Section::Branches | Section::Stash => {
            unreachable!("git-family sections returned above")
        }
        Section::Mine => &[
            ("j/k", "row"),
            ("↵", "open"),
            ("b", "branch"),
            ("o", "browser"),
            ("R", "refresh"),
        ],
        Section::Pr => &[
            ("j/k", "row"),
            ("M", "merge"),
            ("A", "approve"),
            ("r", "rerun"),
            ("o", "browser"),
        ],
        Section::Tests => &[("r", "run"), ("R", "all"), ("f", "failed"), ("↵", "open")],
        Section::Ci => &[
            ("j/k", "row"),
            ("↵", "view"),
            ("r", "rerun"),
            ("o", "browser"),
        ],
        Section::MergeQueue => &[("a/A", "add"), ("D", "drain"), ("l/r/x", "act")],
        Section::Files => &[("↵", "open"), ("y", "yazi")],
        Section::Issues => &[
            ("j/k", "row"),
            ("↵", "link"),
            ("o", "open"),
            ("n", "new"),
            ("e", "edit"),
        ],
        Section::Notifications => &[
            ("j/k", "row"),
            ("↵", "go to"),
            ("/ ", "search"),
            ("r", "read"),
            ("d", "dismiss"),
            ("A", "show all"),
        ],
        Section::Jobs => &[
            ("↵", "run"),
            ("r", "re-run"),
            ("s", "stop"),
            ("o", "output"),
            ("j/k", "select"),
        ],
        Section::Logs => &[
            ("j/k", "row"),
            ("/ ", "filter"),
            ("l", "level"),
            ("y", "copy"),
            ("e", "export"),
        ],
        Section::Problems => &[("↵", "open"), ("j/k", "select")],
        Section::Symbols => &[
            ("↵", "go to def"),
            ("r", "refs"),
            ("h", "hover"),
            ("o", "outline"),
            ("j/k", "select"),
        ],
        Section::Media => &[
            ("space", "play/pause"),
            ("n/p", "next/prev"),
            ("s", "shuffle"),
            ("L", "loop"),
            ("≡", "playlist"),
        ],
        Section::Share => &[("j/k", "row"), ("↵", "copy url")],
        Section::Forward => &[("j/k", "row"), ("o", "open in browser"), ("↵", "copy url")],
        // Row-nav-only sections (Debug, Sandbox, Db, Telemetry, Keys, Across, …).
        _ => &[("j/k", "row")],
    };
    // "esc back" leads every row-mode hint list so the exit path is always visible.
    let mut result: Vec<(String, String)> = vec![("esc".to_string(), "back".to_string())];
    result.extend(pairs.iter().map(|(c, l)| (c.to_string(), l.to_string())));
    result
}

/// One pin's slot in the top strip: where it sits and how to label it. The
/// emulator is looked up by the caller via `pane`.
pub struct StripCell {
    pub pane: crate::center::PaneId,
    pub rect: Rect,
    pub label: String,
    pub glyph: char,
    pub focused: bool,
}

/// Render the top pinned-program strip: for each cell, a 1-row accent header
/// (`glyph label`) then its live pane below. A 1-col gap between cells reads as a
/// divider. The strip background is painted first so empty slack is themed.
pub fn draw_strip<'a>(
    surface: &mut Surface,
    strip: Rect,
    cells: &[StripCell],
    accent: &str,
    lookup: impl Fn(crate::center::PaneId) -> Option<&'a dyn PaneEmulator>,
) {
    if strip.rows == 0 || strip.cols == 0 {
        return;
    }
    fill(surface, strip, col(S::Bg0));
    let accent_c = theme_color(accent);
    let dim = col(S::Dim);
    for cell in cells {
        if cell.rect.rows == 0 || cell.rect.cols == 0 {
            continue;
        }
        // Header row (chrome furniture — the panel tint).
        let header_bg = col(S::Panel);
        let header_rect = Rect {
            x: cell.rect.x,
            y: cell.rect.y,
            cols: cell.rect.cols,
            rows: 1,
        };
        fill(surface, header_rect, header_bg);
        let fg = if cell.focused { accent_c } else { dim };
        let text = format!(" {} {} ", cell.glyph, cell.label);
        draw_text(
            surface,
            cell.rect.x,
            cell.rect.y,
            &text,
            fg,
            header_bg,
            cell.rect.cols,
        );
        // Pane body below the header.
        if cell.rect.rows > 1
            && let Some(emu) = lookup(cell.pane)
        {
            let body = Rect {
                x: cell.rect.x,
                y: cell.rect.y + 1,
                cols: cell.rect.cols,
                rows: cell.rect.rows - 1,
            };
            compose_pane(surface, emu, body);
        }
    }
}

/// Draw the surrounding chrome (sidebar/panel/masthead/statusbar) — the center
/// is filled separately by [`render_tab`].
pub fn draw_chrome(
    surface: &mut Surface,
    chrome: &crate::layout::ChromeLayout,
    model: &FrameModel,
    panel_ui: &crate::panel::PanelUi,
) {
    if let Some(sb) = chrome.sidebar {
        draw_sidebar(surface, sb, model);
    }
    if let Some(pn) = chrome.panel {
        draw_panel(surface, pn, model, panel_ui);
    }
    draw_columns_frame(surface, chrome);
    draw_center_tabs(surface, chrome.center_tabs, model);
    crate::chrome::draw_drawer(surface, chrome.drawer, chrome.drawer_divider, model);
    draw_masthead(surface, chrome, model);
    draw_statusbar(surface, chrome.statusbar, model);
}

/// The horizontal rule that caps all three columns directly below the
/// masthead. The columns themselves separate by tint alone: the 1-col gutters
/// either side of the center stay on the dark `bg0`, so the terminal well
/// reads clearly against the tinted sidebar/panel without any vertical bars.
fn draw_columns_frame(surface: &mut Surface, chrome: &crate::layout::ChromeLayout) {
    if chrome.divider.rows > 0 {
        let line = "\u{2500}".repeat(chrome.divider.cols);
        draw_text(
            surface,
            chrome.divider.x,
            chrome.divider.y,
            &line,
            col(S::Border),
            col(S::Panel),
            chrome.divider.cols,
        );
    }
    // The bottom drawer's horizontal rule, matching the top divider — the seam
    // that gives the popped-up drawer a real panel edge.
    #[allow(clippy::collapsible_if)]
    if let Some(div) = chrome.drawer_divider {
        if div.rows > 0 {
            let line = "\u{2500}".repeat(div.cols);
            draw_text(
                surface,
                div.x,
                div.y,
                &line,
                col(S::Border),
                col(S::Panel),
                div.cols,
            );
        }
    }
}

pub fn draw_drawer(
    _surface: &mut Surface,
    _drawer: Option<Rect>,
    _drawer_divider: Option<Rect>,
    _model: &FrameModel,
) {
    // Left empty: the terminal well clears its own background, and the PTY
    // paints the content. The drawer divider is drawn by draw_columns_frame.
}

/// Compose a multi-pane tab: lay the `center` tree out within `chrome.center`
/// with every pane's 1-cell frame ring reserved, paint each visible pane's
/// content (resolved via `lookup`), draw the frames (focused ring in the
/// focus color when the center zone owns the keyboard), then the chrome.
#[allow(clippy::too_many_arguments)]
/// Draw the relaunch prompt for a pane along the bottom row of its content rect:
/// `↻ <cmd> — Enter to relaunch · Esc to dismiss`. Painted over live content
/// (a resurrected shell) or a crash husk; cleared once the user acts.
fn draw_relaunch_overlay(surface: &mut Surface, content: Rect, cmd: &str) {
    if content.rows == 0 || content.cols < 4 {
        return;
    }
    let y = content.y + content.rows - 1;
    let bar = format!(" \u{21bb} {cmd} \u{2014} Enter to relaunch \u{00b7} Esc to dismiss ");
    let row = Rect {
        x: content.x,
        y,
        cols: content.cols,
        rows: 1,
    };
    fill(surface, row, col(S::Raise));
    draw_text(
        surface,
        content.x,
        y,
        &bar,
        col(S::Text),
        col(S::Raise),
        content.cols,
    );
}

/// Compose the center band: every visible pane's terminal content + the card
/// border ring (or the loading splash when no pane is live yet). This is the
/// pane half of a frame; [`draw_chrome`] is the chrome half. They write
/// disjoint cells (chrome owns the sidebar/panel/bars; this owns the center
/// interior), so the damage-tracked loop can recompose one without the other.
#[allow(clippy::too_many_arguments)]
pub fn render_panes<'a>(
    surface: &mut Surface,
    chrome: &crate::layout::ChromeLayout,
    center: &crate::center::CenterTree,
    focused: crate::center::PaneId,
    model: &FrameModel,
    lookup: impl Fn(crate::center::PaneId) -> Option<&'a dyn PaneEmulator>,
    title_of: &dyn Fn(crate::center::PaneId) -> String,
    relaunch_of: &dyn Fn(crate::center::PaneId) -> Option<String>,
) {
    let frames = center.layout_framed(chrome.center);
    // "Empty center" = no visible leaf has a live emulator behind it (fresh
    // launch before the first pane materializes, or every pane died). The
    // splash replaces what used to render as a black hole, and disappears on
    // the exact frame a pane shows up.
    let any_live = frames.iter().any(|(id, _, _)| lookup(*id).is_some());
    // Show the loading splash whenever pane-launch steps are in progress,
    // even on a resurrected session (any_live may be false before the PTY
    // forks, and we want the progress display visible immediately).
    let show_splash = !any_live || !model.load_steps.is_empty();
    if !show_splash {
        // The pane card owns the full center band. Paint the outside/ring
        // background before composing terminal content so no default black halo
        // can remain around the blue/white focus divider.
        fill(surface, chrome.center, col(S::Panel));
        for (id, _, content) in &frames {
            if let Some(emu) = lookup(*id) {
                compose_pane(surface, emu, *content);
            }
            // A pane awaiting relaunch (resurrected with a remembered command,
            // or a crashed husk) shows a one-line prompt over its bottom row.
            if let Some(cmd) = relaunch_of(*id) {
                draw_relaunch_overlay(surface, *content, &cmd);
            }
        }
        // The focused pane keeps a distinct ring at all times: focus blue
        // while the center owns the keyboard, white while the sidebar/panel
        // does — so the return target stays obvious without reading as live.
        let ring = if model.center_focused {
            col(S::Focus)
        } else {
            col(S::Text)
        };
        crate::borders::draw_pane_frames(
            surface,
            &frames,
            Some(focused),
            &crate::borders::FrameStyle {
                border: col(S::Border),
                focus: ring,
                bg: col(S::Panel),
                title: col(S::Dim),
                title_focused: ring,
            },
            title_of,
        );
    } else {
        crate::logotype::draw_splash(surface, chrome.center, model);
    }
}

/// Repaint a single pane's card ring into `surface`, reusing the exact style the
/// full path applies in [`render_panes`]. The partial render paths (scroll /
/// selection drag / incremental pane output) recompose only pane *content*; the
/// card border lives in the 1-cell frame ring *outside* that content, so without
/// this the border can be left stale — or, when a wide glyph composes at the last
/// content column, partially overwritten — producing the gaps reported on the
/// right edge while scrolling. Drawing only the touched pane keeps the bounded
/// diff minimal. No-ops when `pane` has no card (e.g. the drawer, which is a
/// separate reserved rect and never appears in `frames`).
pub fn redraw_pane_card(
    surface: &mut Surface,
    frames: &[(crate::center::PaneId, Rect, Rect)],
    pane: crate::center::PaneId,
    focused: crate::center::PaneId,
    model: &FrameModel,
    title_of: &dyn Fn(crate::center::PaneId) -> String,
) {
    let Some(entry) = frames.iter().find(|(id, _, _)| *id == pane).copied() else {
        return;
    };
    // Same ring rule as render_panes: focus blue while the center owns the
    // keyboard, white otherwise, so the return target stays obvious.
    let ring = if model.center_focused {
        col(S::Focus)
    } else {
        col(S::Text)
    };
    crate::borders::draw_pane_frames(
        surface,
        &[entry],
        Some(focused),
        &crate::borders::FrameStyle {
            border: col(S::Border),
            focus: ring,
            bg: col(S::Panel),
            title: col(S::Dim),
            title_focused: ring,
        },
        title_of,
    );
}

/// Compose a full frame: the center panes ([`render_panes`]) plus the chrome
/// ([`draw_chrome`]). The damage-tracked loop calls the two halves separately
/// for incremental recompose; this wrapper is the full-repaint path + tests.
#[allow(clippy::too_many_arguments)]
pub fn render_tab<'a>(
    surface: &mut Surface,
    chrome: &crate::layout::ChromeLayout,
    center: &crate::center::CenterTree,
    focused: crate::center::PaneId,
    model: &FrameModel,
    panel_ui: &crate::panel::PanelUi,
    lookup: impl Fn(crate::center::PaneId) -> Option<&'a dyn PaneEmulator>,
    title_of: &dyn Fn(crate::center::PaneId) -> String,
    relaunch_of: &dyn Fn(crate::center::PaneId) -> Option<String>,
) {
    render_panes(
        surface,
        chrome,
        center,
        focused,
        model,
        lookup,
        title_of,
        relaunch_of,
    );
    draw_chrome(surface, chrome, model, panel_ui);
}

#[cfg(test)]
#[path = "chrome_tests.rs"]
mod tests;
