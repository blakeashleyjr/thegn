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
    /// `[disk].warn_threshold_gb`: the statusbar disk badge trips when the sum
    /// of all worktree sizes (in `sidebar_status.disk_sizes`) exceeds this many
    /// GiB. 0 disables the badge. Config-derived, set in `build_model`.
    pub disk_warn_threshold_gb: u64,
    /// Active worktree's total size (bytes), for the bottom `disk` widget next
    /// to LOC. From the off-loop scan cache; `None` until first scanned.
    pub active_worktree_disk: Option<u64>,
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
    /// tokei line count for the active worktree (bottom-bar widget).
    pub loc: Option<u64>,
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
    /// Deterministic container name for the active worktree path. The sandbox
    /// panel uses this to show the sandbox for the selected worktree instead of
    /// the first superzej-owned container on the machine.
    pub active_container_name: String,
    /// DB-stored sandbox backend label for the active worktree (e.g. "bwrap",
    /// "podman-rootless", "host"). Used to show non-OCI sandboxes (bwrap,
    /// systemd) as green even though they have no container entry.
    pub active_sandbox_backend: String,
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
        }
    }
    pub fn active(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: StepState::Active,
        }
    }
    pub fn done(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: StepState::Done,
        }
    }
    pub fn failed(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            state: StepState::Failed,
        }
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

    /// True when a freshly hydrated model carries no render-affecting change
    /// versus the one on screen — i.e. the 2 s "safety" refresh tick produced
    /// byte-identical git/db data. The event loop uses this to drain the
    /// hydration result without repainting, keeping idle CPU at ~0%.
    ///
    /// Compares exactly the fields [`crate::hydrate::build_model`] populates
    /// (plus `status`), and nothing else: stats/metrics/containers/accent/
    /// bars/pins/app-tabs are owned by other handlers or config and have their
    /// own dirty triggers, while the session-derived tab/sidebar fields are
    /// stable during an idle period. KEEP THIS IN SYNC WITH `build_model`.
    pub fn hydration_eq(&self, other: &Self) -> bool {
        self.worktree == other.worktree
            && self.tabs == other.tabs
            && self.active_tab == other.active_tab
            && self.sidebar_workspaces == other.sidebar_workspaces
            && self.sidebar_db_worktrees == other.sidebar_db_worktrees
            && self.sidebar_status == other.sidebar_status
            && self.loc == other.loc
            && self.active_container_name == other.active_container_name
            && self.active_sandbox_backend == other.active_sandbox_backend
            && self.container_events == other.container_events
            && self.timeline == other.timeline
            && self.status == other.status
            && self.panel == other.panel
            && self.disk_warn_threshold_gb == other.disk_warn_threshold_gb
            && self.active_worktree_disk == other.active_worktree_disk
    }
}

/// The worktree label parts for the nav row: `(workspace, leaf)`. The
/// workspace prefix renders uppercased (display form of the canonical slug);
/// single-segment names render as the leaf alone.
fn worktree_parts(model: &FrameModel) -> Option<(String, String)> {
    if model.worktree.is_empty() {
        return None;
    }
    match model.worktree.split_once('/') {
        Some((ws, leaf)) => Some((ws.to_uppercase(), leaf.to_string())),
        None => Some((String::new(), model.worktree.clone())),
    }
}

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
    let end = pin_chips_start(model, strip);
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
    let chips_end = pin_chips_start(model, strip);

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
    Ci,
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
        "loc" => model.loc.map(|n| {
            let compact = if n >= 1_000_000 {
                format!("{:.1}M", n as f64 / 1_000_000.0)
            } else if n >= 1_000 {
                format!("{:.1}k", n as f64 / 1_000.0)
            } else {
                n.to_string()
            };
            w(format!("{compact} LOC"), col(S::Dim))
        }),
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
    // CI rollup badge (AV group, item 158): a red ✗ chip when recent runs have
    // failures, an amber ● chip while runs are in flight; silent when all green
    // (mirrors the "clean is quiet" notification posture). Only when CI is
    // configured and the cache is warm (`ci_runs` non-empty).
    if !model.panel.ci_runs.is_empty() {
        use superzej_core::ci::CiState;
        let fail = model
            .panel
            .ci_runs
            .iter()
            .filter(|r| r.state == CiState::Fail)
            .count();
        let running = model
            .panel
            .ci_runs
            .iter()
            .filter(|r| r.state == CiState::Running)
            .count();
        if fail > 0 {
            items.push((
                BarItemId::Badge(BarBadge::Ci),
                vec![Seg::chip(
                    Tok::Hue(superzej_core::theme::Hue::Red),
                    format!(" {} {fail} CI ", crate::caps::active_glyphs().cross),
                )],
            ));
        } else if running > 0 {
            items.push((
                BarItemId::Badge(BarBadge::Ci),
                vec![Seg::chip(
                    Tok::Hue(superzej_core::theme::Hue::Amber),
                    format!(" {} {running} CI ", crate::caps::active_glyphs().dot_filled),
                )],
            ));
        }
    }
    // Disk-usage badge: trips when the sum of all worktree sizes crosses
    // `[disk].warn_threshold_gb` — amber past the threshold, red past 2×. The
    // 300GB-of-target/ failure mode accrued unnoticed; this is the missing
    // feedback loop. Silent below the threshold and when it's disabled (0) or
    // the scan hasn't run (empty `disk_sizes`).
    if model.disk_warn_threshold_gb > 0 && !model.sidebar_status.disk_sizes.is_empty() {
        let total: u64 = model
            .sidebar_status
            .disk_sizes
            .values()
            .map(|&(t, _)| t.max(0) as u64)
            .sum();
        let gib = 1024 * 1024 * 1024;
        let threshold = model.disk_warn_threshold_gb * gib;
        if total > threshold {
            let hue = if total > threshold.saturating_mul(2) {
                superzej_core::theme::Hue::Red
            } else {
                superzej_core::theme::Hue::Amber
            };
            items.push((
                BarItemId::Badge(BarBadge::DiskWarn),
                vec![Seg::chip(
                    Tok::Hue(hue),
                    format!(" \u{26c1} {} ", superzej_core::disk::human(total)),
                )],
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
/// Returns the seg run for [`draw_line`] plus each item's `(id, x_offset, width)`
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

    // Only visible rows are listed; the selection index is into that subset.
    let visible: Vec<&crate::sidebar::SidebarRow> =
        model.sidebar_rows.iter().filter(|r| r.visible).collect();

    // Pre-compute the Ctrl+1..9 quick-jump slot for each visible row, matching
    // `sidebar_workspace_order` exactly: only switchable (DB-backed, has a
    // `worktree_path`) workspace rows count, in visible order, slots 1..=9.
    let workspace_slots: Vec<Option<u8>> = {
        let mut next: u8 = 1;
        visible
            .iter()
            .map(|r| {
                if r.kind == crate::sidebar::RowKind::Workspace && r.worktree_path.is_some() {
                    let slot = (next <= 9).then_some(next);
                    next += 1;
                    slot
                } else {
                    None
                }
            })
            .collect()
    };

    let metrics_rows = if rect.rows > 10 && !model.metrics.targets.is_empty() {
        6.min(rect.rows.saturating_sub(4))
    } else {
        0
    };
    let list_bottom = rect.y + rect.rows.saturating_sub(metrics_rows);

    // Rows advance a running `y` (not `rect.y + 2 + i`) so a first-class
    // section banner can claim a blank breathing line above itself; text,
    // highlight, and badge overpaint all derive from this same `y`.
    let mut y = rect.y + 2;
    for (i, row) in visible.iter().enumerate() {
        if row.kind == crate::sidebar::RowKind::SectionHeading && i > 0 {
            y += 1; // breathing gap above the banner
        }
        if y >= list_bottom {
            break;
        }
        // A section banner renders like the "WORKSPACES" title — bold, in the
        // Text color, no selection pill / status tail / badges.
        if row.kind == crate::sidebar::RowKind::SectionHeading {
            // A section heading is never a workspace, so it carries no
            // Ctrl+1..9 quick-jump slot.
            let composed = compose_sidebar_row(row, None, None);
            draw_text_bold(
                surface,
                rect.x,
                y,
                &format!(" {}", composed.text),
                col(S::Text),
                col(S::Panel),
                rect.cols,
            );
            y += 1;
            continue;
        }
        let selected = i == model.sidebar_selected;
        let marked = model.sidebar_marked.contains(&i);
        // The active worktree/tab row carries the focus-tint pill (same
        // highlight language as the masthead's active chip), blended over the
        // zone's panel tint so it never punches a dark hole in the surface.
        let pill = theme_color(&theme::blend_over(&focus_rgb(), &panel_rgb(), 0.16));
        let bg = if selected {
            col(S::Panel2)
        } else if row.active {
            pill
        } else if marked {
            col(S::Raise)
        } else {
            col(S::Panel)
        };
        if selected || marked || row.active {
            fill(
                surface,
                Rect {
                    x: rect.x,
                    y,
                    cols: rect.cols,
                    rows: 1,
                },
                bg,
            );
        }
        // Left-edge accent bar marks the cursor only while focused, so a stale
        // selection isn't mistaken for focus.
        if selected && model.sidebar_focused {
            draw_text(surface, rect.x, y, "\u{2590}", col(S::Focus), bg, 1);
        }

        let window_title = row
            .worktree_path
            .as_deref()
            .and_then(|p| model.sidebar_window_titles.get(p))
            .map(String::as_str);
        let composed = compose_sidebar_row(row, window_title, workspace_slots[i]);
        let fg = if row.active {
            col(S::Focus)
        } else if selected {
            col(S::Text)
        } else {
            col(S::Dim)
        };
        draw_text(
            surface,
            rect.x + 1,
            y,
            &composed.text,
            fg,
            bg,
            rect.cols.saturating_sub(1),
        );
        // Recolor the activity dot (item 20) in its own state color: white while
        // the worktree is working, red when its agent is waiting for input. The
        // dormant state draws no dot, so `activity_col` is None and we skip.
        if let Some(dc) = composed.activity_col {
            let dx = rect.x + 1 + dc;
            if dx < rect.x + rect.cols {
                draw_text(
                    surface,
                    dx,
                    y,
                    activity_dot(row.activity).trim_end(),
                    activity_dot_color(row.activity),
                    bg,
                    (rect.x + rect.cols).saturating_sub(dx),
                );
            }
        }
        // Overpaint the status segment (git/agent/activity) in its own colors,
        // right after the label, when there's room. Track the running column so
        // badges paint after the status tail.
        let mut col_off = composed.status_col;
        if let Some(seg) = &composed.status {
            let sx = rect.x + 1 + col_off;
            if sx < rect.x + rect.cols {
                draw_text(
                    surface,
                    sx,
                    y,
                    seg,
                    status_seg_color(row),
                    bg,
                    (rect.x + rect.cols).saturating_sub(sx),
                );
            }
            col_off += seg.chars().count();
        }
        // Badges (item 28): PR / unread / alert counts, each in its own color.
        for badge in &composed.badges {
            let sx = rect.x + 1 + col_off;
            if sx >= rect.x + rect.cols {
                break;
            }
            draw_text(
                surface,
                sx,
                y,
                &badge.text,
                badge.color,
                bg,
                (rect.x + rect.cols).saturating_sub(sx),
            );
            col_off += badge.text.chars().count();
        }
        y += 1;
    }

    // Row context menu overlay (item 27).
    if let Some(menu) = &model.sidebar_menu {
        draw_row_menu(surface, rect, menu, accent);
    }

    if metrics_rows > 0 {
        draw_metrics_section(
            surface,
            Rect {
                x: rect.x,
                y: rect.y + rect.rows - metrics_rows,
                cols: rect.cols,
                rows: metrics_rows,
            },
            model,
        );
    }
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

/// The text composed for a row plus where its status segment begins (so the
/// renderer can recolor it). `text` already includes caret/connector/label and
/// a trailing space before the status; `status` is the git/agent/activity tail.
struct ComposedRow {
    text: String,
    status: Option<String>,
    status_col: usize,
    /// Char column of the activity dot within `text`, when the row renders one
    /// (item 20). The renderer over-paints just this glyph in its state color.
    activity_col: Option<usize>,
    /// Badge segments (item 28): PR / unread / alert counts, each with its own
    /// color, drawn after the git status tail.
    badges: Vec<Badge>,
}

/// A single sidebar badge segment with its own color.
struct Badge {
    text: String,
    color: ColorAttribute,
}

fn compose_sidebar_row(
    row: &crate::sidebar::SidebarRow,
    window_title: Option<&str>,
    workspace_slot: Option<u8>,
) -> ComposedRow {
    use crate::sidebar::RowKind;
    let mut text = String::new();
    let mut activity_col = None;

    // Quick-jump digit on a switchable workspace row (Ctrl+1..9). Prepended
    // before the caret so the later char-offset bookkeeping (status/badge
    // columns) folds it in automatically.
    if row.kind == RowKind::Workspace
        && let Some(n) = workspace_slot
    {
        text.push_str(&format!("{n} "));
    }

    match row.kind {
        RowKind::Folder => {
            text.push_str("  ");
            let caret = if row.collapsed {
                "\u{25b8}"
            } else {
                "\u{25be}"
            };
            text.push_str(caret);
            text.push(' ');
            text.push_str("\u{1f4c2} "); // open folder glyph
            text.push_str(&row.label);
        }
        RowKind::SectionHeading => {
            // Bold-banner styling (matching the "WORKSPACES" title) is applied
            // by the draw loop; here we only carry the label text.
            text.push_str(&row.label);
        }
        RowKind::Workspace | RowKind::TerminalHost => {
            let caret = if row.collapsed {
                "\u{25b8}"
            } else {
                "\u{25be}"
            };
            text.push_str(caret);
            text.push(' ');
            if row.kind == RowKind::TerminalHost {
                // Host group glyph: 💻 local vs 🌐 remote, keyed off the
                // representative connection set in `build_rows`.
                let local = row
                    .terminal_connection
                    .as_deref()
                    .map(str::is_empty)
                    .unwrap_or(true);
                text.push_str(if local { "💻 " } else { "🌐 " });
            } else if row.dir {
                // A non-git "dir" workspace gets a folder glyph so it reads
                // differently from a repo workspace.
                text.push_str("\u{1f4c1} ");
            }
            text.push_str(&row.label);
        }
        RowKind::Terminal => {
            text.push_str("  ");
            // Render terminal with a distinct visual language
            if let Some(loc) = &row.terminal_connection {
                if loc.starts_with("ssh") {
                    text.push_str("🌐 ");
                } else if loc.starts_with("mosh") {
                    text.push_str("🚀 ");
                } else {
                    text.push_str("💻 ");
                }
            } else {
                text.push_str("💻 ");
            }
            text.push_str(&row.label);
        }
        RowKind::Worktree => {
            text.push_str("  ");
            let dot = activity_dot(row.activity);
            if !dot.is_empty() {
                activity_col = Some(text.chars().count());
            }
            text.push_str(dot);
            if !dot.is_empty() {
                text.push(' ');
            }
            // Dynamic title: `[PR: <n> | <window-title>]`, else the window
            // title, else the branch (`row.label`). See `compose_row_label`.
            text.push_str(&crate::sidebar::compose_row_label(
                row.pr_number,
                window_title,
                &row.label,
            ));
        }
    }

    // A non-default execution environment badges before the backend, so a
    // worktree pinned to `company-k8s`/`datonya`/etc. is visible at a glance.
    if let Some(env) = &row.env_name
        && !env.is_empty()
        && env != "default"
    {
        text.push_str(&format!(" «{env}»"));
    }

    // Agent glyph (item 19) sits just after the label.
    if let Some(backend) = &row.sandbox_backend
        && !backend.is_empty()
        && backend != "none"
        && backend != "host"
    {
        text.push_str(&format!(" ({backend})"));
    }

    if let Some(agent) = &row.agent {
        text.push(' ');
        text.push_str(&superzej_core::theme::agent_glyph(agent));
    }

    // Git glyphs (item 18) form the recolored status tail.
    let status = row.git.map(|g| {
        let gl = crate::caps::active_glyphs();
        let mut s = String::new();
        if g.dirty {
            s.push(' ');
            s.push_str(gl.dot_filled); // ● / *
        }
        if g.ahead > 0 {
            s.push_str(&format!(" {}{}", gl.arrow_up, g.ahead)); // ↑N / ^N
        }
        if g.behind > 0 {
            s.push_str(&format!(" {}{}", gl.arrow_down, g.behind)); // ↓N / vN
        }
        s
    });
    let status = status.filter(|s| !s.is_empty());
    let status_col = text.chars().count();
    let badges = compose_badges(row);
    ComposedRow {
        text,
        status,
        status_col,
        activity_col,
        badges,
    }
}

/// Build the per-row badge segments (item 28): open PRs, unread notifications,
/// and alerts. Only non-zero counts render. Each badge is a glyph + count and
/// carries its own color so the renderer can paint them distinctly.
fn compose_badges(row: &crate::sidebar::SidebarRow) -> Vec<Badge> {
    let mut badges = Vec::new();
    // PR badge: open PRs for this worktree's branch (green = good state).
    if let Some(pr) = row.pr_count.filter(|&c| c > 0) {
        badges.push(Badge {
            text: format!(" \u{2b21}{pr}"), // ⬡N
            color: theme_color(theme::GREEN),
        });
    }
    // Unread badge: unread notifications (blue = informational).
    if row.unread_count > 0 {
        badges.push(Badge {
            text: format!(" \u{2709}{}", row.unread_count), // ✉N
            color: theme_color(theme::BLUE),
        });
    }
    // Alert badge: test failures, agent failures, log errors (red = action).
    if row.alert_count > 0 {
        badges.push(Badge {
            text: format!(" \u{26a0}{}", row.alert_count), // ⚠N
            color: theme_color(theme::RED),
        });
    }
    // Disk-size badge (item 152/413): the worktree's size, dim by default and
    // amber when the reclaimable `target/` dominates (>1 GiB and >half the
    // total) — a nudge that `superzej clean` would recover real space. Only
    // populated when the off-loop disk scan has run (i.e. `[disk].show_sizes`).
    if let Some(total) = row.disk_bytes {
        let target = row.target_bytes.unwrap_or(0);
        let heavy = target > 1024 * 1024 * 1024 && target * 2 > total;
        badges.push(Badge {
            text: format!(" {}", superzej_core::disk::human(total)),
            color: if heavy {
                theme_color(theme::AMBER)
            } else {
                theme_color(theme::DIM)
            },
        });
    }
    badges
}

/// The activity dot prefix for a worktree row (item 20), recolored at render by
/// [`activity_dot_color`]: `Active` (worktree busy / agent working) is a filled
/// white ●; `Waiting` (idle — agent stuck, unread) is a filled red ●; `Read`
/// (the user has seen it but it is still stuck) is a hollow red ○; dormant
/// (`None`) shows nothing.
fn activity_dot(state: crate::sidebar::ActivityState) -> &'static str {
    use crate::sidebar::ActivityState::*;
    let g = crate::caps::active_glyphs();
    match state {
        Active => g.dot_filled,  // ● / *
        Waiting => g.dot_filled, // ● / *
        Read => g.dot_hollow,    // ○ / o
        None => "",
    }
}

/// The color the activity dot is over-painted in, per state (configurable via
/// `[theme.colors] activity_active` / `activity_waiting`). Both red states share
/// the `activity_waiting` slot — filled vs hollow is glyph-only. `None` never draws.
fn activity_dot_color(state: crate::sidebar::ActivityState) -> ColorAttribute {
    use crate::sidebar::ActivityState::*;
    match state {
        Active => col(S::ActivityActive),
        Waiting | Read => col(S::ActivityWaiting),
        None => col(S::Dim),
    }
}

fn status_seg_color(row: &crate::sidebar::SidebarRow) -> ColorAttribute {
    // Dirty dominates the tail color; otherwise neutral-dim and the ↑↓ read
    // fine. (Per-glyph coloring is a later refinement.)
    match row.git {
        Some(g) if g.dirty => theme_color(theme::AMBER),
        Some(g) if g.ahead > 0 || g.behind > 0 => col(S::Dim),
        _ => col(S::Dim),
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
        Section::Debug | Section::Sandbox | Section::Db | Section::Telemetry | Section::Keys => {
            &[("j/k", "row")]
        }
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
mod tests {
    use super::*;
    use crate::emulator::AlacrittyEmulator;
    use crate::layout;

    fn lines(s: &Surface) -> Vec<String> {
        s.screen_chars_to_string()
            .lines()
            .map(|l| l.to_string())
            .collect()
    }

    #[test]
    fn hydration_eq_ignores_non_hydration_fields() {
        let base = FrameModel::default();
        // Fields owned by other handlers / config must NOT count as a change,
        // or the idle guard would still repaint on every safety tick.
        let mut other = base.clone();
        other.accent = "ff0000".into();
        other.pins.push(crate::pins::PinChip {
            index: 0,
            label: "p".into(),
            glyph: '●',
        });
        other.stats.cpu_pct = Some(99);
        other.app_tabs.push("chat".into());
        other.sidebar_selected = 3;
        other.center_focused = !base.center_focused;
        assert!(
            base.hydration_eq(&other),
            "non-hydration fields should not trip the idle guard"
        );
    }

    #[test]
    fn hydration_eq_detects_real_changes() {
        let base = FrameModel::default();
        let mut panel_changed = base.clone();
        panel_changed.panel.branch = "feature".into();
        assert!(
            !base.hydration_eq(&panel_changed),
            "panel change must repaint"
        );

        let mut sidebar_changed = base.clone();
        sidebar_changed
            .sidebar_status
            .activity
            .insert("tab".into(), crate::sidebar::ActivityState::Active);
        assert!(
            !base.hydration_eq(&sidebar_changed),
            "sidebar status change must repaint"
        );

        let mut loc_changed = base.clone();
        loc_changed.loc = Some(42);
        assert!(!base.hydration_eq(&loc_changed), "loc change must repaint");
    }

    /// Build a minimal sidebar row for renderer tests.
    fn row(kind: crate::sidebar::RowKind, label: &str) -> crate::sidebar::SidebarRow {
        crate::sidebar::SidebarRow {
            kind,
            depth: if kind == crate::sidebar::RowKind::Workspace {
                0
            } else {
                1
            },
            label: label.into(),
            workspace_slug: "app".into(),
            tab_target: None,
            active: false,
            worktree_path: None,
            pin_key: label.into(),
            branch: None,
            git: None,
            agent: None,
            sandbox_backend: None,
            env_name: None,
            activity: crate::sidebar::ActivityState::None,
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
        }
    }

    #[test]
    fn redraw_pane_card_restores_a_nibbled_border() {
        use crate::center::CenterTree;
        let tree = CenterTree::single(1);
        let area = Rect {
            x: 0,
            y: 0,
            cols: 20,
            rows: 5,
        };
        let frames = tree.layout_framed(area);
        let (_, _frame, content) = frames[0];
        let right = area.x + area.cols - 1;
        let model = FrameModel {
            center_focused: true,
            ..Default::default()
        };
        let mut s = Surface::new(20, 5);
        redraw_pane_card(&mut s, &frames, 1, 1, &model, &|_| String::new());
        assert_eq!(
            s.screen_cells()[2][right].str(),
            "\u{2502}",
            "card border drawn"
        );
        // Simulate content overflowing into the border: a wide glyph straddling
        // the last content column and the border column.
        s.add_change(Change::CursorPosition {
            x: Position::Absolute(content.x + content.cols - 1),
            y: Position::Absolute(2),
        });
        s.add_change(Change::Text("\u{6f22}".into()));
        assert_ne!(
            s.screen_cells()[2][right].str(),
            "\u{2502}",
            "the wide glyph nibbled the border (the bug)"
        );
        // Repainting the card heals every interior border row.
        redraw_pane_card(&mut s, &frames, 1, 1, &model, &|_| String::new());
        for y in (area.y + 1)..(area.y + area.rows - 1) {
            assert_eq!(
                s.screen_cells()[y][right].str(),
                "\u{2502}",
                "border solid again at row {y}"
            );
        }
    }

    #[test]
    fn redraw_pane_card_skips_panes_without_a_card() {
        use crate::center::CenterTree;
        let frames = CenterTree::single(1).layout_framed(Rect {
            x: 0,
            y: 0,
            cols: 20,
            rows: 5,
        });
        let mut s = Surface::new(20, 5);
        // A pane id not in `frames` (e.g. the drawer) is a no-op: nothing drawn.
        redraw_pane_card(&mut s, &frames, 99, 99, &FrameModel::default(), &|_| {
            String::new()
        });
        assert!(
            s.screen_chars_to_string().trim().is_empty(),
            "no card drawn for an absent pane"
        );
    }

    #[test]
    fn sidebar_focus_indicator_appears_only_when_focused() {
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 24,
            rows: 6,
        };
        use crate::sidebar::RowKind;
        let model = FrameModel {
            sidebar_rows: vec![
                row(RowKind::Workspace, "app"),
                row(RowKind::Worktree, "home"),
            ],
            sidebar_selected: 1,
            sidebar_focused: true,
            ..Default::default()
        };
        let mut s = Surface::new(24, 6);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        // The cursor's left-edge accent bar appears only while focused.
        assert!(text.contains('\u{2590}'), "focused cursor bar: {text:?}");

        let mut unfocused = model.clone();
        unfocused.sidebar_focused = false;
        let mut s2 = Surface::new(24, 6);
        draw_sidebar(&mut s2, rect, &unfocused);
        let text2 = s2.screen_chars_to_string();
        assert!(
            !text2.contains('\u{2590}'),
            "no cursor bar when unfocused: {text2:?}"
        );
    }

    #[test]
    fn statusbar_items_includes_badges_so_they_are_navigable() {
        // The core fix: badges (notifications/agent/…) must be in the same
        // ordered item list nav steps, after the config widgets — otherwise the
        // cursor can never reach them (the original bug).
        let model = FrameModel {
            bars: superzej_core::config::BarsConfig {
                bottom_right: vec!["loc".into()],
                ..Default::default()
            },
            loc: Some(1234),
            panel: crate::panel::PanelData {
                alert_notifications: 2,
                ..Default::default()
            },
            agent_activity: Some(AgentActivity::default()),
            zoomed: true,
            ..Default::default()
        };
        let ids: Vec<BarItemId> = statusbar_items(&model)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(ids[0], BarItemId::Widget("loc".into()), "widgets first");
        assert!(ids.contains(&BarItemId::Badge(BarBadge::Notifications)));
        assert!(ids.contains(&BarItemId::Badge(BarBadge::Agent)));
        assert!(ids.contains(&BarItemId::Badge(BarBadge::Zoom)));
    }

    #[test]
    fn masthead_fit_sheds_items_when_narrow() {
        // Navigation enumerates exactly what draw shows, so the width-degraded
        // drop must shrink the navigable set too (no cursor on a hidden item).
        let model = FrameModel {
            bars: superzej_core::config::BarsConfig {
                top_right: vec!["cpu".into(), "mem".into(), "swap".into(), "date".into()],
                ..Default::default()
            },
            stats: superzej_metrics::StatsSnapshot {
                cpu_pct: Some(99),
                mem_gib: Some((4.0, 16.0)),
                swap_gib: Some((1.0, 8.0)),
                ..Default::default()
            },
            ..Default::default()
        };
        let (_, wide) = masthead_fit(&model, 200);
        let (_, narrow) = masthead_fit(&model, 24);
        assert_eq!(wide.len(), 4);
        assert!(
            narrow.len() < wide.len(),
            "narrow={} wide={}",
            narrow.len(),
            wide.len()
        );
        // cpu/mem survive longest (softest stats shed first).
        assert!(
            narrow
                .iter()
                .any(|(id, _)| *id == BarItemId::Widget("cpu".into()))
        );
    }

    #[test]
    fn workspace_slot_digits_skip_unswitchable_and_count_in_order() {
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 24,
            rows: 10,
        };
        use crate::sidebar::RowKind;
        // a: switchable → slot 1. b: live-fallback (no path) → no slot.
        // c: switchable → slot 2 (the counter skips b, matching
        // sidebar_workspace_order's filter_map).
        let mut a = row(RowKind::Workspace, "alpha");
        a.worktree_path = Some("/repo/alpha".into());
        let b = row(RowKind::Workspace, "bravo"); // worktree_path: None
        let mut c = row(RowKind::Workspace, "charlie");
        c.worktree_path = Some("/repo/charlie".into());
        let model = FrameModel {
            sidebar_rows: vec![a, b, c],
            ..Default::default()
        };
        let mut s = Surface::new(24, 10);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        assert!(
            text.contains("1 \u{25be} alpha"),
            "alpha is slot 1: {text:?}"
        );
        // bravo gets no digit (unswitchable) — only its bare caret + label.
        assert!(text.contains("\u{25be} bravo"), "bravo present: {text:?}");
        assert!(
            !text.contains("2 \u{25be} bravo") && !text.contains("1 \u{25be} bravo"),
            "bravo has no slot digit: {text:?}"
        );
        assert!(
            text.contains("2 \u{25be} charlie"),
            "charlie is slot 2 (bravo skipped): {text:?}"
        );
    }

    #[test]
    fn workspace_rows_show_quick_jump_digits_regardless_of_focus() {
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 24,
            rows: 6,
        };
        use crate::sidebar::RowKind;
        // A switchable workspace carries a `worktree_path`; an unswitchable
        // (live-fallback) one does not and so gets no slot — mirroring
        // `sidebar_workspace_order`.
        let mut ws = row(RowKind::Workspace, "app");
        ws.worktree_path = Some("/repo/app".into());
        let model = FrameModel {
            sidebar_rows: vec![ws, row(RowKind::Worktree, "home")],
            sidebar_selected: 1,
            sidebar_focused: true,
            ..Default::default()
        };
        let mut s = Surface::new(24, 6);
        draw_sidebar(&mut s, rect, &model);
        let focused = s.screen_chars_to_string();
        // Slot "1" is painted before the caret on the first switchable workspace.
        assert!(
            focused.contains("1 \u{25be} app"),
            "digit hint on workspace row: {focused:?}"
        );

        // The digit persists when the sidebar is not focused (user chose
        // always-on hints, unlike the focus-only cursor bar).
        let mut unfocused = model.clone();
        unfocused.sidebar_focused = false;
        let mut s2 = Surface::new(24, 6);
        draw_sidebar(&mut s2, rect, &unfocused);
        let text2 = s2.screen_chars_to_string();
        assert!(
            text2.contains("1 \u{25be} app"),
            "digit hint shown unfocused too: {text2:?}"
        );
    }

    #[test]
    fn sidebar_renders_glyphs_caret_dirty_and_agent() {
        use crate::sidebar::{ActivityState, GitGlyphs, RowKind};
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 30,
            rows: 8,
        };
        let mut ws = row(RowKind::Workspace, "app");
        ws.collapsed = false;
        let mut wt = row(RowKind::Worktree, "feat");
        wt.git = Some(GitGlyphs {
            dirty: true,
            ahead: 2,
            behind: 1,
        });
        wt.agent = Some("claude".into());
        wt.activity = ActivityState::Active;
        let model = FrameModel {
            sidebar_rows: vec![ws, wt],
            sidebar_selected: 0,
            sidebar_focused: true,
            ..Default::default()
        };
        let mut s = Surface::new(30, 8);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        assert!(text.contains('\u{25be}'), "expanded caret ▾: {text:?}"); // expanded workspace
        assert!(text.contains("feat"));
        assert!(text.contains('\u{2191}'), "ahead glyph ↑: {text:?}");
        assert!(text.contains('\u{2193}'), "behind glyph ↓: {text:?}");
        assert!(text.contains('C'), "agent glyph for claude: {text:?}");
    }

    #[test]
    fn sidebar_renders_badges_for_pr_unread_and_alert() {
        use crate::sidebar::RowKind;
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 6,
        };
        let mut ws = row(RowKind::Workspace, "app");
        ws.collapsed = false;
        let mut wt = row(RowKind::Worktree, "feat");
        wt.pr_count = Some(2);
        wt.unread_count = 3;
        wt.alert_count = 1;
        let model = FrameModel {
            sidebar_rows: vec![ws, wt],
            ..Default::default()
        };
        let mut s = Surface::new(40, 6);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        assert!(text.contains('\u{2b21}'), "PR badge glyph ⬡: {text:?}");
        assert!(text.contains('\u{2709}'), "unread badge glyph ✉: {text:?}");
        assert!(text.contains('\u{26a0}'), "alert badge glyph ⚠: {text:?}");
        // Counts render alongside the glyphs.
        assert!(text.contains('2') && text.contains('3') && text.contains('1'));
    }

    #[test]
    fn sidebar_renders_disk_size_badge() {
        use crate::sidebar::RowKind;
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 6,
        };
        let mut ws = row(RowKind::Workspace, "app");
        ws.collapsed = false;
        let mut wt = row(RowKind::Worktree, "feat");
        wt.disk_bytes = Some(70 * 1024 * 1024 * 1024); // 70G
        wt.target_bytes = Some(60 * 1024 * 1024 * 1024);
        let model = FrameModel {
            sidebar_rows: vec![ws, wt],
            ..Default::default()
        };
        let mut s = Surface::new(40, 6);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        assert!(
            text.contains("70G"),
            "size badge shows human size: {text:?}"
        );
    }

    #[test]
    fn masthead_disk_widget_shows_free_pct_with_inverted_colors() {
        let mut model = FrameModel::default();
        // High free → normal (dim); low free → red; mid → amber. Defaults:
        // warn 15, critical 5.
        model.stats.disk_free_pct = Some(72);
        let hi = masthead_widget("disk", &model).expect("disk widget present");
        assert!(hi.text.contains("72%"), "shows free %: {:?}", hi.text);
        assert_eq!(hi.fg, col(S::Dim), "ample free → dim");

        model.stats.disk_free_pct = Some(12);
        assert_eq!(
            masthead_widget("disk", &model).unwrap().fg,
            theme_color(theme::AMBER),
            "low free → amber"
        );

        model.stats.disk_free_pct = Some(3);
        assert_eq!(
            masthead_widget("disk", &model).unwrap().fg,
            theme_color(theme::RED),
            "critically low free → red"
        );

        // Absent until sampled.
        model.stats.disk_free_pct = None;
        assert!(masthead_widget("disk", &model).is_none());
    }

    #[test]
    fn bottombar_disk_widget_shows_active_worktree_size() {
        let mut model = FrameModel::default();
        assert!(
            bottombar_widget("disk", &model).is_none(),
            "hidden when size unknown"
        );
        model.active_worktree_disk = Some(7 * 1024 * 1024 * 1024); // 7G
        let wdg = bottombar_widget("disk", &model).expect("disk widget present");
        assert_eq!(wdg.text, "7GB");
    }

    #[test]
    fn statusbar_disk_badge_trips_above_threshold_only() {
        let chrome = layout::compute(160, 10, false, false);
        let mk = |total_gb: u64, threshold: u64| -> String {
            let mut sizes = std::collections::HashMap::new();
            sizes.insert(
                "/wt/a".to_string(),
                ((total_gb * 1024 * 1024 * 1024) as i64, 0i64),
            );
            let model = FrameModel {
                disk_warn_threshold_gb: threshold,
                sidebar_status: crate::sidebar::SidebarStatus {
                    disk_sizes: sizes,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut s = Surface::new(160, 10);
            draw_statusbar(&mut s, chrome.statusbar, &model);
            s.screen_chars_to_string()
        };
        // Above threshold → the ⛁ chip with the size appears.
        assert!(mk(150, 100).contains('\u{26c1}'), "trips above threshold");
        // Below threshold → silent.
        assert!(!mk(50, 100).contains('\u{26c1}'), "silent below threshold");
        // Disabled (0) → silent even when huge.
        assert!(!mk(500, 0).contains('\u{26c1}'), "silent when disabled");
    }

    #[test]
    fn sidebar_omits_zero_badges() {
        use crate::sidebar::RowKind;
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 6,
        };
        let mut ws = row(RowKind::Workspace, "app");
        ws.collapsed = false;
        let mut wt = row(RowKind::Worktree, "feat");
        // All zero/none: no badges should render.
        wt.pr_count = Some(0);
        wt.unread_count = 0;
        wt.alert_count = 0;
        let model = FrameModel {
            sidebar_rows: vec![ws, wt],
            ..Default::default()
        };
        let mut s = Surface::new(40, 6);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        assert!(
            !text.contains('\u{2b21}'),
            "no PR badge when zero: {text:?}"
        );
        assert!(
            !text.contains('\u{2709}'),
            "no unread badge when zero: {text:?}"
        );
        assert!(
            !text.contains('\u{26a0}'),
            "no alert badge when zero: {text:?}"
        );
    }

    #[test]
    fn dir_workspace_renders_folder_glyph() {
        use crate::sidebar::RowKind;
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 30,
            rows: 4,
        };
        let mut repo_ws = row(RowKind::Workspace, "repo-ws");
        repo_ws.dir = false;
        let mut dir_ws = row(RowKind::Workspace, "scratch");
        dir_ws.dir = true;
        let model = FrameModel {
            sidebar_rows: vec![repo_ws, dir_ws],
            ..Default::default()
        };
        let mut s = Surface::new(30, 4);
        draw_sidebar(&mut s, rect, &model);
        let text = s.screen_chars_to_string();
        // The dir workspace carries the folder glyph; the repo one does not.
        assert!(text.contains('\u{1f4c1}'), "dir folder glyph 📁: {text:?}");
        assert!(text.contains("scratch") && text.contains("repo-ws"));
    }

    #[test]
    fn clear_frame_removes_stale_cells_from_logical_surface() {
        let mut s = Surface::new(20, 3);
        draw_text(&mut s, 0, 0, "STALE", col(S::Text), col(S::Bg1), 20);
        assert!(s.screen_chars_to_string().contains("STALE"));

        clear_frame(&mut s);
        let text = s.screen_chars_to_string();
        assert!(!text.contains("STALE"), "logical clear removes old cells");
    }
    #[test]
    fn plugin_view_is_host_rendered_with_semantic_roles() {
        use superzej_core::plugin_api::{Span, StyleRole, View};

        let mut s = Surface::new(20, 1);
        let view = View::line([
            Span::styled("ok", StyleRole::Accent),
            Span::styled(" warn", StyleRole::Warning),
        ]);
        draw_plugin_view(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 20,
                rows: 1,
            },
            &view,
            theme::TEAL,
        );

        let text = s.screen_chars_to_string();
        assert!(text.contains("ok warn"), "{text:?}");
    }

    #[test]
    fn center_tabs_show_worktree_label_and_chips() {
        let mut s = Surface::new(80, 2);
        let model = FrameModel {
            worktree: "washu/home".into(),
            tabs: vec!["1".into(), "2".into()],
            active_tab: 1,
            ..Default::default()
        };
        let strip = Rect {
            x: 0,
            y: 1,
            cols: 80,
            rows: 1,
        };
        draw_center_tabs(&mut s, strip, &model);
        let row = &lines(&s)[1];
        // The slug prefix renders uppercased, the leaf in accent, the chips
        // as padded pills after the label.
        assert!(row.contains("WASHU \u{25b8} home"), "{row:?}");
        let leaf_at = row.find(" home").unwrap();
        assert!(row[leaf_at..].contains(" 1 "), "{row:?}");
        assert!(row[leaf_at..].contains(" 2 "), "{row:?}");
        // Hit-test agrees with the rendered chip positions: the spans say
        // where chips draw, and a hit inside the first span returns tab 0.
        let spans = strip_chip_spans(&model, strip);
        assert_eq!(spans.len(), 2);
        assert_eq!(center_tab_hit(&model, strip, spans[0].0), Some(0));
        assert_eq!(center_tab_hit(&model, strip, spans[1].0 + 1), Some(1));
        assert_eq!(center_tab_hit(&model, strip, 0), None);
        // And the drawn cell at the first span really is the chip text.
        let chip0: String = row.chars().skip(spans[0].0).take(spans[0].1).collect();
        assert_eq!(chip0, " 1 ");
    }

    #[test]
    fn center_tabs_render_pin_chips_right_aligned() {
        let mut s = Surface::new(80, 1);
        let model = FrameModel {
            tabs: vec!["1".into()],
            active_tab: 0,
            pins: vec![
                crate::pins::PinChip {
                    index: 1,
                    label: "mail".into(),
                    glyph: crate::pins::PinHealth::Running.glyph(),
                },
                crate::pins::PinChip {
                    index: 2,
                    label: "logs".into(),
                    glyph: crate::pins::PinHealth::Stopped.glyph(),
                },
            ],
            ..Default::default()
        };
        let strip = Rect {
            x: 0,
            y: 0,
            cols: 80,
            rows: 1,
        };
        draw_center_tabs(&mut s, strip, &model);
        let row = &lines(&s)[0];
        assert!(row.contains("mail"), "chip label present: {row:?}");
        assert!(row.contains("logs"));
        let spans = strip_chip_spans(&model, strip);
        assert_eq!(spans.len(), 1, "tab chip still present");
        // The pins are right of the tab chip.
        let mail_at = row.find("mail").unwrap();
        assert!(mail_at > spans[0].0, "pins render to the right of tabs");
    }

    #[test]
    fn stats_cluster_drops_date_then_gpu_when_tight() {
        let parts: Vec<(String, usize)> = [
            ("cpu", 7),
            ("mem", 11),
            ("gpu", 7),
            ("net", 14),
            ("date", 10),
            ("clock", 5),
        ]
        .into_iter()
        .map(|(id, w)| (id.to_string(), w))
        .collect();
        // Plenty of room: everything survives.
        let all = fit_stats_cluster(&parts, 200);
        assert_eq!(all.len(), 6);
        // Tight: date goes first…
        let full = cluster_width(&parts, &all);
        let no_date = fit_stats_cluster(&parts, full - 1);
        assert!(!no_date.iter().any(|&i| parts[i].0 == "date"));
        assert!(no_date.iter().any(|&i| parts[i].0 == "gpu"));
        // …then gpu.
        let tighter = cluster_width(&parts, &no_date);
        let no_gpu = fit_stats_cluster(&parts, tighter - 1);
        assert!(!no_gpu.iter().any(|&i| parts[i].0 == "gpu"));
        assert!(no_gpu.iter().any(|&i| parts[i].0 == "clock"));
    }

    #[test]
    fn masthead_brand_breakpoints() {
        assert_eq!(masthead_brand_cols(160), BRAND_FULL_COLS);
        assert_eq!(masthead_brand_cols(96), BRAND_FULL_COLS);
        assert_eq!(masthead_brand_cols(95), BRAND_COMPACT_COLS);
        assert_eq!(masthead_brand_cols(60), BRAND_COMPACT_COLS);
        assert_eq!(masthead_brand_cols(59), 0);
    }

    #[test]
    fn masthead_stats_use_quiet_separators_and_threshold_colors() {
        let chrome = layout::compute(160, 10, false, false);
        let model = FrameModel {
            stats: superzej_metrics::StatsSnapshot {
                cpu_pct: Some(95),
                mem_gib: Some((10.0, 64.0)),
                ..Default::default()
            },
            bars: superzej_core::config::BarsConfig {
                top_right: vec!["cpu".into(), "mem".into()],
                ..Default::default()
            },
            ..Default::default()
        };
        let mut s = Surface::new(160, 10);
        draw_masthead(&mut s, &chrome, &model);
        let row = &lines(&s)[0];
        assert!(row.contains(" \u{00b7} "), "dot separator: {row:?}");
        assert!(
            !row.contains('\u{2502}'),
            "no heavy bar separators: {row:?}"
        );
        // 95% CPU renders in the critical (red) color.
        let pct_col = row.find("95%").unwrap();
        let pct_chars = row[..pct_col].chars().count();
        let cells = s.screen_cells();
        assert_eq!(
            cells[0][pct_chars].attrs().foreground(),
            theme_color(theme::RED),
            "critical cpu reads in red"
        );
        drop(cells);
        assert_eq!(stat_level(79), Level::Normal);
        assert_eq!(stat_level(80), Level::Warn);
        assert_eq!(stat_level(92), Level::Crit);
        assert_eq!(ratio_level(54.4, 64.0), Level::Warn);
        assert_eq!(ratio_level(10.0, 64.0), Level::Normal);
        assert_eq!(ratio_level(63.0, 64.0), Level::Crit);
        assert_eq!(ratio_level(1.0, 0.0), Level::Normal);
    }

    #[test]
    fn strip_draws_header_label_and_glyph() {
        let mut s = Surface::new(40, 6);
        let strip = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 6,
        };
        let emu = AlacrittyEmulator::new(5, 40, 100);
        let cells = vec![StripCell {
            pane: 1,
            rect: strip,
            label: "syslog".into(),
            glyph: crate::pins::PinHealth::Running.glyph(),
            focused: true,
        }];
        draw_strip(&mut s, strip, &cells, theme::TEAL, |id| {
            (id == 1).then_some(&emu as &dyn PaneEmulator)
        });
        let header = &lines(&s)[0];
        assert!(header.contains("syslog"), "header label: {header:?}");
        assert!(header.contains(crate::pins::PinHealth::Running.glyph()));
    }

    #[test]
    fn center_tab_bar_sits_below_the_divider() {
        let chrome = layout::compute(160, 10, true, true);
        let mut s = Surface::new(160, 10);
        let model = FrameModel {
            worktree: "repo/home".into(),
            tabs: vec!["1".into(), "2".into()],
            active_tab: 0,
            ..Default::default()
        };

        draw_chrome(&mut s, &chrome, &model, &crate::panel::PanelUi::default());

        let brand_cols = masthead_brand_cols(160);
        let l = lines(&s);
        // The masthead carries only brand + stats; the worktree label and
        // chips live on the center tab bar below the divider.
        assert!(
            !l[0].contains("REPO"),
            "masthead carries no nav labels: {:?}",
            l[0]
        );
        let tabs_row = &l[chrome.center_tabs.y];
        assert!(
            tabs_row.contains("REPO \u{25b8} home"),
            "worktree label on the center tab bar: {tabs_row:?}"
        );
        // The divider rule caps the columns above the tab bar.
        assert!(
            l[chrome.divider.y].contains("\u{2500}\u{2500}\u{2500}"),
            "divider rule: {:?}",
            l[chrome.divider.y]
        );
        // The text brand occupies the masthead's brand slot.
        let brand_zone: String = l[0].chars().take(brand_cols).collect();
        assert!(
            brand_zone.contains("superzej"),
            "text brand on the masthead: {:?}",
            l[0]
        );
    }

    #[test]
    fn full_frame_tab_chip_lands_on_the_center_tab_bar() {
        let cols = 160usize;
        let rows = 10usize;
        let chrome = layout::compute(cols, rows, true, true);
        let mut emu =
            AlacrittyEmulator::new(chrome.center.rows as u16, chrome.center.cols as u16, 0);
        emu.advance(b"CENTER");
        let model = FrameModel {
            worktree: "repo/home".into(),
            tabs: vec!["1".into()],
            active_tab: 0,
            ..Default::default()
        };
        let center = crate::center::CenterTree::Leaf(1);
        let mut s = Surface::new(cols, rows);

        render_tab(
            &mut s,
            &chrome,
            &center,
            1,
            &model,
            &crate::panel::PanelUi::default(),
            |id| (id == 1).then_some(&emu as &dyn PaneEmulator),
            &|_| String::new(),
            &|_| None,
        );

        let l = lines(&s);
        let spans = strip_chip_spans(&model, chrome.center_tabs);
        assert_eq!(spans.len(), 1);
        let tabs_row = &l[chrome.center_tabs.y];
        let chip: String = tabs_row.chars().skip(spans[0].0).take(spans[0].1).collect();
        assert_eq!(chip, " 1 ", "tab chip on the center tab bar: {tabs_row:?}");
        assert!(
            tabs_row.contains("REPO \u{25b8} home"),
            "worktree label beside the chips: {tabs_row:?}"
        );
    }

    #[test]
    fn render_tab_paints_every_visible_pane() {
        use crate::center::{Branch, CenterTree, Dir};
        let cols = 160usize;
        let rows = 40usize;
        let chrome = layout::compute(cols, rows, false, false); // full-width center

        // Two side-by-side panes (ids 1 and 2).
        let center = CenterTree::Split {
            dir: Dir::Row,
            children: vec![
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(1),
                },
                Branch {
                    weight: 1.0,
                    child: CenterTree::Leaf(2),
                },
            ],
        };
        let half = (chrome.center.cols / 2) as u16;
        let mut left = AlacrittyEmulator::new(chrome.center.rows as u16, half, 0);
        left.advance(b"LEFTPANE");
        let mut right = AlacrittyEmulator::new(chrome.center.rows as u16, half, 0);
        right.advance(b"RIGHTPANE");

        let model = FrameModel {
            tabs: vec!["repo/home".into()],
            ..Default::default()
        };
        let mut s = Surface::new(cols, rows);
        render_tab(
            &mut s,
            &chrome,
            &center,
            1,
            &model,
            &crate::panel::PanelUi::default(),
            |id| match id {
                1 => Some(&left as &dyn PaneEmulator),
                2 => Some(&right as &dyn PaneEmulator),
                _ => None,
            },
            &|id| format!("pane-{id}"),
            &|_| None,
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("LEFTPANE"), "left pane painted");
        assert!(text.contains("RIGHTPANE"), "right pane painted");
        // Card titles ride the top border of each pane frame.
        assert!(text.contains(" pane-1 "), "embedded card title: {text:?}");
        assert!(text.contains(" pane-2 "));
    }

    #[test]
    fn render_tab_shows_splash_when_no_live_panes() {
        let cols = 160usize;
        let rows = 40usize;
        let chrome = layout::compute(cols, rows, true, true);
        let model = FrameModel {
            worktree: "repo/home".into(),
            tabs: vec!["1".into()],
            ..Default::default()
        };
        let center = crate::center::CenterTree::Leaf(1);
        let mut s = Surface::new(cols, rows);
        render_tab(
            &mut s,
            &chrome,
            &center,
            1,
            &model,
            &crate::panel::PanelUi::default(),
            |_| None,
            &|_| String::new(),
            &|_| None,
        );
        let l = lines(&s);
        // The splash wordmark lands inside the center band, with chrome intact.
        let mid = &l[chrome.center.y + chrome.center.rows / 2 - 1];
        let band: String = l[chrome.center.y..chrome.center.y + chrome.center.rows]
            .iter()
            .map(|r| {
                r.chars()
                    .skip(chrome.center.x)
                    .take(chrome.center.cols)
                    .collect::<String>()
            })
            .collect();
        assert!(
            band.contains("Ctrl-Space"),
            "splash hints in center: {mid:?}"
        );
        assert!(band.chars().any(|c| "▀▄█".contains(c)), "splash wordmark");
        assert!(l.join("\n").contains("WORKSPACES"), "chrome still drawn");
        // No card rings drawn around the empty center.
        assert!(!band.contains('\u{256d}'), "no pane card on empty center");
    }

    #[test]
    fn full_frame_places_chrome_and_center_pane() {
        let cols = 160usize;
        let rows = 40usize;
        let chrome = layout::compute(cols, rows, true, true);

        let mut emu =
            AlacrittyEmulator::new(chrome.center.rows as u16, chrome.center.cols as u16, 0);
        emu.advance(b"CENTER-CONTENT");

        let model = FrameModel {
            tabs: vec!["repo/home".into()],
            active_tab: 0,
            sidebar_rows: vec![
                row(crate::sidebar::RowKind::Workspace, "repo"),
                row(crate::sidebar::RowKind::Worktree, "feat"),
            ],
            panel: crate::panel::PanelData {
                branch: "feat".into(),
                pr: Some(crate::panel::PrSummary {
                    number: 42,
                    title: "a pr".into(),
                    state: "OPEN".into(),
                    url: "https://example/42".into(),
                    is_draft: false,
                    review_decision: None,
                }),
                ..Default::default()
            },
            status: "Cmd-K  Alt-w new  Alt-o switch".into(),
            bars: superzej_core::config::BarsConfig {
                bottom_left: vec!["status".into()],
                ..Default::default()
            },
            ..Default::default()
        };

        let mut s = Surface::new(cols, rows);
        let center = crate::center::CenterTree::Leaf(1);
        // Pr section open (Work tab) so the #42 PR summary is on screen.
        let panel_ui = crate::panel::PanelUi {
            tab: crate::panel::PanelTab::Work,
            open: crate::panel::Section::Pr,
            ..Default::default()
        };
        render_tab(
            &mut s,
            &chrome,
            &center,
            1,
            &model,
            &panel_ui,
            |id| (id == 1).then_some(&emu as &dyn PaneEmulator),
            &|_| String::new(),
            &|_| None,
        );
        let l = lines(&s);

        // Masthead: the text brand on row 0; the tab chip rides the center
        // tab bar; the accordion sections fill the panel column; the
        // statusbar (last row) carries the status widget.
        assert!(l[0].contains("superzej"), "brand: {:?}", l[0]);
        let tabs_row = &l[chrome.center_tabs.y];
        assert!(
            tabs_row.contains("repo/home") || tabs_row.contains(" repo/home "),
            "tab chip on the center tab bar: {tabs_row:?}"
        );
        let panel_rect = chrome.panel.unwrap();
        let panel_col: String = l
            .iter()
            .map(|row| {
                row.chars()
                    .skip(panel_rect.x)
                    .take(panel_rect.cols)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Default tab = Work (Pr section open with #42 PR data); tab bar is visible.
        assert!(
            panel_col.contains("git") && panel_col.contains("work") && panel_col.contains("system"),
            "tab bar in panel column: {panel_col:?}"
        );
        // Work tab sections are visible.
        assert!(
            panel_col.contains("pr") && panel_col.contains("issues"),
            "accordion sections fill the panel column: {panel_col:?}"
        );
        assert!(l[rows - 1].contains("Cmd-K"), "status: {:?}", l[rows - 1]);
        // Sidebar title and center content all present.
        let all = l.join("\n");
        assert!(all.contains("WORKSPACES"));
        assert!(all.contains("CENTER-CONTENT"));
        assert!(all.contains("#42"));
    }

    #[test]
    fn statusbar_keyhints_stop_at_last_whole_binding() {
        // Three bindings; "a alpha" (7) + "   b bravo" (10) overflow a tight bar.
        let model = FrameModel {
            keyhints: vec![
                ("a".into(), "alpha".into()),
                ("b".into(), "bravo".into()),
                ("c".into(), "charlie".into()),
            ],
            bars: superzej_core::config::BarsConfig {
                bottom_left: vec!["keyhints".into()],
                bottom_right: vec![],
                ..Default::default()
            },
            ..Default::default()
        };
        // Budget = 16 - 2 = 14: " " (1) + "a alpha" (7) fits at 8; the next
        // binding would land at 18, so it is dropped whole.
        let mut s = Surface::new(16, 1);
        draw_statusbar(
            &mut s,
            Rect {
                x: 0,
                y: 0,
                cols: 16,
                rows: 1,
            },
            &model,
        );
        let text = s.screen_chars_to_string();
        assert!(text.contains("alpha"), "first binding shown: {text:?}");
        // No mid-binding cut: the dropped binding is fully absent, not "b…".
        assert!(
            !text.contains('\u{2026}'),
            "no ellipsis truncation: {text:?}"
        );
        assert!(
            !text.contains("bravo"),
            "overflowing binding dropped: {text:?}"
        );
        assert!(
            !text.contains("charlie"),
            "overflowing binding dropped: {text:?}"
        );

        // With ample width every binding is present, untouched.
        let mut wide = Surface::new(60, 1);
        draw_statusbar(
            &mut wide,
            Rect {
                x: 0,
                y: 0,
                cols: 60,
                rows: 1,
            },
            &model,
        );
        let wtext = wide.screen_chars_to_string();
        assert!(
            wtext.contains("alpha") && wtext.contains("bravo") && wtext.contains("charlie"),
            "all bindings fit when wide: {wtext:?}"
        );
    }

    #[test]
    fn statusbar_tests_widget_renders_pass_fail_rollup() {
        use crate::panel::{PanelData, TestsLite};
        let bars = || superzej_core::config::BarsConfig {
            bottom_left: vec![],
            bottom_right: vec!["tests".into()],
            ..Default::default()
        };
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 40,
            rows: 1,
        };

        // All-pass run: only the ✓ count, no ✗.
        let pass = FrameModel {
            panel: PanelData {
                tests: Some(TestsLite {
                    passed: 12,
                    ..Default::default()
                }),
                ..Default::default()
            },
            bars: bars(),
            ..Default::default()
        };
        let mut s = Surface::new(40, 1);
        draw_statusbar(&mut s, rect, &pass);
        let text = s.screen_chars_to_string();
        assert!(text.contains("\u{2713}12"), "pass count shown: {text:?}");
        assert!(
            !text.contains('\u{2717}'),
            "no fail glyph all-pass: {text:?}"
        );

        // Mixed run: ✓ and ✗ both shown.
        let mixed = FrameModel {
            panel: PanelData {
                tests: Some(TestsLite {
                    passed: 8,
                    failed: 3,
                    ..Default::default()
                }),
                ..Default::default()
            },
            bars: bars(),
            ..Default::default()
        };
        let mut s2 = Surface::new(40, 1);
        draw_statusbar(&mut s2, rect, &mixed);
        let t2 = s2.screen_chars_to_string();
        assert!(
            t2.contains("\u{2713}8") && t2.contains("\u{2717}3"),
            "pass+fail counts shown: {t2:?}"
        );

        // No run yet (default counts) → widget hidden.
        let empty = FrameModel {
            panel: PanelData {
                tests: Some(TestsLite::default()),
                ..Default::default()
            },
            bars: bars(),
            ..Default::default()
        };
        let mut s3 = Surface::new(40, 1);
        draw_statusbar(&mut s3, rect, &empty);
        let t3 = s3.screen_chars_to_string();
        assert!(
            !t3.contains('\u{2713}') && !t3.contains('\u{2717}'),
            "widget hidden with no counts: {t3:?}"
        );
    }

    /// A minimal panel model with one unstaged change.
    fn panel_model() -> FrameModel {
        use crate::panel::{ChangeRow, PanelData, Stage};
        FrameModel {
            panel: PanelData {
                branch: "feat".into(),
                changes: vec![ChangeRow {
                    status: "M".into(),
                    stage: Stage::Unstaged,
                    dir: "src/".into(),
                    name: "main.rs".into(),
                    path: "src/main.rs".into(),
                    added: 3,
                    deleted: 1,
                }],
                ..Default::default()
            },
            panel_focused: true,
            ..Default::default()
        }
    }

    #[test]
    fn panel_renders_accordion_sections_and_open_content() {
        use crate::panel::PanelUi;
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 44,
            rows: 30,
        };
        let model = panel_model();
        let ui = PanelUi::default(); // tab = Git
        let mut s = Surface::new(44, 30);
        draw_panel(&mut s, rect, &model, &ui);
        let text = s.screen_chars_to_string();
        // Active-tab sections (Git: changes, commits, branches, stash, files)
        // are on screen; other-tab sections are hidden.
        for sec in ui.tab_sections() {
            assert!(
                text.contains(sec.label()),
                "{} missing: {text:?}",
                sec.label()
            );
        }
        // Tab bar labels are always visible.
        assert!(text.contains("git"), "tab bar: {text:?}");
        assert!(text.contains("work"), "tab bar: {text:?}");
        assert!(text.contains("system"), "tab bar: {text:?}");
        assert!(text.contains("feat"), "branch header: {text:?}");
        assert!(text.contains("main.rs"), "open section content: {text:?}");
        // Help hints moved to the bottom bar: section mode offers the open
        // affordance (Enter to drill into rows), row mode the section's actions.
        assert!(
            panel_help_pairs(&PanelUi::default())
                .iter()
                .any(|(_, l)| l == "open")
        );
        let row_mode = PanelUi {
            row_mode: true,
            ..Default::default()
        };
        assert!(
            panel_help_pairs(&row_mode)
                .iter()
                .any(|(_, l)| l == "stage")
        );
        // During an active merge conflict the flow hint leads and replaces the
        // generic "m flow menu" entry in the table.
        let merge_flow = PanelUi {
            row_mode: true,
            git: crate::panel::gitui::GitUi {
                flow: crate::panel::gitui::GitFlow::Merge(crate::panel::gitui::SequencerUi {
                    onto: "main".to_string(),
                    conflict: true,
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let mf_pairs = panel_help_pairs(&merge_flow);
        assert_eq!(
            mf_pairs[0],
            ("m".to_string(), "merge continue/abort".to_string())
        );
        // The generic "m flow menu" entry is suppressed (deduplicated by chord).
        assert!(!mf_pairs.iter().any(|(_, l)| l == "flow menu"));
        // Degenerate rects never panic or paint.
        let mut tiny = Surface::new(44, 30);
        draw_panel(
            &mut tiny,
            Rect {
                x: 0,
                y: 0,
                cols: 0,
                rows: 0,
            },
            &model,
            &PanelUi::default(),
        );
    }

    #[test]
    fn panel_hits_expose_all_sections_at_distinct_rows() {
        use crate::panel::{PanelHit, PanelUi};
        let rect = Rect {
            x: 0,
            y: 3,
            cols: 44,
            rows: 30,
        };
        let model = panel_model();
        let hits = panel_hits(&model, &PanelUi::default(), rect);
        let section_rows: Vec<usize> = hits
            .iter()
            .filter(|(_, h)| matches!(h, PanelHit::OpenSection(_)))
            .map(|(y, _)| *y)
            .collect();
        // Default tab = Git → 5 sections shown (Changes, Commits, Branches, Stash, Files).
        let default_ui = PanelUi::default();
        assert_eq!(
            section_rows.len(),
            default_ui.tab_sections().len(),
            "hits: {hits:?}"
        );
        let mut dedup = section_rows.clone();
        dedup.dedup();
        assert_eq!(dedup, section_rows, "section rows are distinct + ordered");
        for y in &section_rows {
            assert!(*y >= rect.y && *y < rect.y + rect.rows, "y in rect: {y}");
        }
    }

    #[test]
    fn checks_render_inside_the_open_git_section() {
        use crate::panel::{CheckLine, CheckState, PanelUi, PrSummary, Section};
        let rect = Rect {
            x: 0,
            y: 0,
            cols: 44,
            rows: 30,
        };
        let mut model = panel_model();
        model.panel.pr = Some(PrSummary {
            number: 42,
            title: "a pr".into(),
            state: "OPEN".into(),
            url: "https://example/42".into(),
            is_draft: false,
            review_decision: None,
        });
        model.panel.checks = vec![
            CheckLine {
                name: "build".into(),
                state: CheckState::Pass,
                duration_secs: None,
                details_url: None,
            },
            CheckLine {
                name: "lint".into(),
                state: CheckState::Fail,
                duration_secs: None,
                details_url: None,
            },
        ];
        let ui = PanelUi {
            tab: crate::panel::PanelTab::Work,
            open: Section::Pr,
            ..Default::default()
        };
        let mut s = Surface::new(44, 30);
        draw_panel(&mut s, rect, &model, &ui);
        let text = s.screen_chars_to_string();
        assert!(text.contains("CHECKS"), "{text:?}");
        assert!(text.contains("build"), "{text:?}");
        assert!(text.contains("lint"), "{text:?}");
        assert!(text.contains("#42"), "{text:?}");
    }
}
